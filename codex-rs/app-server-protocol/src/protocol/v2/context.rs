//! Experimental model-context inspection API.

use crate::JsonSchema;
use crate::TS;
use codex_experimental_api_macros::ExperimentalApi;
use codex_protocol::protocol::RuntimeBuildInfo as CoreRuntimeBuildInfo;
use serde::Deserialize;
use serde::Serialize;

/// Parameters for inspecting one thread's model context.
///
/// The app-server chooses a live snapshot for a loaded thread and a cold
/// snapshot when a cold source is available. `include_preview` is deliberately
/// a non-optional boolean so omission has the safe default of `false` while
/// responses still preserve every optional value as JSON `null`.
#[derive(
    Serialize, Deserialize, Debug, Clone, Default, PartialEq, Eq, JsonSchema, TS, ExperimentalApi,
)]
#[serde(rename_all = "camelCase")]
#[ts(rename_all = "camelCase", export_to = "v2/")]
pub struct ContextInspectParams {
    pub thread_id: String,
    #[serde(default)]
    pub include_preview: bool,
}

/// Runtime build identity associated with a context snapshot.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema, TS, ExperimentalApi)]
#[serde(rename_all = "camelCase")]
#[ts(rename_all = "camelCase", export_to = "v2/")]
pub struct ContextRuntimeBuildInfo {
    pub version: String,
    pub build_commit: String,
    pub target: String,
}

impl From<CoreRuntimeBuildInfo> for ContextRuntimeBuildInfo {
    fn from(value: CoreRuntimeBuildInfo) -> Self {
        Self {
            version: value.version,
            build_commit: value.build_commit,
            target: value.target,
        }
    }
}

/// Selects whether an inspection came from a live, speculative, or cold view.
#[derive(
    Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, JsonSchema, TS, ExperimentalApi,
)]
#[serde(rename_all = "camelCase")]
#[ts(rename_all = "camelCase", export_to = "v2/")]
pub enum ContextSnapshotKind {
    Live,
    Speculative,
    Cold,
}

/// Logical provenance for one model-visible item or prompt group.
#[derive(
    Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, JsonSchema, TS, ExperimentalApi,
)]
#[serde(rename_all = "camelCase")]
#[ts(rename_all = "camelCase", export_to = "v2/")]
pub enum ContextLogicalOrigin {
    BaseInstructions,
    ThreadContext,
    TurnContext,
    WorldState,
    InheritedHistory,
    NewOutput,
    ToolOutput,
    CompactionReplacement,
    Derived,
    Unknown,
}

/// Audience visibility for an inspected item or group.
#[derive(
    Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, JsonSchema, TS, ExperimentalApi,
)]
#[serde(rename_all = "camelCase")]
#[ts(rename_all = "camelCase", export_to = "v2/")]
pub enum ContextVisibility {
    Model,
    User,
    Internal,
    Unknown,
}

/// Whether an item is known to survive the next compaction.
#[derive(
    Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, JsonSchema, TS, ExperimentalApi,
)]
#[serde(rename_all = "lowercase")]
#[ts(rename_all = "lowercase", export_to = "v2/")]
pub enum CompactionSurvival {
    True,
    False,
    Unknown,
}

/// A prompt component accounted for outside the response-item list.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema, TS, ExperimentalApi)]
#[serde(rename_all = "camelCase")]
#[ts(rename_all = "camelCase", export_to = "v2/")]
pub struct ContextInspectionGroup {
    pub index: usize,
    pub item_count: usize,
    pub role: String,
    pub content_kind: String,
    pub logical_origin: ContextLogicalOrigin,
    pub visibility: ContextVisibility,
    #[ts(type = "number")]
    pub estimated_tokens: i64,
    pub serialized_bytes: usize,
    pub survives_compaction: CompactionSurvival,
    pub encrypted: bool,
    pub duplicate_group: Option<String>,
    pub duplicate_count: usize,
    pub preview: Option<String>,
}

/// One response item in the model-visible prompt.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema, TS, ExperimentalApi)]
#[serde(rename_all = "camelCase")]
#[ts(rename_all = "camelCase", export_to = "v2/")]
pub struct ContextInspectionItem {
    pub index: usize,
    pub role: String,
    pub content_kind: String,
    pub logical_origin: ContextLogicalOrigin,
    pub visibility: ContextVisibility,
    #[ts(type = "number")]
    pub estimated_tokens: i64,
    pub serialized_bytes: usize,
    pub survives_compaction: CompactionSurvival,
    pub duplicate_group: Option<String>,
    pub duplicate_count: usize,
    pub encrypted: bool,
    pub preview: Option<String>,
}

/// Read-only projection of a model context and its provenance.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema, TS, ExperimentalApi)]
#[serde(rename_all = "camelCase")]
#[ts(rename_all = "camelCase", export_to = "v2/")]
pub struct ContextInspection {
    pub thread_id: String,
    pub turn_id: Option<String>,
    pub snapshot_kind: ContextSnapshotKind,
    pub partial: bool,
    pub item_count: usize,
    #[ts(type = "number | null")]
    pub estimated_prompt_tokens: Option<i64>,
    #[ts(type = "number | null")]
    pub estimated_active_tokens: Option<i64>,
    #[ts(type = "number | null")]
    pub estimated_context_window_tokens: Option<i64>,
    #[ts(type = "number | null")]
    pub cached_input_tokens: Option<i64>,
    #[ts(type = "number | null")]
    pub uncached_input_tokens: Option<i64>,
    #[ts(type = "number | null")]
    pub cache_write_input_tokens: Option<i64>,
    pub runtime_build_info: Option<ContextRuntimeBuildInfo>,
    pub config_layer_revision: Option<String>,
    pub runtime_feature_revision: Option<String>,
    pub persisted_runtime_build_info: Option<ContextRuntimeBuildInfo>,
    pub persisted_config_layer_revision: Option<String>,
    pub persisted_runtime_feature_revision: Option<String>,
    pub stale: bool,
    pub window_id: Option<String>,
    pub context_window_id: Option<String>,
    #[ts(type = "number | null")]
    pub window_number: Option<u64>,
    pub first_window_id: Option<String>,
    pub previous_window_id: Option<String>,
    pub checkpoint_id: Option<String>,
    #[ts(type = "number | null")]
    pub checkpoint_revision: Option<u64>,
    pub base_instructions: ContextInspectionGroup,
    pub tools: ContextInspectionGroup,
    pub items: Vec<ContextInspectionItem>,
}

/// Successful response from `context/inspect`.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema, TS, ExperimentalApi)]
#[serde(rename_all = "camelCase")]
#[ts(rename_all = "camelCase", export_to = "v2/")]
pub struct ContextInspectResponse {
    pub context: ContextInspection,
}

#[cfg(test)]
#[path = "context_tests.rs"]
mod tests;
