use codex_exec_server::GetMetadataOptions;
use codex_exec_server::ReadFileOptions;
use codex_protocol::items::ImageViewItem;
use codex_protocol::items::TurnItem;
use codex_protocol::models::DEFAULT_IMAGE_DETAIL;
use codex_protocol::models::FunctionCallOutputBody;
use codex_protocol::models::FunctionCallOutputContentItem;
use codex_protocol::models::FunctionCallOutputPayload;
use codex_protocol::models::ImageDetail;
use codex_protocol::models::ResponseInputItem;
use codex_protocol::openai_models::InputModality;
use codex_utils_image::PromptImageMode;
use codex_utils_image::data_url_from_bytes;
use serde::Deserialize;

use crate::function_tool::FunctionCallError;
use crate::original_image_detail::can_request_original_image_detail;
use crate::tools::context::FunctionToolOutput;
use crate::tools::context::ToolInvocation;
use crate::tools::context::ToolOutput;
use crate::tools::context::ToolPayload;
use crate::tools::context::boxed_tool_output;
use crate::tools::handlers::parse_arguments;
use crate::tools::handlers::resolve_tool_environment;
use crate::tools::handlers::view_image_spec::ViewImageToolOptions;
use crate::tools::handlers::view_image_spec::create_view_image_tool;
use crate::tools::registry::CoreToolRuntime;
use crate::tools::registry::ToolExecutor;
use crate::tools::router::ToolCallSource;
use codex_tools::ToolName;
use codex_tools::ToolSpec;

pub struct ViewImageHandler {
    options: ViewImageToolOptions,
}

impl Default for ViewImageHandler {
    fn default() -> Self {
        Self {
            options: ViewImageToolOptions {
                can_request_original_image_detail: false,
                unified_image_budget: false,
                include_environment_id: false,
                delegate: false,
            },
        }
    }
}

impl ViewImageHandler {
    pub(crate) fn new(options: ViewImageToolOptions) -> Self {
        Self { options }
    }
}

const VIEW_IMAGE_UNSUPPORTED_MESSAGE: &str =
    "view_image is not allowed because you do not support image inputs";
const VIEW_IMAGE_INVALID_MESSAGE: &str =
    "unable to process image: invalid or unsupported image data";
/// FORK: `raw` bypasses the reader model, so it needs the main model's own eyes.
const VIEW_IMAGE_RAW_UNSUPPORTED_MESSAGE: &str = "view_image raw=true is not allowed because you do not support image inputs; omit `raw` to get the reader model's description instead";
/// FORK: `question` only exists because a reader model answers it.
const VIEW_IMAGE_QUESTION_UNSUPPORTED_MESSAGE: &str = "view_image.question requires `[tools.view_image] delegate = true`; omit `question` to load the image itself";

#[derive(Deserialize)]
struct ViewImageArgs {
    path: String,
    #[serde(default)]
    environment_id: Option<String>,
    detail: Option<String>,
    /// FORK: ask the reader model about the image instead of loading it.
    #[serde(default)]
    question: Option<String>,
    /// FORK: keep the pixels in context instead of the reader's description.
    #[serde(default)]
    raw: bool,
}

#[derive(Clone, Copy, Eq, PartialEq)]
enum ViewImageDetail {
    High,
    Original,
}

impl ToolExecutor<ToolInvocation> for ViewImageHandler {
    fn tool_name(&self) -> ToolName {
        ToolName::plain("view_image")
    }

    fn spec(&self) -> ToolSpec {
        create_view_image_tool(self.options)
    }

    fn supports_parallel_tool_calls(&self) -> bool {
        true
    }

    fn handle<'a>(&'a self, invocation: ToolInvocation) -> codex_tools::ToolExecutorFuture<'a>
    where
        ToolInvocation: 'a,
    {
        Box::pin(self.handle_call(invocation))
    }
}

