//! Public, read-only model-context inspection value types.

use codex_protocol::ThreadId;
use codex_protocol::protocol::RuntimeBuildInfo;
use serde::Deserialize;
use serde::Serialize;

/// Selects the source used for a context inspection.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum ContextInspectionMode {
    /// Inspect the currently loaded session snapshot without refreshing contributors.
    #[default]
    Loaded,
    /// Reconstruct the latest persisted model context without changing the loaded session.
    Cold,
}

/// Describes whether an inspection was taken from a live or reconstructed view.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum ContextSnapshotKind {
    /// A request-scoped context retained by an active turn.
    Live,
    /// A bounded view assembled without a request-scoped dynamic context.
    Speculative,
    /// A view rebuilt from persisted rollout history.
    Cold,
}

/// Logical provenance for one model-visible item or prompt group.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
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
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum ContextVisibility {
    /// The provider receives this value as part of the model request.
    Model,
    /// The value is retained for a client-facing view but is not model input.
    User,
    /// The value is harness state rather than model-visible content.
    Internal,
    /// The source did not retain enough information to decide.
    Unknown,
}

/// Whether the current item is known to survive the next compaction.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum CompactionSurvival {
    True,
    False,
    Unknown,
}

/// Aggregate provider cache accounting. Values are deliberately not attached to individual
/// items because provider cache boundaries are prefix-level, not item-level claims.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ContextCacheMetrics {
    pub cached_input_tokens: Option<i64>,
    pub uncached_input_tokens: Option<i64>,
    pub cache_write_input_tokens: Option<i64>,
}

/// A prompt component accounted for outside the response-item list.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ContextInspectionGroup {
    pub index: usize,
    pub item_count: usize,
    pub role: String,
    pub content_kind: String,
    pub logical_origin: ContextLogicalOrigin,
    pub visibility: ContextVisibility,
    pub estimated_tokens: i64,
    pub serialized_bytes: usize,
    pub survives_compaction: CompactionSurvival,
    pub encrypted: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub duplicate_group: Option<String>,
    pub duplicate_count: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub preview: Option<String>,
}

/// One response item in the model-visible prompt.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ContextInspectionItem {
    pub index: usize,
    pub role: String,
    pub content_kind: String,
    pub logical_origin: ContextLogicalOrigin,
    pub visibility: ContextVisibility,
    pub estimated_tokens: i64,
    pub serialized_bytes: usize,
    pub survives_compaction: CompactionSurvival,
    /// Opaque local duplicate identity. It never contains item content.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub duplicate_group: Option<String>,
    pub duplicate_count: usize,
    pub encrypted: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub preview: Option<String>,
}

/// Public read-only projection of a model context and its provenance.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ContextInspection {
    pub thread_id: ThreadId,
    pub turn_id: Option<String>,
    pub snapshot_kind: ContextSnapshotKind,
    pub partial: bool,
    pub item_count: usize,
    pub estimated_prompt_tokens: Option<i64>,
    pub estimated_active_tokens: Option<i64>,
    pub estimated_context_window_tokens: Option<i64>,
    pub cached_input_tokens: Option<i64>,
    pub uncached_input_tokens: Option<i64>,
    pub cache_write_input_tokens: Option<i64>,
    /// Current runtime identity and effective-config revisions.
    pub runtime_build_info: Option<RuntimeBuildInfo>,
    pub config_layer_revision: Option<String>,
    pub runtime_feature_revision: Option<String>,
    /// Revisions read from the persisted session/checkpoint metadata.
    pub persisted_runtime_build_info: Option<RuntimeBuildInfo>,
    pub persisted_config_layer_revision: Option<String>,
    pub persisted_runtime_feature_revision: Option<String>,
    pub stale: bool,
    /// Logical context-window identity (`thread_id:window_number`).
    pub window_id: Option<String>,
    /// UUID identity sent in response metadata for the active context window.
    pub context_window_id: Option<String>,
    pub window_number: Option<u64>,
    pub first_window_id: Option<String>,
    pub previous_window_id: Option<String>,
    /// The latest persisted compaction checkpoint, when the source exposes one.
    pub checkpoint_id: Option<String>,
    pub checkpoint_revision: Option<u64>,
    pub base_instructions: ContextInspectionGroup,
    pub tools: ContextInspectionGroup,
    pub items: Vec<ContextInspectionItem>,
}

/// Options controlling a read-only context inspection.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ContextInspectionOptions {
    #[serde(default)]
    pub mode: ContextInspectionMode,
    /// Include bounded redacted previews for safe text items.
    #[serde(default)]
    pub include_preview: bool,
    /// Optional turn identity to associate with a cold or speculative view.
    pub turn_id: Option<String>,
}
