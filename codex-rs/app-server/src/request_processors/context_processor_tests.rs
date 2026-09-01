use super::*;
use pretty_assertions::assert_eq;

fn group(preview: Option<String>, encrypted: bool) -> ContextInspectionGroup {
    ContextInspectionGroup {
        index: 0,
        item_count: 1,
        role: "system".to_string(),
        content_kind: "baseInstructions".to_string(),
        logical_origin: ContextLogicalOrigin::BaseInstructions,
        visibility: ContextVisibility::Model,
        estimated_tokens: 0,
        serialized_bytes: 0,
        survives_compaction: CompactionSurvival::True,
        encrypted,
        duplicate_group: None,
        duplicate_count: 1,
        preview,
    }
}

#[test]
fn revalidate_previews_enforces_redaction_bounds_again_at_wire_boundary() {
    let mut inspection = ContextInspection {
        thread_id: "thread-1".to_string(),
        turn_id: None,
        snapshot_kind: ContextSnapshotKind::Speculative,
        partial: true,
        item_count: 1,
        estimated_prompt_tokens: None,
        estimated_active_tokens: None,
        estimated_context_window_tokens: None,
        cached_input_tokens: None,
        uncached_input_tokens: None,
        cache_write_input_tokens: None,
        runtime_build_info: None,
        config_layer_revision: None,
        runtime_feature_revision: None,
        persisted_runtime_build_info: None,
        persisted_config_layer_revision: None,
        persisted_runtime_feature_revision: None,
        stale: false,
        window_id: None,
        context_window_id: None,
        window_number: None,
        first_window_id: None,
        previous_window_id: None,
        checkpoint_id: None,
        checkpoint_revision: None,
        base_instructions: group(Some("a".repeat(MAX_PREVIEW_CHARS + 1)), false),
        tools: group(None, false),
        items: vec![ContextInspectionItem {
            index: 0,
            role: "assistant".to_string(),
            content_kind: "reasoning".to_string(),
            logical_origin: ContextLogicalOrigin::Unknown,
            visibility: ContextVisibility::Model,
            estimated_tokens: 0,
            serialized_bytes: 0,
            survives_compaction: CompactionSurvival::Unknown,
            duplicate_group: None,
            duplicate_count: 1,
            encrypted: true,
            preview: Some("ciphertext".to_string()),
        }],
    };

    revalidate_previews(&mut inspection);

    assert_eq!(
        inspection
            .base_instructions
            .preview
            .as_ref()
            .map(|preview| preview.chars().count()),
        Some(MAX_PREVIEW_CHARS)
    );
    assert_eq!(inspection.items[0].preview, None);
}
