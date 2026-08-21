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

/// Models backed by the local Claude Code CLI (`wire_api = "claude_code"`).
pub fn locally_served_models() -> &'static [ModelInfo] {
    static MODELS: OnceLock<Vec<ModelInfo>> = OnceLock::new();
    MODELS.get_or_init(|| {
        match serde_json::from_str::<codex_protocol::openai_models::ModelsResponse>(include_str!(
            "../claude_code_models.json"
        )) {
            Ok(response) => response.models,
            Err(err) => {
                // A malformed bundle must not take the picker down with it.
                tracing::error!("failed to parse bundled claude_code models: {err}");
                Vec::new()
            }
        }
    })
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
        assert!(
            models
                .iter()
                .all(|model| model.visibility == ModelVisibility::Hide),
            "Claude models must be agent-only and hidden from the /model picker"
        );
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
