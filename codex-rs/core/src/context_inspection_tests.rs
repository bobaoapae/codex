use super::*;
use crate::client_common::Prompt;
use codex_protocol::models::BaseInstructions;
use codex_protocol::models::ContentItem;
use codex_protocol::models::ContentItemKind;
use codex_protocol::models::FunctionCallOutputPayload;
use codex_protocol::models::InternalChatMessageMetadataPassthrough;
use codex_protocol::models::ReasoningItemReasoningSummary;
use codex_protocol::models::ResponseItem;
use codex_protocol::protocol::TokenUsage;
use pretty_assertions::assert_eq;

fn message(role: &str, text: &str, kind: Option<&str>) -> ResponseItem {
    ResponseItem::Message {
        id: None,
        role: role.to_string(),
        content: vec![ContentItem::InputText {
            text: text.to_string(),
        }],
        phase: None,
        internal_chat_message_metadata_passthrough: kind.map(|kind| {
            InternalChatMessageMetadataPassthrough {
                content_item_kinds: Some(vec![ContentItemKind(kind.to_string())]),
                ..Default::default()
            }
        }),
    }
}

fn prompt(items: Vec<ResponseItem>) -> Prompt {
    Prompt {
        input: items,
        base_instructions: BaseInstructions {
            text: "base instructions".to_string(),
            provenance: None,
        },
        ..Prompt::default()
    }
}

#[test]
fn inspection_preserves_prompt_item_count_and_legacy_unknown_kind() {
    let prompt = prompt(vec![
        message("user", "hello", Some("user.text")),
        message("assistant", "legacy", None),
    ]);
    let inspection = inspect_prompt_for_tests(
        codex_protocol::ThreadId::new(),
        &prompt,
        &ContextInspectionOptions::default(),
    );

    assert_eq!(inspection.item_count, prompt.input.len());
    assert_eq!(inspection.items[0].content_kind, "user.text");
    assert_eq!(inspection.items[1].content_kind, "unknown");
    assert_eq!(inspection.base_instructions.item_count, 1);
}

#[test]
fn inspection_uses_history_boundary_and_compaction_replacement() {
    let inherited = message("user", "inherited", Some("user.text"));
    let new_output = {
        let mut item = message("assistant", "new", Some("assistant.text"));
        item.set_turn_id_if_missing("turn-2");
        item
    };
    let prompt = prompt(vec![inherited.clone(), new_output]);
    let assembly = InspectionAssembly {
        snapshot_kind: ContextSnapshotKind::Live,
        turn_id: Some("turn-2".to_string()),
        dynamic_context_available: true,
        source_available: true,
        inherited_item_count: Some(1),
        replacement_items: vec![inherited],
        active_tokens: None,
        usage: None,
        context_window_tokens: None,
        window_id: None,
        window_number: None,
        first_window_id: None,
        previous_window_id: None,
        context_window_id: None,
        checkpoint: CheckpointMetadata::default(),
        persisted: PersistedMetadata::default(),
    };
    let inspection = build_inspection_with_metadata(
        codex_protocol::ThreadId::new(),
        &prompt,
        &assembly,
        None,
        None,
        None,
        &ContextInspectionOptions::default(),
    );

    assert_eq!(
        inspection.items[0].logical_origin,
        ContextLogicalOrigin::CompactionReplacement
    );
    assert_eq!(
        inspection.items[0].survives_compaction,
        CompactionSurvival::True
    );
    assert_eq!(
        inspection.items[1].logical_origin,
        ContextLogicalOrigin::NewOutput
    );
    assert_eq!(
        inspection.items[1].survives_compaction,
        CompactionSurvival::Unknown
    );
}

