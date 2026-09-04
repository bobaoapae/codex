//! FORK: JSON shapes of the daemon's loopback control API, shared by the daemon
//! and the session-side client.

use crate::chatgpt_web::connector::contract::CallTarget;
use crate::chatgpt_web::connector::contract::ExecTool;
use crate::chatgpt_web::connector::contract::ToolSummary;
use serde::Deserialize;
use serde::Serialize;
use serde_json::Value;

/// `GET /healthz`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HealthResponse {
    pub ok: bool,
    pub pid: u32,
    pub version: String,
    /// Host of the public MCP endpoint, never the secret path.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub public_url: Option<String>,
    pub registry_status: String,
    /// FORK: the registry's own words for why it failed, so the turn-side gate
    /// can say something better than "not ready within 90s".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub registry_reason: Option<String>,
    /// FORK: `FailureKind::label()`; a terminal kind fails the turn at once.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub registry_failure_kind: Option<String>,
    /// FORK: when the daemon will try again, in Unix milliseconds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub registry_retry_at_ms: Option<u64>,
    /// FORK: the watcher has stopped retrying on its own.
    #[serde(default)]
    pub registry_parked: bool,
    pub tunnel_state: String,
    pub sessions: usize,
    pub active_turns: usize,
}

/// FORK: `POST /v1/registry/reconcile` and `POST /v1/registry/refresh`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReconcileResponse {
    pub registry_status: String,
    pub detail: crate::chatgpt_web::connector::daemon::state::RegistryStatus,
}

/// `POST /v1/sessions`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RegisterSessionRequest {
    pub codex_pid: u32,
    pub session_id: String,
    pub codex_version: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RegisterSessionResponse {
    pub session_token: String,
    pub poll_url: String,
}

/// `POST /v1/turns`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RegisterTurnRequest {
    pub session_id: String,
    pub turn_token: String,
    pub thread_id: String,
    pub turn_id: String,
    pub ttl_ms: u64,
    pub tools: Vec<ToolSummary>,
    pub exec_tool: ExecTool,
    /// `true` when the turn announces a free-form `apply_patch` tool.
    #[serde(default)]
    pub apply_patch: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RegisterTurnResponse {
    pub registry_status: String,
    pub tunnel_state: String,
}

/// `DELETE /v1/turns/{turn_token}`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct EndTurnRequest {
    #[serde(default)]
    pub reason: Option<String>,
}

/// One call the owning session must run.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PendingCallWire {
    pub call_id: String,
    pub target: CallTarget,
    /// Unix milliseconds by which the daemon stops waiting for the result.
    pub deadline_ms: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CallBatchWire {
    pub seq: u64,
    pub turn_token: String,
    pub calls: Vec<PendingCallWire>,
}

/// `GET /v1/sessions/{sid}/calls?after=&wait_ms=`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CallsResponse {
    /// Highest sequence number delivered so far; echo it as `after`.
    pub seq: u64,
    pub batches: Vec<CallBatchWire>,
}

#[derive(Debug, Clone, Deserialize, Serialize, Default)]
pub struct CallsQuery {
    #[serde(default)]
    pub after: u64,
    #[serde(default)]
    pub wait_ms: Option<u64>,
}

/// A piece of tool output going back to ChatGPT.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ResultContent {
    Text {
        text: String,
    },
    Image {
        /// Base64 payload.
        data: String,
        mime_type: String,
    },
}

/// `POST /v1/calls/{call_id}/result`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CallResultRequest {
    pub session_id: String,
    #[serde(default)]
    pub content: Vec<ResultContent>,
    #[serde(default)]
    pub is_error: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub structured: Option<Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ErrorResponse {
    pub error: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OkResponse {
    pub ok: bool,
}
