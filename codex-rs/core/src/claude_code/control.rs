//! FORK: the Claude Code CLI's stdio control protocol.
//!
//! Under `--input-format stream-json` the CLI multiplexes two conversations on
//! the same pipes. One is the turn itself (`user` in, `assistant`/`result` out).
//! The other is a small bidirectional RPC — `control_request` /
//! `control_response` / `control_cancel_request` — that the CLI uses to ask its
//! host for things it cannot decide alone: whether a tool call may run
//! (`can_use_tool`), what an in-process MCP server replies (`mcp_message`), what
//! the current usage looks like (`get_usage`).
//!
//! Codex used to write one line to stdin and close it, so none of that was
//! reachable. Every "ask" decision the CLI made in `--permission-mode auto` was
//! therefore terminal — the origin of the "dotnet requires approval" dead ends
//! seen in production. Keeping stdin open for the length of the turn and
//! answering these frames is what Phases 4 and 6 build on.
//!
//! The protocol is internal to the CLI and undocumented; every field is optional
//! on the way in, and an unrecognized subtype is answered with an error rather
//! than left hanging. A CLI that does not speak it at all simply never sends a
//! frame, and the feature flag falls back to the previous behavior.

use serde::Deserialize;
use serde_json::Value as JsonValue;
use serde_json::json;
use tokio::sync::mpsc;
use tracing::debug;
use tracing::warn;

/// A control frame the CLI sent us, already classified.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum InboundControl {
    /// The CLI is asking whether one of its tool calls may run.
    CanUseTool(Box<CanUseTool>),
    /// A JSON-RPC message addressed to an MCP server we host in-process.
    McpMessage { server: String, message: JsonValue },
    /// A lifecycle hook callback. Acknowledged, not acted on.
    HookCallback { callback_id: Option<String> },
    /// Anything else: answered with an error so the CLI stops waiting.
    Unknown { subtype: String },
}

/// The CLI's `can_use_tool` request.
///
/// Only `tool_name` and `input` are reliably present; everything else depends on
/// the CLI version and on which rule triggered the prompt.
#[derive(Debug, Clone, PartialEq, Deserialize, Default)]
pub(crate) struct CanUseTool {
    /// Name as the CLI knows it: `Bash`, `Edit`, `mcp__server__tool`, …
    #[serde(default)]
    pub(crate) tool_name: String,
    /// The arguments the CLI intends to run the tool with.
    #[serde(default)]
    pub(crate) input: JsonValue,
    /// Identifier of the `tool_use` block this decision belongs to.
    #[serde(default)]
    pub(crate) tool_use_id: Option<String>,
    /// Permission updates the CLI offers to remember if we approve.
    #[serde(default)]
    pub(crate) permission_suggestions: Option<JsonValue>,
    /// Why the CLI is asking, when it says.
    #[serde(default)]
    pub(crate) decision_reason: Option<JsonValue>,
    /// Path that tripped a filesystem rule, when there is one.
    #[serde(default)]
    pub(crate) blocked_path: Option<String>,
}

/// Our answer to a `can_use_tool` request.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum ToolPermissionDecision {
    Allow {
        /// Arguments to run instead of the requested ones. `None` keeps them.
        updated_input: Option<JsonValue>,
        /// Permission updates the CLI should remember, so an approval that
        /// covers a whole session is not asked again on the next call.
        updated_permissions: Option<JsonValue>,
    },
    Deny {
        message: String,
        /// Whether the CLI should abandon the turn rather than continue without
        /// this tool. Always false in practice: a denied command is information
        /// the agent can work around, while an interrupt loses the turn.
        interrupt: bool,
    },
}

impl ToolPermissionDecision {
    fn to_payload(&self) -> JsonValue {
        match self {
            Self::Allow {
                updated_input,
                updated_permissions,
            } => {
                let mut payload = json!({ "behavior": "allow" });
                if let Some(updated_input) = updated_input {
                    payload["updatedInput"] = updated_input.clone();
                }
                if let Some(updated_permissions) = updated_permissions {
                    payload["updatedPermissions"] = updated_permissions.clone();
                }
                payload
            }
            Self::Deny { message, interrupt } => json!({
                "behavior": "deny",
                "message": message,
                "interrupt": interrupt,
            }),
        }
    }
}

/// Writes control frames to the CLI's stdin.
///
/// One per attempt. Cloning is deliberate: the reader loop and the host both
/// need to send, and the channel is the only shared state.
#[derive(Debug, Clone)]
pub(crate) struct ControlChannel {
    tx_stdin: mpsc::Sender<String>,
    next_id: std::sync::Arc<std::sync::atomic::AtomicU64>,
}

/// What the CLI said about a request we sent.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct ControlOutcome {
    pub(crate) request_id: String,
    pub(crate) result: Result<JsonValue, String>,
}

impl ControlChannel {
    pub(crate) fn new(tx_stdin: mpsc::Sender<String>) -> Self {
        Self {
            tx_stdin,
            next_id: std::sync::Arc::new(std::sync::atomic::AtomicU64::new(1)),
        }
    }

    fn next_request_id(&self) -> String {
        let id = self
            .next_id
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        format!("codex-{id}")
    }

