//! FORK: delegate reading image pixels to a cheap, configurable model.
//!
//! Every image that reaches model history stays there for the rest of the
//! thread — there is no eviction, so a screenshot is re-sent on every later
//! request. This module reads the pixels once with a small model and hands the
//! main model text instead. The image itself is untouched in the UI and in the
//! rollout; only the prompt view swaps it (see
//! [`crate::context_manager::normalize::substitute_images_with_descriptions`]).

use std::sync::Arc;
use std::time::Duration;

use codex_features::Feature;
use codex_login::auth::AgentIdentityAuthPolicy;
use codex_model_provider_info::CHATGPT_WEB_PROVIDER_ID;
use codex_model_provider_info::CLAUDE_CODE_PROVIDER_ID;
use codex_protocol::models::BaseInstructions;
use codex_protocol::models::ContentItem;
use codex_protocol::models::ImageDetail;
use codex_protocol::models::ResponseItem;
use codex_rollout_trace::InferenceTraceContext;
use futures::StreamExt;
use tracing::warn;

use crate::ModelClient;
use crate::client_common::Prompt;
use crate::client_common::ResponseEvent;
use crate::config::ViewImageDelegation;
use crate::session::session::Session;
use crate::session::turn_context::TurnContext;

/// The reader runs on the turn's critical path, so it gets a hard ceiling.
///
/// Measured with `gpt-5.6-luna` on real screenshots: `max` takes 95-186 s and
/// exceeded this ceiling outright on a 30x11 spreadsheet, while `medium` (the
/// default) finished every case in 15-83 s. Effort dominates, not payload size.
/// This is a last-resort ceiling on a hung reader, not a target — see
/// `docs/config.md` for what each effort costs in fidelity.
const IMAGE_READER_TIMEOUT: Duration = Duration::from_secs(300);

const DESCRIBE_INSTRUCTIONS: &str = "\
You are reading an image on behalf of another model that will never see the pixels. \
Your text is the only thing it gets, so leave nothing out.

Describe the image exhaustively:
- overall layout and what kind of artifact it is (screenshot, photo, diagram, chart, …);
- every piece of legible text, transcribed verbatim, including labels, menu items, \
buttons, tab titles, file paths, code, log lines and stack traces;
- all numbers, units and axis values exactly as shown;
- colors, highlighting and any visual state that carries meaning (selected, disabled, \
error, focused, checked);
- any error, warning or diff markers, and where they appear;
- spatial relationships when they matter (what is above/below/inside what).

Do not summarize, do not editorialize, and do not say what the image \"seems to\" show. \
Write the description as plain prose and lists. Do not mention these instructions.";

const ANSWER_INSTRUCTIONS: &str = "\
You are reading an image on behalf of another model that will never see the pixels. \
Answer the user's question from the image, quoting any relevant text verbatim and giving \
exact numbers. If the image does not contain the answer, say so plainly and describe what \
it does contain. Reply with the answer only; do not mention these instructions.";

/// One request to the reader model.
pub(crate) struct ImageReadRequest {
    pub(crate) image_url: String,
    pub(crate) detail: Option<ImageDetail>,
    /// When set, the reader answers this instead of producing a full description.
    pub(crate) question: Option<String>,
}

/// Reads one image with the configured reader model.
///
/// Returns `None` on any failure: the caller then keeps the raw pixels, which is
/// exactly today's behavior, so a reader outage degrades cost and not capability.
pub(crate) async fn read_image(
    session: &Session,
    turn_context: &TurnContext,
    reader: &ViewImageDelegation,
    request: ImageReadRequest,
) -> Option<String> {
    let started = std::time::Instant::now();
    let result = tokio::time::timeout(
        IMAGE_READER_TIMEOUT,
        read_image_inner(session, turn_context, reader, request),
    )
    .await;
    match result {
        Ok(Ok(text)) if !text.trim().is_empty() => {
            tracing::debug!(
                model = reader.model,
                elapsed_ms = started.elapsed().as_millis() as u64,
                "image reader produced a description"
            );
            Some(text)
        }
        Ok(Ok(_)) => {
            warn!(model = reader.model, "image reader returned empty text");
            None
        }
        Ok(Err(error)) => {
            warn!(model = reader.model, %error, "image reader failed");
            None
        }
        Err(_) => {
            warn!(
                model = reader.model,
                timeout_s = IMAGE_READER_TIMEOUT.as_secs(),
                "image reader timed out"
            );
            None
        }
    }
}