impl ViewImageHandler {
    async fn handle_call(
        &self,
        invocation: ToolInvocation,
    ) -> Result<Box<dyn crate::tools::context::ToolOutput>, FunctionCallError> {
        let ToolInvocation {
            session,
            turn,
            step_context,
            payload,
            call_id,
            source,
            ..
        } = invocation;

        let arguments = match payload {
            ToolPayload::Function { arguments } => arguments,
            _ => {
                return Err(FunctionCallError::RespondToModel(
                    "view_image handler received unsupported payload".to_string(),
                ));
            }
        };

        let ViewImageArgs {
            path,
            environment_id,
            detail,
            question,
            raw,
        } = parse_arguments(&arguments)?;
        // FORK: with delegation on, the pixels only have to reach the reader
        // model, so `view_image` also works on a text-only main model.
        let delegation = turn.config.tools_view_image.clone();
        let main_supports_images = turn
            .model_info()
            .input_modalities
            .contains(&InputModality::Image);
        if question.is_some() && delegation.is_none() {
            return Err(FunctionCallError::RespondToModel(
                VIEW_IMAGE_QUESTION_UNSUPPORTED_MESSAGE.to_string(),
            ));
        }
        if question.is_some() && raw {
            return Err(FunctionCallError::RespondToModel(
                "view_image cannot take `question` and `raw` together: `question` returns the reader model's answer as text, `raw` returns the image itself".to_string(),
            ));
        }
        if raw && !main_supports_images {
            // `raw` puts the pixels in the main model's context, so it is the
            // main model that has to be able to read them.
            return Err(FunctionCallError::RespondToModel(
                VIEW_IMAGE_RAW_UNSUPPORTED_MESSAGE.to_string(),
            ));
        }
        match delegation.as_ref() {
            Some(reader) if !raw => {
                let reader_info = session
                    .services
                    .models_manager
                    .get_model_info(&reader.model, &turn.config.to_models_manager_config())
                    .await;
                if !reader_info.input_modalities.contains(&InputModality::Image) {
                    return Err(FunctionCallError::RespondToModel(format!(
                        "view_image is not allowed because the configured image reader `{}` does not support image inputs",
                        reader.model
                    )));
                }
            }
            _ if !main_supports_images => {
                return Err(FunctionCallError::RespondToModel(
                    VIEW_IMAGE_UNSUPPORTED_MESSAGE.to_string(),
                ));
            }
            _ => {}
        }
        // Keep accepting previously supported detail hints after they disappear from the schema.
        let detail = match detail.as_deref() {
            None => None,
            Some("high") => Some(ViewImageDetail::High),
            Some("original") => Some(ViewImageDetail::Original),
            Some(detail) => {
                return Err(FunctionCallError::RespondToModel(format!(
                    "view_image.detail only supports `high` or `original`; omit `detail` for default high resized behavior, got `{detail}`"
                )));
            }
        };

        let Some(turn_environment) =
            resolve_tool_environment(&step_context.environments, environment_id.as_deref())?
        else {
            return Err(FunctionCallError::RespondToModel(
                "view_image is unavailable in this session".to_string(),
            ));
        };
        let path_uri = turn_environment.cwd().join(&path).map_err(|err| {
            FunctionCallError::RespondToModel(format!(
                "unable to resolve image path `{path}` against environment cwd `{}`: {err}",
                turn_environment.cwd(),
            ))
        })?;
        let model_visible_path = path_uri.inferred_native_path_string();
        let sandbox = turn_environment.sandbox_context(/*additional_permissions*/ None);
        let fs = turn_environment.environment.get_filesystem();

        let metadata = fs
            .get_metadata(&path_uri, GetMetadataOptions::default(), Some(&sandbox))
            .await
            .map_err(|error| {
                FunctionCallError::RespondToModel(format!(
                    "unable to locate image at `{model_visible_path}`: {error}"
                ))
            })?;

        if !metadata.is_file {
            return Err(FunctionCallError::RespondToModel(format!(
                "image path `{model_visible_path}` is not a file"
            )));
        }
        let file_bytes = fs
            .read_file(&path_uri, ReadFileOptions::default(), Some(&sandbox))
            .await
            .map_err(|error| {
                FunctionCallError::RespondToModel(format!(
                    "unable to read image at `{model_visible_path}`: {error}"
                ))
            })?;
        // Reject non-images before their bytes can reach code mode without changing
        // valid image bytes, metadata, or centralized image preparation.
        image::load_from_memory(&file_bytes).map_err(|_| {
            FunctionCallError::RespondToModel(VIEW_IMAGE_INVALID_MESSAGE.to_string())
        })?;

        let can_request_original_detail =
            can_request_original_image_detail(&step_context.settings.model_info);
        let use_original_detail = self.options.unified_image_budget
            || can_request_original_detail && matches!(detail, Some(ViewImageDetail::Original));
        let image_detail = if use_original_detail {
            ImageDetail::Original
        } else {
            DEFAULT_IMAGE_DETAIL
        };

        // The history insertion path owns image preparation and resizing.
        let image_url = data_url_from_bytes("application/octet-stream", &file_bytes);

        let item = TurnItem::ImageView(ImageViewItem {
            id: call_id,
            path: path_uri,
        });
        session.emit_turn_item_started(turn.as_ref(), &item).await;
        session.emit_turn_item_completed(turn.as_ref(), item).await;

        // FORK: a question is answered by the reader model, so the tool result
        // is text and the pixels never enter the main model's context.
        if let (Some(reader), Some(question)) = (delegation.as_ref(), question) {
            // The reader gets the same resized bytes the history path would
            // have produced, not the raw file.
            let prepared_image_url = codex_utils_image::load_data_url_for_prompt(
                &image_url,
                PromptImageMode::HIGH_DETAIL,
            )
            .map(codex_utils_image::EncodedImage::into_data_url)
            .unwrap_or_else(|_| image_url.clone());
            let answer = crate::image_reader::read_image(
                session.as_ref(),
                turn.as_ref(),
                reader,
                crate::image_reader::ImageReadRequest {
                    image_url: prepared_image_url,
                    detail: None,
                    question: Some(question),
                },
            )
            .await;
            return match answer.text {
                Some(text) => Ok(boxed_tool_output(ViewImageTextOutput { text })),
                None => Err(FunctionCallError::RespondToModel(format!(
                    "the image reader `{}` failed to answer; call view_image again without `question` to load the image itself",
                    reader.model
                ))),
            };
        }

        // FORK: in code mode the image reaches history through the cell's own
        // output, so the pin has to travel with the cell instead of this result.
        // Without delegation the pixels already reach the model unchanged, so a
        // pin would only add metadata nothing reads.
        let pins_raw_image = raw && delegation.is_some();
        if pins_raw_image && let ToolCallSource::CodeMode { cell_id, .. } = &source {
            session
                .services
                .code_mode_service
                .pin_raw_image_cell(codex_code_mode::CellId::new(cell_id.clone()));
        }

        Ok(boxed_tool_output(ViewImageOutput {
            image_url,
            image_detail,
            unified_image_budget: self.options.unified_image_budget,
            pins_raw_image,
        }))
    }
}