    /// Sends a control request without waiting for its answer.
    ///
    /// Deliberately fire-and-forget. Only the task reading the CLI's stdout can
    /// deliver a `control_response`, so a caller that blocks on one either *is*
    /// that task — deadlocking itself — or runs before that task has started and
    /// waits out a timeout for a response nobody will route. Callers send the
    /// request and react when [`Self::resolve_response`] reports the outcome.
    ///
    /// Returns the request id, so the caller can recognize the answer.
    pub(crate) async fn send_request(&self, subtype: &str, payload: JsonValue) -> Option<String> {
        let request_id = self.next_request_id();
        let mut request = json!({ "subtype": subtype });
        if let JsonValue::Object(fields) = payload {
            for (key, value) in fields {
                request[key] = value;
            }
        }
        let frame = json!({
            "type": "control_request",
            "request_id": request_id,
            "request": request,
        });
        self.send_frame(frame).await.ok()?;
        Some(request_id)
    }

    /// Answers a request the CLI made, with a successful payload.
    pub(crate) async fn respond_success(&self, request_id: &str, response: JsonValue) {
        let frame = json!({
            "type": "control_response",
            "response": {
                "subtype": "success",
                "request_id": request_id,
                "response": response,
            },
        });
        let _ = self.send_frame(frame).await;
    }

    /// Answers a request the CLI made, with an error.
    ///
    /// Always answer: a request left hanging stalls the CLI's own turn until it
    /// times out, which reads to the parent as a wedged agent.
    pub(crate) async fn respond_error(&self, request_id: &str, error: &str) {
        let frame = json!({
            "type": "control_response",
            "response": {
                "subtype": "error",
                "request_id": request_id,
                "error": error,
            },
        });
        let _ = self.send_frame(frame).await;
    }

    /// Answers a `can_use_tool` request with an approval decision.
    pub(crate) async fn respond_tool_permission(
        &self,
        request_id: &str,
        decision: &ToolPermissionDecision,
    ) {
        self.respond_success(request_id, decision.to_payload())
            .await;
    }

    /// Reads an inbound `control_response`.
    ///
    /// Returns which request it answers and whether the CLI could do it; `None`
    /// for a frame that is not a well-formed response.
    pub(crate) fn resolve_response(&self, frame: &JsonValue) -> Option<ControlOutcome> {
        let response = frame.get("response").unwrap_or(frame);
        let request_id = response
            .get("request_id")
            .and_then(JsonValue::as_str)
            .map(str::to_string)?;
        let result = match response.get("subtype").and_then(JsonValue::as_str) {
            Some("error") => {
                let error = response
                    .get("error")
                    .and_then(JsonValue::as_str)
                    .unwrap_or("unknown control error")
                    .to_string();
                debug!("claude_code: control request `{request_id}` failed: {error}");
                Err(error)
            }
            _ => Ok(response.get("response").cloned().unwrap_or(JsonValue::Null)),
        };
        Some(ControlOutcome { request_id, result })
    }

    async fn send_frame(&self, frame: JsonValue) -> Result<(), ()> {
        self.tx_stdin.send(frame.to_string()).await.map_err(|_| ())
    }
}

/// Classifies an inbound `control_request` frame.
///
/// Returns the request id alongside the classification: every branch has to
/// answer, including the ones we refuse.
pub(crate) fn classify_request(frame: &JsonValue) -> Option<(String, InboundControl)> {
    let request_id = frame
        .get("request_id")
        .and_then(JsonValue::as_str)?
        .to_string();
    let request = frame.get("request").unwrap_or(&JsonValue::Null);
    let subtype = request
        .get("subtype")
        .and_then(JsonValue::as_str)
        .unwrap_or_default();

    let classified = match subtype {
        "can_use_tool" => match serde_json::from_value::<CanUseTool>(request.clone()) {
            Ok(can_use_tool) => InboundControl::CanUseTool(Box::new(can_use_tool)),
            Err(err) => {
                warn!("claude_code: malformed can_use_tool request: {err}");
                InboundControl::Unknown {
                    subtype: subtype.to_string(),
                }
            }
        },
        "mcp_message" => InboundControl::McpMessage {
            server: request
                .get("server_name")
                .or_else(|| request.get("server"))
                .and_then(JsonValue::as_str)
                .unwrap_or_default()
                .to_string(),
            message: request.get("message").cloned().unwrap_or(JsonValue::Null),
        },
        "hook_callback" => InboundControl::HookCallback {
            callback_id: request
                .get("callback_id")
                .and_then(JsonValue::as_str)
                .map(str::to_string),
        },
        other => InboundControl::Unknown {
            subtype: other.to_string(),
        },
    };
    Some((request_id, classified))
}

/// The `initialize` payload sent as the first line of stdin.
///
/// `appendSystemPrompt` is how the role's instructions and the subagent protocol
/// reach the child without a command-line flag: Windows caps a command line at
/// roughly 32k characters, and role instructions alone can exceed that.
pub(crate) fn initialize_payload(
    append_system_prompt: Option<&str>,
    sdk_mcp_servers: &[&str],
) -> JsonValue {
    let mut payload = json!({});
    if let Some(prompt) = append_system_prompt
        .map(str::trim)
        .filter(|p| !p.is_empty())
    {
        payload["appendSystemPrompt"] = json!(prompt);
    }
    if !sdk_mcp_servers.is_empty() {
        payload["sdkMcpServers"] = json!(sdk_mcp_servers);
    }
    payload
}

#[cfg(test)]
#[path = "control_tests.rs"]
mod tests;
