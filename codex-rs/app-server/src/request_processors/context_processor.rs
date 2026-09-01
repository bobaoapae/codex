//! `context/inspect` request handling.
//!
//! This processor is deliberately read-only. Loaded threads are projected by
//! Core's context-inspection API, which observes the last request snapshot
//! without refreshing contributors. Stored threads that are not loaded use
//! Core's detached cold reconstruction and never start a runtime.

use crate::error_code::internal_error;
use crate::error_code::invalid_request;
use codex_app_server_protocol::ClientResponsePayload;
use codex_app_server_protocol::CompactionSurvival;
use codex_app_server_protocol::ContextInspectParams;
use codex_app_server_protocol::ContextInspectResponse;
use codex_app_server_protocol::ContextInspection;
use codex_app_server_protocol::ContextInspectionGroup;
use codex_app_server_protocol::ContextInspectionItem;
use codex_app_server_protocol::ContextLogicalOrigin;
use codex_app_server_protocol::ContextSnapshotKind;
use codex_app_server_protocol::ContextVisibility;
use codex_app_server_protocol::JSONRPCErrorError;
use codex_core::ThreadManager;
use codex_core::context_inspection::CompactionSurvival as CoreCompactionSurvival;
use codex_core::context_inspection::ContextInspection as CoreContextInspection;
use codex_core::context_inspection::ContextInspectionGroup as CoreContextInspectionGroup;
use codex_core::context_inspection::ContextInspectionItem as CoreContextInspectionItem;
use codex_core::context_inspection::ContextInspectionMode;
use codex_core::context_inspection::ContextInspectionOptions;
use codex_core::context_inspection::ContextLogicalOrigin as CoreContextLogicalOrigin;
use codex_core::context_inspection::ContextSnapshotKind as CoreContextSnapshotKind;
use codex_core::context_inspection::ContextVisibility as CoreContextVisibility;
use codex_protocol::ThreadId;
use codex_protocol::error::CodexErr;
use codex_protocol::error::CodexErrorDetails;
use codex_rollout::StateDbHandle;
use serde_json::json;
use std::sync::Arc;

const MAX_PREVIEW_CHARS: usize = 512;
const MAX_PREVIEW_TOKENS: usize = 10_000;

/// Handles read-only context inspection requests.
#[derive(Clone)]
pub(crate) struct ContextRequestProcessor {
    thread_manager: Arc<ThreadManager>,
    state_db: Option<StateDbHandle>,
}

impl ContextRequestProcessor {
    pub(crate) fn new(thread_manager: Arc<ThreadManager>, state_db: Option<StateDbHandle>) -> Self {
        Self {
            thread_manager,
            state_db,
        }
    }

    pub(crate) async fn context_inspect(
        &self,
        params: ContextInspectParams,
    ) -> Result<Option<ClientResponsePayload>, JSONRPCErrorError> {
        let raw_thread_id = params.thread_id;
        let thread_id = ThreadId::from_string(&raw_thread_id).map_err(|error| {
            context_error(
                invalid_request(format!("invalid thread id: {error}")),
                "invalid",
                &raw_thread_id,
            )
        })?;

        let Some(thread) = self.thread_manager.get_thread(thread_id).await.ok() else {
            return self
                .inspect_unloaded(thread_id, raw_thread_id.as_str(), params.include_preview)
                .await;
        };

        let inspection = thread
            .inspect_context(ContextInspectionOptions {
                mode: ContextInspectionMode::Loaded,
                include_preview: params.include_preview,
                turn_id: None,
            })
            .await
            .map_err(|error| inspection_error(thread_id, error))?;

        Ok(Some(
            ContextInspectResponse {
                context: map_inspection(inspection),
            }
            .into(),
        ))
    }

