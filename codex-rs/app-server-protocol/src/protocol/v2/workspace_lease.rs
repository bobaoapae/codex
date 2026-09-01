//! Experimental app-server contracts for durable workspace path leases.
//!
//! Lease tokens are deliberately confined to grant responses and release
//! requests. List/read projections contain only display-safe lease metadata.

use crate::JsonSchema;
use crate::TS;
use codex_experimental_api_macros::ExperimentalApi;
use serde::Deserialize;
use serde::Serialize;

/// Access requested for a workspace lease.
#[derive(
    Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, JsonSchema, TS, ExperimentalApi,
)]
#[serde(rename_all = "lowercase")]
#[ts(rename_all = "lowercase", export_to = "v2/")]
pub enum WorkspaceLeaseMode {
    Read,
    Write,
}

/// Durable lifecycle state of a workspace lease.
#[derive(
    Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, JsonSchema, TS, ExperimentalApi,
)]
#[serde(rename_all = "lowercase")]
#[ts(rename_all = "lowercase", export_to = "v2/")]
pub enum WorkspaceLeaseState {
    Active,
    Released,
    Expired,
    Recoverable,
}

/// Display-safe summary of one workspace lease.
///
/// The normalized paths are display values only. Comparison keys, lease
/// tokens, and other fencing internals are never exposed by this type.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema, TS, ExperimentalApi)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct WorkspaceLease {
    pub lease_id: String,
    pub root_thread_id: String,
    pub owner_thread_id: String,
    pub normalized_paths: Vec<String>,
    pub mode: WorkspaceLeaseMode,
    pub state: WorkspaceLeaseState,
    #[ts(type = "number")]
    pub generation: i64,
    pub environment_id: Option<String>,
    /// Unix timestamp in seconds when the lease was issued.
    #[ts(type = "number")]
    pub issued_at: i64,
    /// Unix timestamp in seconds when the lease expires, if bounded.
    #[ts(type = "number | null")]
    pub expires_at: Option<i64>,
    /// Unix timestamp in seconds when the lease was explicitly released.
    #[ts(type = "number | null")]
    pub released_at: Option<i64>,
}

/// One newly granted lease and its fencing token.
///
/// Tokens are returned only by `workspaceLease/grant`; they are not part of
/// [`WorkspaceLease`] and therefore cannot appear in list responses.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema, TS, ExperimentalApi)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct WorkspaceLeaseGrant {
    pub lease: WorkspaceLease,
    pub token: String,
}

/// List workspace leases for one root using bounded keyset pagination.
#[derive(
    Serialize, Deserialize, Debug, Clone, Default, PartialEq, Eq, JsonSchema, TS, ExperimentalApi,
)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct WorkspaceLeaseListParams {
    pub root_thread_id: String,
    #[ts(optional = nullable)]
    pub owner_thread_id: Option<String>,
    #[ts(optional = nullable)]
    pub path: Option<String>,
    #[ts(optional = nullable)]
    pub cursor: Option<String>,
    #[ts(optional = nullable)]
    pub limit: Option<u32>,
}

/// A page of display-safe workspace lease summaries.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema, TS, ExperimentalApi)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct WorkspaceLeaseListResponse {
    pub data: Vec<WorkspaceLease>,
    pub next_cursor: Option<String>,
}

/// Atomically acquire one or more normalized workspace paths.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema, TS, ExperimentalApi)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct WorkspaceLeaseGrantParams {
    pub root_thread_id: String,
    pub owner_thread_id: String,
    /// Absolute paths. The server normalizes and validates them before claim.
    pub paths: Vec<String>,
    pub mode: WorkspaceLeaseMode,
    /// Requested duration in seconds; the server applies its bounded maximum.
    #[ts(optional = nullable)]
    pub ttl_seconds: Option<u64>,
    #[ts(optional = nullable)]
    pub environment_id: Option<String>,
}

/// Newly granted leases and their fencing tokens.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema, TS, ExperimentalApi)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct WorkspaceLeaseGrantResponse {
    pub leases: Vec<WorkspaceLeaseGrant>,
}

/// Release one lease using its token and current generation fence.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema, TS, ExperimentalApi)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct WorkspaceLeaseReleaseParams {
    pub root_thread_id: String,
    pub lease_id: String,
    pub token: String,
    #[ts(type = "number")]
    pub generation: i64,
}

/// Released lease summary. The fencing token is intentionally omitted.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema, TS, ExperimentalApi)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct WorkspaceLeaseReleaseResponse {
    pub lease: WorkspaceLease,
}