#[test]
fn inspection_reports_aggregate_cache_usage_without_item_claims() {
    let prompt = prompt(vec![message("user", "cached", Some("user.text"))]);
    let assembly = InspectionAssembly {
        snapshot_kind: ContextSnapshotKind::Cold,
        turn_id: None,
        dynamic_context_available: false,
        source_available: true,
        inherited_item_count: None,
        replacement_items: Vec::new(),
        active_tokens: Some(170),
        usage: Some(TokenUsage {
            input_tokens: 100,
            cached_input_tokens: 60,
            cache_write_input_tokens: 10,
            total_tokens: 170,
            ..Default::default()
        }),
        context_window_tokens: Some(200),
        window_id: None,
        window_number: None,
        first_window_id: None,
        previous_window_id: None,
        context_window_id: None,
        checkpoint: CheckpointMetadata::default(),
        persisted: PersistedMetadata::default(),
    };
    let inspection = build_inspection_with_metadata(
        codex_protocol::ThreadId::new(),
        &prompt,
        &assembly,
        None,
        None,
        None,
        &ContextInspectionOptions::default(),
    );

    assert_eq!(inspection.cached_input_tokens, Some(60));
    assert_eq!(inspection.uncached_input_tokens, Some(30));
    assert_eq!(inspection.cache_write_input_tokens, Some(10));
    assert_eq!(inspection.estimated_active_tokens, Some(170));
    assert_eq!(
        inspection.items[0].duplicate_group, None,
        "cache accounting must not be attached to item records"
    );
}

#[test]
fn fork_invariant_context_preview_excludes_ciphertext_and_tool_output() {
    let long = "x".repeat(700);
    let prompt = prompt(vec![
        message(
            "user",
            "api_key=super-secret password: hidden",
            Some("user.text"),
        ),
        message("user", &long, Some("user.text")),
        message(
            "user",
            "https://example.test/media.png?token=secret",
            Some("user.text"),
        ),
        ResponseItem::Message {
            id: None,
            role: "user".to_string(),
            content: vec![ContentItem::InputImage {
                image_url: "data:image/png;base64,AAAA".to_string(),
                detail: None,
            }],
            phase: None,
            internal_chat_message_metadata_passthrough: None,
        },
        ResponseItem::Reasoning {
            id: None,
            summary: vec![ReasoningItemReasoningSummary::SummaryText {
                text: "private reasoning".to_string(),
            }],
            content: None,
            encrypted_content: Some("ciphertext".to_string()),
            internal_chat_message_metadata_passthrough: None,
        },
        ResponseItem::FunctionCallOutput {
            id: None,
            call_id: Some("call".to_string()),
            name: None,
            namespace: None,
            output: FunctionCallOutputPayload::from_text("raw tool output".to_string()),
            internal_chat_message_metadata_passthrough: None,
        },
    ]);
    let options = ContextInspectionOptions {
        include_preview: true,
        ..Default::default()
    };
    let inspection = inspect_prompt_for_tests(codex_protocol::ThreadId::new(), &prompt, &options);

    let secret_preview = inspection.items[0].preview.as_deref().unwrap_or_default();
    assert!(!secret_preview.contains("super-secret"));
    assert!(!secret_preview.contains("hidden"));
    assert!(secret_preview.contains(REDACTED));
    assert!(
        inspection.items[1]
            .preview
            .as_ref()
            .is_some_and(|preview| { preview.chars().count() <= MAX_PREVIEW_CHARS })
    );
    assert!(inspection.items[2].preview.is_none());
    assert!(inspection.items[3].preview.is_none());
    assert!(inspection.items[4].encrypted);
    assert!(inspection.items[4].preview.is_none());
    assert!(inspection.items[5].preview.is_none());
}

#[test]
fn inspection_marks_stale_provenance_when_checkpoint_is_missing_or_differs() {
    let persisted = PersistedMetadata {
        available: true,
        runtime_build_info: Some(RuntimeBuildInfo {
            version: "old".to_string(),
            build_commit: "old".to_string(),
            target: "old".to_string(),
        }),
        config_layer_revision: Some("old-config".to_string()),
        runtime_feature_revision: Some("old-features".to_string()),
        ..Default::default()
    };
    assert!(provenance_is_stale(
        Some(&RuntimeBuildInfo::current()),
        Some("new-config"),
        Some("new-features"),
        &persisted,
    ));
    assert!(provenance_is_stale(
        None,
        None,
        None,
        &PersistedMetadata::default()
    ));
}