    async fn inspect_unloaded(
        &self,
        thread_id: ThreadId,
        raw_thread_id: &str,
        include_preview: bool,
    ) -> Result<Option<ClientResponsePayload>, JSONRPCErrorError> {
        if let Some(state_db) = self.state_db.as_ref() {
            match state_db.is_thread_tombstoned(thread_id).await {
                Ok(true) => {
                    return Err(context_error(
                        invalid_request(format!("thread is tombstoned: {thread_id}")),
                        "tombstoned",
                        raw_thread_id,
                    ));
                }
                Ok(false) => {}
                Err(error) => {
                    return Err(context_error(
                        internal_error(format!(
                            "failed to read thread state for {thread_id}: {error}"
                        )),
                        "stateUnavailable",
                        raw_thread_id,
                    ));
                }
            }
        }

        let inspection = self
            .thread_manager
            .inspect_stored_context(
                thread_id,
                ContextInspectionOptions {
                    mode: ContextInspectionMode::Cold,
                    include_preview,
                    turn_id: None,
                },
            )
            .await
            .map_err(|error| inspection_error(thread_id, error))?;

        Ok(Some(
            ContextInspectResponse {
                context: map_inspection(inspection),
            }
            .into(),
        ))
    }
}

fn context_error(
    mut error: JSONRPCErrorError,
    reason: &'static str,
    thread_id: &str,
) -> JSONRPCErrorError {
    error.data = Some(json!({
        "reason": reason,
        "threadId": thread_id,
    }));
    error
}

fn inspection_error(thread_id: ThreadId, error: CodexErr) -> JSONRPCErrorError {
    match error.details() {
        CodexErrorDetails::InvalidRequest(message) => context_error(
            invalid_request(message.clone()),
            "invalid",
            &thread_id.to_string(),
        ),
        CodexErrorDetails::ThreadNotFound(_) => context_error(
            invalid_request(format!("thread not found: {thread_id}")),
            "notFound",
            &thread_id.to_string(),
        ),
        CodexErrorDetails::Fatal(message) => context_error(
            internal_error(message.clone()),
            "stateUnavailable",
            &thread_id.to_string(),
        ),
        _ => context_error(
            internal_error(format!(
                "failed to inspect context for {thread_id}: {error}"
            )),
            "stateUnavailable",
            &thread_id.to_string(),
        ),
    }
}

fn map_inspection(value: CoreContextInspection) -> ContextInspection {
    let mut inspection = ContextInspection {
        thread_id: value.thread_id.to_string(),
        turn_id: value.turn_id,
        snapshot_kind: map_snapshot_kind(value.snapshot_kind),
        partial: value.partial,
        item_count: value.item_count,
        estimated_prompt_tokens: value.estimated_prompt_tokens,
        estimated_active_tokens: value.estimated_active_tokens,
        estimated_context_window_tokens: value.estimated_context_window_tokens,
        cached_input_tokens: value.cached_input_tokens,
        uncached_input_tokens: value.uncached_input_tokens,
        cache_write_input_tokens: value.cache_write_input_tokens,
        runtime_build_info: value.runtime_build_info.map(Into::into),
        config_layer_revision: value.config_layer_revision,
        runtime_feature_revision: value.runtime_feature_revision,
        persisted_runtime_build_info: value.persisted_runtime_build_info.map(Into::into),
        persisted_config_layer_revision: value.persisted_config_layer_revision,
        persisted_runtime_feature_revision: value.persisted_runtime_feature_revision,
        stale: value.stale,
        window_id: value.window_id,
        context_window_id: value.context_window_id,
        window_number: value.window_number,
        first_window_id: value.first_window_id,
        previous_window_id: value.previous_window_id,
        checkpoint_id: value.checkpoint_id,
        checkpoint_revision: value.checkpoint_revision,
        base_instructions: map_group(value.base_instructions),
        tools: map_group(value.tools),
        items: value.items.into_iter().map(map_item).collect(),
    };
    revalidate_previews(&mut inspection);
    inspection
}

fn revalidate_previews(inspection: &mut ContextInspection) {
    let mut remaining = MAX_PREVIEW_TOKENS;
    revalidate_group_preview(&mut inspection.base_instructions, &mut remaining);
    revalidate_group_preview(&mut inspection.tools, &mut remaining);
    for item in &mut inspection.items {
        revalidate_preview(&mut item.preview, item.encrypted, &mut remaining);
    }
}

fn revalidate_group_preview(group: &mut ContextInspectionGroup, remaining: &mut usize) {
    revalidate_preview(&mut group.preview, group.encrypted, remaining);
}

