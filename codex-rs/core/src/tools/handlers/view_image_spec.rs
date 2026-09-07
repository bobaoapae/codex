use codex_protocol::models::VIEW_IMAGE_TOOL_NAME;
use codex_tools::JsonSchema;
use codex_tools::ResponsesApiTool;
use codex_tools::ToolSpec;
use serde_json::Value;
use serde_json::json;
use std::collections::BTreeMap;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ViewImageToolOptions {
    pub can_request_original_image_detail: bool,
    pub unified_image_budget: bool,
    pub include_environment_id: bool,
    /// FORK: a cheaper model reads the pixels, so the tool also answers
    /// questions about the image and can be forced to return the pixels.
    pub delegate: bool,
}

pub fn create_view_image_tool(options: ViewImageToolOptions) -> ToolSpec {
    let mut properties = BTreeMap::from([(
        "path".to_string(),
        JsonSchema::string(Some("Local filesystem path to an image file.".to_string())),
    )]);
    if options.can_request_original_image_detail && !options.unified_image_budget {
        properties.insert(
            "detail".to_string(),
            JsonSchema::string_enum(
                vec![json!("high"), json!("original")],
                Some(
                    "Image detail level. Defaults to `high`; use `original` to preserve exact resolution.".to_string(),
                ),
            ),
        );
    }
    if options.include_environment_id {
        properties.insert(
            "environment_id".to_string(),
            JsonSchema::string(Some(
                "Environment id from <environment_context>. Omit to use the primary environment."
                    .to_string(),
            )),
        );
    }

    if options.delegate {
        properties.insert(
            "question".to_string(),
            JsonSchema::string(Some(
                "Ask about the image instead of loading it. Returns the reader model's answer as `text`; the image is still shown in the thread. Use this for follow-up detail on an image you already viewed.".to_string(),
            )),
        );
        properties.insert(
            "raw".to_string(),
            JsonSchema::boolean(Some(
                "Keep the pixels in your context instead of the reader model's description. Costs image tokens on every later request, so use it only when the description is not enough.".to_string(),
            )),
        );
    }

    let description = if options.delegate {
        "View a local image file from the filesystem when visual inspection is needed. Use this for images already available on disk. Images are read for you by a cheaper model and reach you as text: pass `question` to ask that model something specific, or `raw: true` to keep the pixels themselves."
    } else {
        "View a local image file from the filesystem when visual inspection is needed. Use this for images already available on disk."
    };

    ToolSpec::Function(ResponsesApiTool {
        name: VIEW_IMAGE_TOOL_NAME.to_string(),
        description: description.to_string(),
        strict: false,
        defer_loading: None,
        parameters: JsonSchema::object(
            properties,
            Some(vec!["path".to_string()]),
            Some(false.into()),
        ),
        output_schema: Some(view_image_output_schema(options)),
    })
}

fn view_image_output_schema(options: ViewImageToolOptions) -> Value {
    let mut schema = json!({
        "type": "object",
        "properties": {
            "image_url": {
                "type": "string",
                "description": "Data URL for the loaded image."
            }
        },
        "required": ["image_url"],
        "additionalProperties": false
    });
    if !options.unified_image_budget {
        schema["properties"]["detail"] = json!({
            "type": "string",
            "enum": ["high", "original"],
            "description": "Image detail hint returned by view_image. Returns `high` for default resized behavior or `original` when original resolution is preserved."
        });
        schema["required"] = json!(["image_url", "detail"]);
    }
    // FORK: with `question` the tool returns text and no image, so neither field
    // can be required any more.
    if options.delegate {
        schema["properties"]["text"] = json!({
            "type": "string",
            "description": "The reader model's answer, returned instead of the image when `question` is set."
        });
        schema["required"] = json!([]);
    }
    schema
}