impl CoreToolRuntime for ViewImageHandler {
    fn is_builtin_control_tool(&self) -> bool {
        true
    }
}

pub struct ViewImageOutput {
    image_url: String,
    image_detail: ImageDetail,
    unified_image_budget: bool,
    /// FORK: the model asked for the pixels, so history keeps them.
    pins_raw_image: bool,
}

impl ToolOutput for ViewImageOutput {
    fn log_output(&self) -> String {
        format!("<image data URL omitted: {} bytes>", self.image_url.len())
    }

    fn success_for_logging(&self) -> bool {
        true
    }

    fn pins_raw_image(&self) -> bool {
        self.pins_raw_image
    }

    fn to_response_item(&self, call_id: &str, _payload: &ToolPayload) -> ResponseInputItem {
        let body =
            FunctionCallOutputBody::ContentItems(vec![FunctionCallOutputContentItem::InputImage {
                image_url: self.image_url.clone(),
                detail: Some(self.image_detail),
            }]);
        let output = FunctionCallOutputPayload {
            body,
            success: Some(true),
        };

        ResponseInputItem::FunctionCallOutput {
            call_id: call_id.to_string(),
            output,
        }
    }

    fn code_mode_result(&self, _payload: &ToolPayload) -> serde_json::Value {
        if self.unified_image_budget {
            serde_json::json!({ "image_url": self.image_url })
        } else {
            serde_json::json!({
                "image_url": self.image_url,
                "detail": self.image_detail
            })
        }
    }
}

/// FORK: the reader model's answer to a `view_image` question.
pub struct ViewImageTextOutput {
    text: String,
}

impl ToolOutput for ViewImageTextOutput {
    fn log_output(&self) -> String {
        self.text.clone()
    }

    fn success_for_logging(&self) -> bool {
        true
    }

    fn to_response_item(&self, call_id: &str, payload: &ToolPayload) -> ResponseInputItem {
        // Reuse the shared shape so the answer looks like any other text tool
        // output on the wire.
        FunctionToolOutput::from_text(self.text.clone(), Some(true))
            .to_response_item(call_id, payload)
    }

