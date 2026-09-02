//! Persisted model-context provenance and checkpoint helpers.

use crate::session::session::Session;
use codex_history::RolloutItem;
use codex_protocol::ThreadId;
use codex_protocol::models::BaseInstructions;
use codex_protocol::models::ResponseItem;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::RuntimeBuildInfo;
use codex_protocol::protocol::TokenUsage;
use codex_thread_store::LoadThreadHistoryParams;

const MAX_REPLACEMENT_ITEMS: usize = 4_096;

pub(super) fn provenance_is_stale(
    current_build: Option<&RuntimeBuildInfo>,
    current_config_revision: Option<&str>,
    current_runtime_feature_revision: Option<&str>,
    persisted: &PersistedMetadata,
) -> bool {
    match (
        current_build,
        current_config_revision,
        current_runtime_feature_revision,
        persisted.runtime_build_info.as_ref(),
        persisted.config_layer_revision.as_deref(),
        persisted.runtime_feature_revision.as_deref(),
    ) {
        (
            Some(current_build),
            Some(current_config),
            Some(current_features),
            Some(persisted_build),
            Some(persisted_config),
            Some(persisted_features),
        ) => {
            current_build != persisted_build
                || current_config != persisted_config
                || current_features != persisted_features
        }
        _ => true,
    }
}

#[derive(Clone, Debug, Default)]
pub(super) struct PersistedMetadata {
    pub(super) available: bool,
    pub(super) base_instructions: Option<BaseInstructions>,
    pub(super) runtime_build_info: Option<RuntimeBuildInfo>,
    pub(super) config_layer_revision: Option<String>,
    pub(super) runtime_feature_revision: Option<String>,
}

#[derive(Clone, Debug, Default)]
pub(super) struct CheckpointMetadata {
    pub(super) id: Option<String>,
    pub(super) revision: Option<u64>,
}

pub(super) fn persisted_metadata(
    items: &[RolloutItem],
    expected_thread_id: ThreadId,
) -> PersistedMetadata {
    let mut metadata = PersistedMetadata::default();
    for item in items {
        match item {
            RolloutItem::SessionMeta(line) => {
                if line.meta.id != expected_thread_id {
                    continue;
                }
                metadata.available = true;
                metadata.base_instructions = line.meta.base_instructions.clone();
                metadata.runtime_build_info = line.meta.runtime_build_info.clone();
                metadata.config_layer_revision = line.meta.config_layer_revision.clone();
                metadata.runtime_feature_revision = line.meta.runtime_feature_revision.clone();
            }
            RolloutItem::EventMsg(EventMsg::ThreadSettingsApplied(event)) => {
                if event
                    .thread_id
                    .is_some_and(|thread_id| thread_id != expected_thread_id)
                {
                    continue;
                }
                metadata.available = true;
                if event.runtime_build_info.is_some() {
                    metadata.runtime_build_info = event.runtime_build_info.clone();
                }
                if event.config_layer_revision.is_some() {
                    metadata.config_layer_revision = event.config_layer_revision.clone();
                }
                if event.runtime_feature_revision.is_some() {
                    metadata.runtime_feature_revision = event.runtime_feature_revision.clone();
                }
            }
            _ => {}
        }
    }
    metadata
}

pub(super) fn latest_checkpoint(items: &[RolloutItem]) -> CheckpointMetadata {
    items
        .iter()
        .rev()
        .find_map(|item| match item {
            RolloutItem::Compacted(compacted) => Some(CheckpointMetadata {
                id: compacted.window_id.clone(),
                revision: compacted.window_number,
            }),
            _ => None,
        })
        .unwrap_or_default()
}

pub(super) fn latest_replacement_items(items: &[RolloutItem]) -> Vec<ResponseItem> {
    items
        .iter()
        .rev()
        .find_map(|item| match item {
            RolloutItem::Compacted(compacted) => compacted.replacement_history.as_ref(),
            _ => None,
        })
        .map(|items| {
            items
                .iter()
                .take(MAX_REPLACEMENT_ITEMS)
                .map(|item| item.item.clone())
                .collect()
        })
        .unwrap_or_default()
}

pub(super) fn latest_turn_id(items: &[RolloutItem]) -> Option<String> {
    items.iter().rev().find_map(|item| match item {
        RolloutItem::TurnContext(context) => context.turn_id.clone(),
        RolloutItem::ResponseItem(item) => item.item.turn_id().map(str::to_string),
        RolloutItem::InterAgentCommunication(message) => message
            .internal_chat_message_metadata_passthrough
            .as_ref()
            .and_then(|metadata| metadata.turn_id.clone()),
        _ => None,
    })
}

pub(super) fn latest_token_usage(items: &[RolloutItem]) -> Option<TokenUsage> {
    items.iter().rev().find_map(|item| match item {
        RolloutItem::EventMsg(EventMsg::TokenCount(event)) => event
            .info
            .as_ref()
            .map(|info| info.total_token_usage.clone()),
        _ => None,
    })
}

pub(crate) fn persisted_parent_thread_id(
    items: &[RolloutItem],
) -> Option<codex_protocol::ThreadId> {
    items.iter().find_map(|item| match item {
        RolloutItem::SessionMeta(line) => line.meta.parent_thread_id,
        _ => None,
    })
}

/// Translate the rollout-level E6 fork boundary into a model-item boundary.
///
/// Fork metrics count persisted rollout records, while the inspection list counts only response
/// items after reconstruction. Compaction replacement histories are expanded because their
/// response envelopes become model-visible history; metadata/events remain outside the prompt.
pub(super) fn prompt_item_count_through_rollout_boundary(
    items: &[RolloutItem],
    rollout_boundary: usize,
) -> usize {
    items
        .iter()
        .take(rollout_boundary)
        .map(|item| match item {
            RolloutItem::ResponseItem(_) | RolloutItem::InterAgentCommunication(_) => 1,
            RolloutItem::Compacted(compacted) => {
                compacted.replacement_history.as_ref().map_or(0, Vec::len)
            }
            RolloutItem::SessionMeta(_)
            | RolloutItem::InterAgentCommunicationMetadata { .. }
            | RolloutItem::TurnContext(_)
            | RolloutItem::WorldState(_)
            | RolloutItem::SecurityRiskScore(_)
            | RolloutItem::RealtimeItem(_)
            | RolloutItem::RetainedContext(_)
            | RolloutItem::TokenUsageRecord(_)
            | RolloutItem::EventMsg(_) => 0,
        })
        .sum()
}

pub(super) async fn load_persisted_items(session: &Session) -> Option<Vec<RolloutItem>> {
    session
        .services
        .thread_store
        .load_latest_model_context(LoadThreadHistoryParams {
            thread_id: session.thread_id(),
            include_archived: true,
        })
        .await
        .ok()
        .map(|context| context.items)
}
