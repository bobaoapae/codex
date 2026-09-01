//! Experimental app-server contracts for reading persisted artifacts.
//!
//! Artifact identity is server-owned and opaque. The read boundary accepts
//! only that identity and a bounded continuation cursor; callers cannot
//! select a path or thread as an alternate authority.

use crate::JsonSchema;
use crate::TS;
use codex_experimental_api_macros::ExperimentalApi;
use serde::Deserialize;
use serde::Serialize;

/// Allowlisted metadata returned alongside an artifact payload chunk.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema, TS, ExperimentalApi)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct ArtifactMetadata {
    pub artifact_id: String,
    pub thread_id: String,
    pub artifact_type: String,
    pub identity_key: String,
    /// Unix timestamp in seconds when the artifact was attached.
    #[ts(type = "number")]
    pub created_at: i64,
}

/// Read one bounded UTF-8 JSON chunk from a server-owned artifact.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema, TS, ExperimentalApi)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct ArtifactReadParams {
    /// Opaque artifact identity returned by the state store.
    pub artifact_id: String,
    /// Cursor returned by a previous read for this same artifact.
    #[ts(optional = nullable)]
    pub cursor: Option<String>,
    /// Requested chunk size in bytes. The server defaults and caps this value.
    #[ts(optional = nullable)]
    pub limit: Option<u32>,
}

/// One artifact payload chunk and its allowlisted metadata.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema, TS, ExperimentalApi)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct ArtifactReadResponse {
    pub artifact: ArtifactMetadata,
    /// Consecutive UTF-8 bytes from the canonical serialized JSON payload.
    pub chunk: String,
    /// Opaque cursor for the next chunk, or `null` when the payload is complete.
    pub next_cursor: Option<String>,
    /// Total number of UTF-8 bytes in the serialized JSON payload.
    #[ts(type = "number")]
    pub total_bytes: u64,
}
