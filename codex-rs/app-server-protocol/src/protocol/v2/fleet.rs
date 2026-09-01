//! Experimental app-server contracts for durable agent-fleet lifecycle control.

use crate::JsonSchema;
use crate::TS;
use codex_experimental_api_macros::ExperimentalApi;
use serde::Deserialize;
use serde::Serialize;

/// Runtime state of one member in an agent fleet.
#[derive(
    Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, JsonSchema, TS, ExperimentalApi,
)]
#[serde(rename_all = "camelCase")]
#[ts(rename_all = "camelCase", export_to = "v2/")]
pub enum FleetMemberState {
    Running,
    WaitingForTool,
    WaitingForApproval,
    WaitingForUser,
    Idle,
    Suspended,
    Closed,
    Failed,
}

/// Exclusive lifecycle operation applied to a fleet root.
#[derive(
    Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, JsonSchema, TS, ExperimentalApi,
)]
#[serde(rename_all = "camelCase")]
#[ts(rename_all = "camelCase", export_to = "v2/")]
pub enum FleetOperationKind {
    Suspend,
    Resume,
    Close,
}

/// Durable status of an exclusive fleet operation.
#[derive(
    Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, JsonSchema, TS, ExperimentalApi,
)]
#[serde(rename_all = "camelCase")]
#[ts(rename_all = "camelCase", export_to = "v2/")]
pub enum FleetOperationStatus {
    Running,
    Recoverable,
    Complete,
    Failed,
}

/// One member visible in a fleet status response.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema, TS, ExperimentalApi)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct FleetMember {
    pub member_id: String,
    pub thread_id: Option<String>,
    pub run_id: Option<String>,
    pub parent_member_id: Option<String>,
    pub state: FleetMemberState,
    #[ts(type = "number")]
    pub depth: i64,
    #[ts(type = "number")]
    pub order_index: i64,
    #[ts(type = "number")]
    pub updated_at: i64,
}

/// Durable operation metadata returned with fleet lifecycle results.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema, TS, ExperimentalApi)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct FleetOperation {
    pub operation_id: String,
    pub root_thread_id: String,
    pub kind: FleetOperationKind,
    pub status: FleetOperationStatus,
    #[ts(type = "number")]
    pub expected_generation: i64,
    #[ts(type = "number")]
    pub new_generation: i64,
    #[ts(type = "number")]
    pub expected_member_count: u32,
    #[ts(type = "number")]
    pub result_count: u32,
    pub partial: bool,
    #[ts(type = "number")]
    pub created_at: i64,
    #[ts(type = "number")]
    pub updated_at: i64,
}

/// Result for one member during a fleet lifecycle operation.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema, TS, ExperimentalApi)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct FleetResult {
    pub operation_id: String,
    pub member_id: String,
    pub thread_id: Option<String>,
    pub run_id: Option<String>,
    pub requested_state: FleetMemberState,
    pub previous_state: Option<FleetMemberState>,
    pub final_state: Option<FleetMemberState>,
    pub success: bool,
    pub error: Option<String>,
    #[ts(type = "number")]
    pub depth: i64,
    #[ts(type = "number")]
    pub order_index: i64,
    #[ts(type = "number")]
    pub updated_at: i64,
}

/// Read the current fleet tree with bounded keyset pagination.
#[derive(
    Serialize, Deserialize, Debug, Clone, Default, PartialEq, Eq, JsonSchema, TS, ExperimentalApi,
)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct AgentFleetStatusParams {
    pub root_thread_id: String,
    #[ts(optional = nullable)]
    pub cursor: Option<String>,
    #[ts(optional = nullable)]
    pub limit: Option<u32>,
}

/// Current fleet tree and root lifecycle metadata.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema, TS, ExperimentalApi)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct AgentFleetStatusResponse {
    pub root_thread_id: String,
    #[ts(type = "number")]
    pub generation: i64,
    pub sealed: bool,
    pub operation_id: Option<String>,
    pub data: Vec<FleetMember>,
    pub next_cursor: Option<String>,
}

/// Begin suspending a fleet tree at an expected root generation.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema, TS, ExperimentalApi)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct AgentFleetSuspendParams {
    pub root_thread_id: String,
    #[ts(type = "number")]
    pub expected_generation: i64,
}

/// Suspend operation admission and per-member results.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema, TS, ExperimentalApi)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct AgentFleetSuspendResponse {
    pub root_thread_id: String,
    #[ts(type = "number")]
    pub generation: i64,
    pub sealed: bool,
    pub operation_id: Option<String>,
    pub results: Vec<FleetResult>,
    pub next_cursor: Option<String>,
}

/// Begin resuming a suspended fleet tree at an expected root generation.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema, TS, ExperimentalApi)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct AgentFleetResumeParams {
    pub root_thread_id: String,
    #[ts(type = "number")]
    pub expected_generation: i64,
}

/// Resume operation admission and per-member results.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema, TS, ExperimentalApi)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct AgentFleetResumeResponse {
    pub root_thread_id: String,
    #[ts(type = "number")]
    pub generation: i64,
    pub sealed: bool,
    pub operation_id: Option<String>,
    pub results: Vec<FleetResult>,
    pub next_cursor: Option<String>,
}

/// Begin closing a fleet tree at an expected root generation.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema, TS, ExperimentalApi)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct AgentFleetCloseParams {
    pub root_thread_id: String,
    #[ts(type = "number")]
    pub expected_generation: i64,
}

/// Close operation admission and per-member results.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema, TS, ExperimentalApi)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct AgentFleetCloseResponse {
    pub root_thread_id: String,
    #[ts(type = "number")]
    pub generation: i64,
    pub sealed: bool,
    pub operation_id: Option<String>,
    pub results: Vec<FleetResult>,
    pub next_cursor: Option<String>,
}
