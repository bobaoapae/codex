//! FORK extension: read access to the plans persisted by Plan mode.

use crate::JsonSchema;
use crate::TS;
use codex_experimental_api_macros::ExperimentalApi;
use serde::Deserialize;
use serde::Serialize;

/// Lifecycle of a persisted plan revision.
#[derive(
    Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, JsonSchema, TS, ExperimentalApi,
)]
#[serde(rename_all = "camelCase")]
#[ts(rename_all = "camelCase", export_to = "v2/")]
pub enum PlanLifecycle {
    /// Mutable plan draft.
    Draft,
    /// Current immutable approved snapshot.
    Approved,
    /// Immutable approved snapshot superseded by a later revision.
    Superseded,
}

/// A stable reference to one immutable approved plan snapshot.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema, TS, ExperimentalApi)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct ApprovedPlanRef {
    pub id: String,
    pub revision: u32,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema, TS, ExperimentalApi)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct PlanSummary {
    pub id: String,
    pub title: String,
    pub path: String,
    pub thread_id: Option<String>,
    pub turn_id: Option<String>,
    pub cwd: Option<String>,
    pub model: Option<String>,
    #[ts(type = "number")]
    pub created_at: i64,
    #[ts(type = "number")]
    pub updated_at: i64,
    pub revision: u32,
    pub lifecycle: PlanLifecycle,
}

#[derive(
    Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema, TS, Default, ExperimentalApi,
)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct PlanListParams {
    #[ts(optional = nullable)]
    pub cursor: Option<String>,
    #[ts(optional = nullable)]
    pub limit: Option<u32>,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema, TS, ExperimentalApi)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct PlanListResponse {
    pub data: Vec<PlanSummary>,
    pub next_cursor: Option<String>,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema, TS, ExperimentalApi)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct PlanReadParams {
    pub id: String,
    /// Select an immutable approved snapshot. Omission preserves the legacy draft read.
    #[ts(optional = nullable)]
    pub revision: Option<u32>,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema, TS, ExperimentalApi)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct PlanReadResponse {
    pub plan: PlanSummary,
    pub markdown: String,
}

/// Approve the current draft revision using optimistic concurrency.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema, TS, ExperimentalApi)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct PlanApproveParams {
    pub id: String,
    pub expected_revision: u32,
}

/// Result of approving a draft, including the immutable pinned snapshot reference.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema, TS, ExperimentalApi)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct PlanApproveResponse {
    pub plan: PlanSummary,
    pub approved_plan: ApprovedPlanRef,
}