    fn code_mode_result(&self, _payload: &ToolPayload) -> serde_json::Value {
        serde_json::json!({ "text": self.text })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::PermissionProfileSnapshot;
    use crate::environment_selection::TurnEnvironmentState;
    use crate::session::step_context::StepContext;
    use crate::session::tests::make_session_and_context;
    use crate::session::turn_context::TurnEnvironment;
    use crate::tools::context::ToolCallSource;
    use crate::tools::context::ToolInvocation;
    use crate::turn_diff_tracker::TurnDiffTracker;
    use codex_protocol::models::PermissionProfile;
    use codex_utils_absolute_path::AbsolutePathBuf;
    use codex_utils_path_uri::PathUri;
    use core_test_support::TempDirExt;
    use image::ImageBuffer;
    use image::ImageFormat;
    use image::Rgba;
    use pretty_assertions::assert_eq;
    use serde_json::json;
    use std::io::Cursor;
    use std::sync::Arc;
    use tokio::sync::Mutex;

    fn replace_primary_environment_cwd(turn: &mut crate::TurnContext, cwd: AbsolutePathBuf) {
        let mut current = turn
            .environments
            .turn_environments()
            .next()
            .cloned()
            .expect("default local turn environment");
        current.config_mut().workspace_roots.clear();
        let mut selection = current.selection;
        selection.cwd = PathUri::from_abs_path(&cwd);
        selection.workspace_roots.clear();
        turn.environments.environments[0] = TurnEnvironmentState::Ready(TurnEnvironment::new(
            selection,
            current.config_origin,
            current.environment,
            current.shell,
        ));
    }

    fn tiny_png() -> Vec<u8> {
        let image = ImageBuffer::from_pixel(
            /*width*/ 1,
            /*height*/ 1,
            Rgba([255u8, 0, 0, 255]),
        );
        let mut bytes = Vec::new();
        image
            .write_to(&mut Cursor::new(&mut bytes), ImageFormat::Png)
            .expect("encode test image");
        bytes
    }

    #[test]
    fn log_preview_omits_image_data() {
        let output = ViewImageOutput {
            image_url: "data:image/png;base64,AAA".to_string(),
            image_detail: DEFAULT_IMAGE_DETAIL,
            unified_image_budget: false,
            pins_raw_image: false,
        };

        assert_eq!(output.log_output(), "<image data URL omitted: 25 bytes>");
    }

    #[test]
    fn code_mode_result_returns_image_url_object() {
        let output = ViewImageOutput {
            image_url: "data:image/png;base64,AAA".to_string(),
            image_detail: DEFAULT_IMAGE_DETAIL,
            unified_image_budget: false,
            pins_raw_image: false,
        };

        let result = output.code_mode_result(&ToolPayload::Function {
            arguments: "{}".to_string(),
        });

        assert_eq!(
            result,
            json!({
                "image_url": "data:image/png;base64,AAA",
                "detail": "high",
            })
        );
    }

    #[tokio::test]
    async fn handle_passes_sandbox_context_for_local_filesystem_reads() {
        let (session, mut turn) = make_session_and_context().await;
        let image_dir = tempfile::tempdir().expect("create image temp dir");
        let image_cwd = image_dir.abs();

        replace_primary_environment_cwd(&mut turn, image_cwd.clone());
        let image_path = image_cwd.join("image.png");
        std::fs::write(image_path.as_path(), tiny_png()).expect("write test image");
        Arc::make_mut(&mut turn.config)
            .permissions
            .set_permission_profile(PermissionProfile::Disabled)
            .expect("set thread permission profile");
        let TurnEnvironmentState::Ready(environment) = &mut turn.environments.environments[0]
        else {
            panic!("primary environment should be ready");
        };
        environment.config_mut().permission_profile =
            PermissionProfileSnapshot::legacy(PermissionProfile::read_only());
        let turn = Arc::new(turn);

        let result = ViewImageHandler::default()
            .handle(ToolInvocation {
                session: Arc::new(session),
                step_context: StepContext::for_test(Arc::clone(&turn)),
                turn,
                cancellation_token: tokio_util::sync::CancellationToken::new(),
                tracker: Arc::new(Mutex::new(TurnDiffTracker::new())),
                call_id: "call-view-image".to_string(),
                tool_name: codex_tools::ToolName::plain("view_image"),
                source: ToolCallSource::Direct,
                payload: ToolPayload::Function {
                    arguments: json!({ "path": "image.png" }).to_string(),
                },
            })
            .await;

        let Err(FunctionCallError::RespondToModel(message)) = result else {
            panic!("expected sandboxed filesystem error");
        };
        assert!(
            message.contains("sandboxed filesystem operations require configured runtime paths"),
            "{message}"
        );
    }

    #[tokio::test]
    async fn handle_rejects_unsupported_detail() {
        let (session, turn) = make_session_and_context().await;
        let turn = Arc::new(turn);

        let result = ViewImageHandler::default()
            .handle(ToolInvocation {
                session: Arc::new(session),
                step_context: StepContext::for_test(Arc::clone(&turn)),
                turn,
                cancellation_token: tokio_util::sync::CancellationToken::new(),
                tracker: Arc::new(Mutex::new(TurnDiffTracker::new())),
                call_id: "call-view-image".to_string(),
                tool_name: codex_tools::ToolName::plain("view_image"),
                source: ToolCallSource::Direct,
                payload: ToolPayload::Function {
                    arguments: json!({ "path": "image.png", "detail": "low" }).to_string(),
                },
            })
            .await;

        let Err(FunctionCallError::RespondToModel(message)) = result else {
            panic!("expected unsupported detail error");
        };
        assert_eq!(
            message,
            "view_image.detail only supports `high` or `original`; omit `detail` for default high resized behavior, got `low`"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn handle_accepts_explicit_high_detail() {
        let (session, mut turn) = make_session_and_context().await;
        let image_dir = tempfile::tempdir().expect("create image temp dir");
        let image_cwd = image_dir.abs();

        replace_primary_environment_cwd(&mut turn, image_cwd.clone());
        let image_path = image_cwd.join("image.png");
        std::fs::write(image_path.as_path(), tiny_png()).expect("write test image");
        let TurnEnvironmentState::Ready(environment) = &mut turn.environments.environments[0]
        else {
            panic!("primary environment should be ready");
        };
        environment.config_mut().permission_profile =
            PermissionProfileSnapshot::legacy(PermissionProfile::Disabled);
        let turn = Arc::new(turn);

        let result = ViewImageHandler::default()
            .handle(ToolInvocation {
                session: Arc::new(session),
                step_context: StepContext::for_test(Arc::clone(&turn)),
                turn,
                cancellation_token: tokio_util::sync::CancellationToken::new(),
                tracker: Arc::new(Mutex::new(TurnDiffTracker::new())),
                call_id: "call-view-image".to_string(),
                tool_name: codex_tools::ToolName::plain("view_image"),
                source: ToolCallSource::Direct,
                payload: ToolPayload::Function {
                    arguments: json!({ "path": "image.png", "detail": "high" }).to_string(),
                },
            })
            .await;

        result.expect("explicit high detail should be accepted");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn handle_rejects_invalid_image_before_returning_output_to_code_mode() {
        let (session, mut turn) = make_session_and_context().await;
        let image_dir = tempfile::tempdir().expect("create image temp dir");
        let image_cwd = image_dir.abs();

        replace_primary_environment_cwd(&mut turn, image_cwd.clone());
        let image_path = image_cwd.join("not-an-image.txt");
        std::fs::write(image_path.as_path(), b"arbitrary file contents")
            .expect("write invalid image");
        let TurnEnvironmentState::Ready(environment) = &mut turn.environments.environments[0]
        else {
            panic!("primary environment should be ready");
        };
        environment.config_mut().permission_profile =
            PermissionProfileSnapshot::legacy(PermissionProfile::Disabled);
        let turn = Arc::new(turn);

        let result = ViewImageHandler::default()
            .handle(ToolInvocation {
                session: Arc::new(session),
                step_context: StepContext::for_test(Arc::clone(&turn)),
                turn,
                cancellation_token: tokio_util::sync::CancellationToken::new(),
                tracker: Arc::new(Mutex::new(TurnDiffTracker::new())),
                call_id: "call-view-image".to_string(),
                tool_name: codex_tools::ToolName::plain("view_image"),
                source: ToolCallSource::CodeMode {
                    cell_id: "cell-1".to_string(),
                    runtime_tool_call_id: "tool-1".to_string(),
                },
                payload: ToolPayload::Function {
                    arguments: json!({ "path": "not-an-image.txt" }).to_string(),
                },
            })
            .await;

        let Err(FunctionCallError::RespondToModel(message)) = result else {
            panic!("expected invalid image error");
        };
        assert_eq!(message, VIEW_IMAGE_INVALID_MESSAGE);
    }
}
