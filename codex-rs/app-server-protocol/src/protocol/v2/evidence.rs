//! Experimental app-server contracts for host-owned evidence receipts.
//!
//! Evidence is intentionally a metadata-only view of the canonical
//! `receipt.attached` extension item.  Rollout bytes, tool arguments, stdout,
//! and other provider payloads are never part of these wire types.

use crate::JsonSchema;
use crate::TS;
use codex_experimental_api_macros::ExperimentalApi;
use serde::Deserialize;
use serde::Serialize;
use serde_json::Value as JsonValue;
use std::collections::BTreeMap;

/// Stable status vocabulary for host-attached evidence.
#[derive(
    Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, JsonSchema, TS, ExperimentalApi,
)]
#[serde(rename_all = "lowercase")]
#[ts(rename_all = "lowercase", export_to = "v2/")]
pub enum EvidenceStatus {
    Pass,
    Fail,
    Blocked,
    Inconclusive,
    Informational,
}

/// A bounded reference to a canonical rollout item or artifact.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema, TS, ExperimentalApi)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct EvidenceReference {
    pub kind: String,
    pub id: String,
}

/// Metadata-only receipt returned by the evidence APIs.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, JsonSchema, TS, ExperimentalApi)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct Evidence {
    pub receipt_id: String,
    pub schema_version: u64,
    /// Extension-defined kind. Unknown kinds are preserved verbatim.
    pub kind: String,
    pub subject: String,
    pub status: EvidenceStatus,
    pub thread_id: Option<String>,
    pub turn_id: Option<String>,
    pub job_id: Option<String>,
    pub plan_snapshot_id: Option<String>,
    /// Unix timestamp in seconds when the receipt was created.
    #[ts(type = "number")]
    pub created_at: i64,
    pub source: String,
    pub provenance: Option<JsonValue>,
    pub tags: BTreeMap<String, String>,
    pub refs: Vec<EvidenceReference>,
    /// Bounded, redacted metadata supplied by the trusted host integration.
    pub metadata: Option<JsonValue>,
}

/// List host-owned evidence receipts with bounded keyset pagination.
#[derive(
    Serialize, Deserialize, Debug, Clone, Default, PartialEq, Eq, JsonSchema, TS, ExperimentalApi,
)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct EvidenceListParams {
    #[ts(optional = nullable)]
    pub cursor: Option<String>,
    #[ts(optional = nullable)]
    pub limit: Option<u32>,
    #[ts(optional = nullable)]
    pub thread_id: Option<String>,
    #[ts(optional = nullable)]
    pub job_id: Option<String>,
    #[ts(optional = nullable)]
    pub plan_snapshot_id: Option<String>,
    #[ts(optional = nullable)]
    pub status: Option<EvidenceStatus>,
    #[ts(optional = nullable)]
    pub kind: Option<String>,
}

/// A page of evidence receipts.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, JsonSchema, TS, ExperimentalApi)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct EvidenceListResponse {
    pub data: Vec<Evidence>,
    pub next_cursor: Option<String>,
}

/// Attach one trusted, metadata-only evidence receipt to a thread rollout.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, JsonSchema, TS, ExperimentalApi)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct EvidenceAttachParams {
    pub thread_id: String,
    pub receipt_id: String,
    pub schema_version: u64,
    /// Extension-defined kind. Unknown kinds are accepted and preserved.
    pub kind: String,
    pub subject: String,
    pub status: EvidenceStatus,
    pub source: String,
    #[ts(optional = nullable)]
    pub turn_id: Option<String>,
    #[ts(optional = nullable)]
    pub job_id: Option<String>,
    #[ts(optional = nullable)]
    pub plan_snapshot_id: Option<String>,
    /// Optional Unix timestamp in seconds. The server supplies one when omitted.
    #[ts(optional = nullable)]
    pub created_at: Option<i64>,
    #[ts(optional = nullable)]
    pub provenance: Option<JsonValue>,
    #[ts(optional = nullable)]
    pub tags: Option<BTreeMap<String, String>>,
    #[ts(optional = nullable)]
    pub refs: Option<Vec<EvidenceReference>>,
    #[ts(optional = nullable)]
    pub metadata: Option<JsonValue>,
}

/// Result of attaching evidence to a canonical rollout.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, JsonSchema, TS, ExperimentalApi)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct EvidenceAttachResponse {
    pub evidence: Evidence,
}

/// Explicit selection for a redacted evidence export.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, JsonSchema, TS, ExperimentalApi)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct EvidenceExportParams {
    /// Non-empty explicit selection; the server never treats omission as "all".
    pub receipt_ids: Vec<String>,
}

/// Redacted metadata-only evidence export.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, JsonSchema, TS, ExperimentalApi)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct EvidenceExportResponse {
    pub data: Vec<Evidence>,
    /// Whether the export sanitizer removed any reserved raw-data fields.
    pub redacted: bool,
    /// Bounded number of removed fields across the selected receipts.
    pub redacted_count: u32,
}