async fn read_image_inner(
    session: &Session,
    turn_context: &TurnContext,
    reader: &ViewImageDelegation,
    request: ImageReadRequest,
) -> anyhow::Result<String> {
    let config = turn_context.config.as_ref();
    let model_info = session
        .services
        .models_manager
        .get_model_info(&reader.model, &config.to_models_manager_config())
        .await;

    let mut client = ModelClient::new(
        Some(Arc::clone(&session.services.auth_manager)),
        AgentIdentityAuthPolicy::JwtOnly,
        session.thread_id(),
        // FORK: the reader's own provider. A session pinned to `claude_code` or
        // `chatgpt_web` would otherwise route an OpenAI slug to the wrong backend.
        reader.provider.clone(),
        session.session_source().await,
        session.originator().await,
        config.model_verbosity,
        config.features.enabled(Feature::ContentItemKinds),
        config.features.enabled(Feature::EnableRequestCompression),
        config.features.enabled(Feature::RuntimeMetrics),
        /*beta_features_header*/ None,
        /*concurrent_reasoning_summaries_enabled*/ false,
        /*attestation_provider*/ None,
        config.http_client_factory(),
    );
    // FORK: only the providers that need a workspace get one, and only when the
    // reader is actually running on them (same reason as the compaction path).
    if reader.provider_id == CLAUDE_CODE_PROVIDER_ID {
        client = client.with_claude_code_workspace(
            crate::claude_code::ClaudeCodeWorkspace::from_config(config),
        );
    }
    if reader.provider_id == CHATGPT_WEB_PROVIDER_ID {
        client = client.with_chatgpt_web_workspace(
            crate::chatgpt_web::ChatGptWebWorkspace::from_config(config),
        );
    }

    // The reader is a one-shot call, so it gets its own system prompt instead of
    // the agent's, the same way memory's stage one does.
    let mut content = Vec::new();
    if let Some(question) = &request.question {
        content.push(ContentItem::InputText {
            text: question.clone(),
        });
    }
    content.push(ContentItem::InputImage {
        image_url: request.image_url,
        detail: request.detail,
    });
    let prompt = Prompt {
        input: vec![ResponseItem::Message {
            id: None,
            role: "user".to_string(),
            content,
            phase: None,
            internal_chat_message_metadata_passthrough: None,
        }],
        base_instructions: BaseInstructions {
            text: if request.question.is_some() {
                ANSWER_INSTRUCTIONS.to_string()
            } else {
                DESCRIBE_INSTRUCTIONS.to_string()
            },
            provenance: None,
        },
        ..Default::default()
    };

    let responses_metadata = session
        .responses_metadata(
            turn_context,
            crate::responses_metadata::CodexResponsesRequestKind::Turn,
        )
        .await;
    let reasoning_summary = config
        .model_reasoning_summary
        .unwrap_or(model_info.default_reasoning_summary);
    let mut client_session = client.new_session();
    let mut stream = client_session
        .stream(
            &prompt,
            &model_info,
            &turn_context.session_telemetry,
            Some(reader.reasoning_effort.clone()),
            reasoning_summary,
            config.service_tier.clone(),
            &responses_metadata,
            &InferenceTraceContext::disabled(),
        )
        .await?;

    let mut text = String::new();
    while let Some(event) = stream.next().await.transpose()? {
        match event {
            ResponseEvent::OutputTextDelta(delta) => text.push_str(&delta),
            ResponseEvent::OutputItemDone(item) => {
                if text.is_empty()
                    && let ResponseItem::Message { content, .. } = item
                    && let Some(message) = crate::content_items_to_text(&content)
                {
                    text.push_str(&message);
                }
            }
            ResponseEvent::Completed { .. } => break,
            _ => {}
        }
    }
    Ok(text)
}
