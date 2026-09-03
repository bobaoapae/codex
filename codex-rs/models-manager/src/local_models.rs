//! Models that are served by a local process rather than by the `/models`
//! endpoint.
//!
//! The remote catalog is authoritative for OpenAI models and replaces itself on
//! every refresh, so locally served models cannot live in `models.json` — a
//! successful refresh would drop them. They are merged into the catalog instead,
//! at the two points where the catalog is consumed: picker listing and metadata
//! lookup.

use codex_protocol::openai_models::ModelInfo;
use std::sync::OnceLock;

/// Provider id that serves the `claude-*` bundle (`wire_api = "claude_code"`).
const CLAUDE_CODE_PROVIDER_ID: &str = "claude_code";
/// FORK: provider id that serves the `chatgpt-web/*` bundle
/// (`wire_api = "chatgpt_web"`).
const CHATGPT_WEB_PROVIDER_ID: &str = "chatgpt_web";

/// One bundled catalog file and the provider that serves its models.
struct Bundle {
    provider_id: &'static str,
    /// Human-readable name, for the parse-failure log line.
    label: &'static str,
    json: &'static str,
}

/// FORK: every bundle of locally served models. Order is the merge order.
const BUNDLES: &[Bundle] = &[
    Bundle {
        provider_id: CLAUDE_CODE_PROVIDER_ID,
        label: "claude_code",
        json: include_str!("../claude_code_models.json"),
    },
    Bundle {
        provider_id: CHATGPT_WEB_PROVIDER_ID,
        label: "chatgpt_web",
        json: include_str!("../chatgpt_web_models.json"),
    },
];

fn parse_bundle(bundle: &Bundle) -> Vec<ModelInfo> {
    match serde_json::from_str::<codex_protocol::openai_models::ModelsResponse>(bundle.json) {
        Ok(response) => response.models,
        Err(err) => {
            // A malformed bundle must not take the picker down with it.
            tracing::error!("failed to parse bundled {} models: {err}", bundle.label);
            Vec::new()
        }
    }
}

/// Every locally served model, tagged with the provider that serves it.
fn locally_served_models_by_provider() -> &'static [(&'static str, ModelInfo)] {
    static MODELS: OnceLock<Vec<(&'static str, ModelInfo)>> = OnceLock::new();
    MODELS.get_or_init(|| {
        BUNDLES
            .iter()
            .flat_map(|bundle| {
                parse_bundle(bundle)
                    .into_iter()
                    .map(move |model| (bundle.provider_id, model))
            })
            .collect()
    })
}

/// Models backed by a local process: the Claude Code CLI
/// (`wire_api = "claude_code"`) and the ChatGPT web app
/// (`wire_api = "chatgpt_web"`).
pub fn locally_served_models() -> &'static [ModelInfo] {
    static MODELS: OnceLock<Vec<ModelInfo>> = OnceLock::new();
    MODELS.get_or_init(|| {
        locally_served_models_by_provider()
            .iter()
            .map(|(_, model)| model.clone())
            .collect()
    })
}

/// FORK: the provider id that serves a locally served model slug, or `None`
/// when the slug is not in any bundle.
///
/// Looked up by bundle membership rather than by slug prefix, so a bundle can
/// name its models however it likes.
pub fn provider_for_locally_served_model(slug: &str) -> Option<&'static str> {
    locally_served_models_by_provider()
        .iter()
        .find(|(_, model)| model.slug == slug)
        .map(|(provider_id, _)| *provider_id)
}