fn revalidate_preview(preview: &mut Option<String>, encrypted: bool, remaining: &mut usize) {
    if encrypted {
        *preview = None;
        return;
    }
    let Some(value) = preview.take() else {
        return;
    };
    let limit = value.chars().count().min(MAX_PREVIEW_CHARS).min(*remaining);
    if limit == 0 {
        return;
    }
    *remaining = (*remaining).saturating_sub(limit);
    *preview = Some(value.chars().take(limit).collect());
}

fn map_snapshot_kind(value: CoreContextSnapshotKind) -> ContextSnapshotKind {
    match value {
        CoreContextSnapshotKind::Live => ContextSnapshotKind::Live,
        CoreContextSnapshotKind::Speculative => ContextSnapshotKind::Speculative,
        CoreContextSnapshotKind::Cold => ContextSnapshotKind::Cold,
    }
}

fn map_origin(value: CoreContextLogicalOrigin) -> ContextLogicalOrigin {
    match value {
        CoreContextLogicalOrigin::BaseInstructions => ContextLogicalOrigin::BaseInstructions,
        CoreContextLogicalOrigin::ThreadContext => ContextLogicalOrigin::ThreadContext,
        CoreContextLogicalOrigin::TurnContext => ContextLogicalOrigin::TurnContext,
        CoreContextLogicalOrigin::WorldState => ContextLogicalOrigin::WorldState,
        CoreContextLogicalOrigin::InheritedHistory => ContextLogicalOrigin::InheritedHistory,
        CoreContextLogicalOrigin::NewOutput => ContextLogicalOrigin::NewOutput,
        CoreContextLogicalOrigin::ToolOutput => ContextLogicalOrigin::ToolOutput,
        CoreContextLogicalOrigin::CompactionReplacement => {
            ContextLogicalOrigin::CompactionReplacement
        }
        CoreContextLogicalOrigin::Derived => ContextLogicalOrigin::Derived,
        CoreContextLogicalOrigin::Unknown => ContextLogicalOrigin::Unknown,
    }
}

fn map_visibility(value: CoreContextVisibility) -> ContextVisibility {
    match value {
        CoreContextVisibility::Model => ContextVisibility::Model,
        CoreContextVisibility::User => ContextVisibility::User,
        CoreContextVisibility::Internal => ContextVisibility::Internal,
        CoreContextVisibility::Unknown => ContextVisibility::Unknown,
    }
}

fn map_survival(value: CoreCompactionSurvival) -> CompactionSurvival {
    match value {
        CoreCompactionSurvival::True => CompactionSurvival::True,
        CoreCompactionSurvival::False => CompactionSurvival::False,
        CoreCompactionSurvival::Unknown => CompactionSurvival::Unknown,
    }
}

fn map_group(value: CoreContextInspectionGroup) -> ContextInspectionGroup {
    ContextInspectionGroup {
        index: value.index,
        item_count: value.item_count,
        role: value.role,
        content_kind: value.content_kind,
        logical_origin: map_origin(value.logical_origin),
        visibility: map_visibility(value.visibility),
        estimated_tokens: value.estimated_tokens,
        serialized_bytes: value.serialized_bytes,
        survives_compaction: map_survival(value.survives_compaction),
        encrypted: value.encrypted,
        duplicate_group: value.duplicate_group,
        duplicate_count: value.duplicate_count,
        preview: value.preview,
    }
}

fn map_item(value: CoreContextInspectionItem) -> ContextInspectionItem {
    ContextInspectionItem {
        index: value.index,
        role: value.role,
        content_kind: value.content_kind,
        logical_origin: map_origin(value.logical_origin),
        visibility: map_visibility(value.visibility),
        estimated_tokens: value.estimated_tokens,
        serialized_bytes: value.serialized_bytes,
        survives_compaction: map_survival(value.survives_compaction),
        duplicate_group: value.duplicate_group,
        duplicate_count: value.duplicate_count,
        encrypted: value.encrypted,
        preview: value.preview,
    }
}

#[cfg(test)]
#[path = "context_processor_tests.rs"]
mod tests;
