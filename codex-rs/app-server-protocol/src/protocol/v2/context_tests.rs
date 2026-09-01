use super::*;
use crate::ClientRequest;
use crate::ClientRequestSerializationScope;
use crate::ExperimentalApi as ExperimentalApiTrait;
use crate::RequestId;
use pretty_assertions::assert_eq;
use serde_json::json;

#[test]
fn fork_invariant_context_requests_are_experimental_and_thread_scoped() {
    let request = ClientRequest::ContextInspect {
        request_id: RequestId::Integer(1),
        params: ContextInspectParams {
            thread_id: "thread-1".to_string(),
            include_preview: false,
        },
    };

    assert_eq!(
        serde_json::to_value(&request).expect("context request should serialize"),
        json!({
            "method": "context/inspect",
            "id": 1,
            "params": {
                "threadId": "thread-1",
                "includePreview": false,
            },
        })
    );
    assert_eq!(
        request.serialization_scope(),
        Some(ClientRequestSerializationScope::Thread {
            thread_id: "thread-1".to_string(),
        })
    );
    assert_eq!(
        ExperimentalApiTrait::experimental_reason(&request),
        Some("context/inspect")
    );

    let params: ContextInspectParams = serde_json::from_value(json!({
        "threadId": "thread-1",
    }))
    .expect("omitted includePreview should deserialize");
    assert_eq!(params.include_preview, false);
}

#[test]
fn context_inspection_serializes_all_optional_fields_as_null() {
    let group = ContextInspectionGroup {
        index: 0,
        item_count: 0,
        role: "system".to_string(),
        content_kind: "baseInstructions".to_string(),
        logical_origin: ContextLogicalOrigin::BaseInstructions,
        visibility: ContextVisibility::Model,
        estimated_tokens: 0,
        serialized_bytes: 0,
        survives_compaction: CompactionSurvival::True,
        encrypted: false,
        duplicate_group: None,
        duplicate_count: 1,
        preview: None,
    };
    let response = ContextInspectResponse {
        context: ContextInspection {
            thread_id: "thread-1".to_string(),
            turn_id: None,
            snapshot_kind: ContextSnapshotKind::Speculative,
            partial: true,
            item_count: 0,
            estimated_prompt_tokens: None,
            estimated_active_tokens: None,
            estimated_context_window_tokens: None,
            cached_input_tokens: None,
            uncached_input_tokens: None,
            cache_write_input_tokens: None,
            runtime_build_info: Some(ContextRuntimeBuildInfo {
                version: "0.0.0".to_string(),
                build_commit: "source".to_string(),
                target: "windows-x86_64".to_string(),
            }),
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
            base_instructions: group.clone(),
            tools: group,
            items: Vec::new(),
        },
    };

    let value = serde_json::to_value(response).expect("context response should serialize");
    assert_eq!(
        value["context"]["runtimeBuildInfo"]["buildCommit"],
        "source"
    );
    assert_eq!(value["context"]["turnId"], json!(null));
    assert_eq!(value["context"]["estimatedPromptTokens"], json!(null));
    assert_eq!(value["context"]["baseInstructions"]["preview"], json!(null));
    assert_eq!(
        value["context"]["baseInstructions"]["duplicateGroup"],
        json!(null)
    );
}