/// Appends locally served models to a catalog snapshot, skipping any slug the
/// catalog already defines so a remote definition always wins.
pub fn merge_locally_served_models(models: &mut Vec<ModelInfo>) {
    for model in locally_served_models() {
        if models.iter().any(|existing| existing.slug == model.slug) {
            continue;
        }
        models.push(model.clone());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use codex_protocol::openai_models::ApplyPatchToolType;
    use codex_protocol::openai_models::InputModality;
    use codex_protocol::openai_models::ModelVisibility;

    #[test]
    fn bundled_claude_models_parse() {
        serde_json::from_str::<codex_protocol::openai_models::ModelsResponse>(include_str!(
            "../claude_code_models.json"
        ))
        .unwrap_or_else(|err| panic!("bundled claude_code models should parse: {err}"));
        let models = locally_served_models();
        let slugs: Vec<&str> = models.iter().map(|model| model.slug.as_str()).collect();
        assert!(slugs.contains(&"claude-opus-5"), "got {slugs:?}");
        assert!(slugs.contains(&"claude-sonnet-5"), "got {slugs:?}");
        assert!(slugs.contains(&"claude-fable-5"), "got {slugs:?}");
        assert!(
            models
                .iter()
                .all(|model| model.visibility == ModelVisibility::Hide),
            "locally served models must be agent-only and hidden from the /model picker"
        );
        // FORK: all three Anthropic models are 1M-context on the API. Without a
        // bundled entry the caller falls back to a 272k default and sizes the
        // window (and the auto-compaction point) wrong.
        for slug in ["claude-opus-5", "claude-sonnet-5", "claude-fable-5"] {
            let model = models
                .iter()
                .find(|model| model.slug == slug)
                .unwrap_or_else(|| panic!("{slug} should be bundled"));
            assert_eq!(model.context_window, Some(1_000_000), "{slug}");
            assert_eq!(model.max_context_window, Some(1_000_000), "{slug}");
        }
    }

    /// FORK: the ChatGPT Web bundle — one line per reasoning level, each
    /// exposing exactly that level.
    #[test]
    fn bundled_chatgpt_web_models_parse_with_one_level_each() {
        let response = serde_json::from_str::<codex_protocol::openai_models::ModelsResponse>(
            include_str!("../chatgpt_web_models.json"),
        )
        .unwrap_or_else(|err| panic!("bundled chatgpt_web models should parse: {err}"));
        let slugs: Vec<&str> = response
            .models
            .iter()
            .map(|model| model.slug.as_str())
            .collect();
        assert_eq!(
            slugs,
            vec![
                "chatgpt-web/instant",
                "chatgpt-web/thinking",
                "chatgpt-web/high",
                "chatgpt-web/extra-high",
                "chatgpt-web/pro",
            ]
        );
        for model in &response.models {
            assert_eq!(
                model.supported_reasoning_levels.len(),
                1,
                "{} must expose exactly one level",
                model.slug
            );
            assert_eq!(
                model.default_reasoning_level.as_ref(),
                Some(&model.supported_reasoning_levels[0].effort),
                "{} default level must be its only level",
                model.slug
            );
            assert_eq!(model.visibility, ModelVisibility::Hide, "{}", model.slug);
            assert!(model.supported_in_api, "{}", model.slug);
            assert!(
                model.input_modalities.contains(&InputModality::Image),
                "{} must accept images",
                model.slug
            );
            // The connector mode routes `codex_apply_patch` through the
            // freeform `apply_patch` tool, which is only registered when the
            // model declares it.
            assert_eq!(
                model.apply_patch_tool_type,
                Some(ApplyPatchToolType::Freeform),
                "{}",
                model.slug
            );
        }
    }

    /// Core clamps `auto_compact_token_limit` to 90% of the context window;
    /// a bundled value above that would be silently rewritten.
    #[test]
    fn chatgpt_web_compaction_limits_fit_under_the_clamp() {
        for model in locally_served_models()
            .iter()
            .filter(|model| model.slug.starts_with("chatgpt-web/"))
        {
            let context_window = model
                .context_window
                .unwrap_or_else(|| panic!("{} needs a context window", model.slug));
            let limit = model
                .auto_compact_token_limit
                .unwrap_or_else(|| panic!("{} needs an auto-compact limit", model.slug));
            assert!(
                (limit as f64) <= 0.9 * (context_window as f64),
                "{}: {limit} exceeds 90% of {context_window}",
                model.slug
            );
            assert_eq!(
                model.max_context_window,
                Some(context_window),
                "{}",
                model.slug
            );
        }
    }

    #[test]
    fn provider_is_resolved_by_bundle_membership() {
        assert_eq!(
            provider_for_locally_served_model("claude-opus-5"),
            Some(CLAUDE_CODE_PROVIDER_ID)
        );
        assert_eq!(
            provider_for_locally_served_model("chatgpt-web/pro"),
            Some(CHATGPT_WEB_PROVIDER_ID)
        );
        assert_eq!(provider_for_locally_served_model("gpt-5"), None);
    }

    #[test]
    fn merge_does_not_override_catalog_definitions() {
        let mut catalog = locally_served_models().to_vec();
        catalog[0].display_name = "Overridden".to_string();
        let expected_len = catalog.len();

        merge_locally_served_models(&mut catalog);

        assert_eq!(catalog.len(), expected_len);
        assert_eq!(catalog[0].display_name, "Overridden");
    }

    #[test]
    fn merge_appends_missing_models() {
        let mut catalog = Vec::new();

        merge_locally_served_models(&mut catalog);

        assert_eq!(catalog.len(), locally_served_models().len());
    }
}
