//! FORK: a minimal MCP server the Claude CLI hosts in-process.
//!
//! The CLI can run MCP servers that live inside its client rather than in a
//! separate process: `initialize` declares them as `sdkMcpServers`, and every
//! JSON-RPC message for one arrives as an `mcp_message` control request. That is
//! how a Claude child gets to call `send_message`, `wait_agent`, `update_plan`
//! and the session's own MCP tools — with no port, no token and no orphan
//! process to clean up.
//!
//! What this buys, concretely: a child that needs to tell its parent something
//! mid-task now has a way to do it through the ordinary inter-agent channel,
//! instead of reaching for the Desktop's `send_message_to_thread` and producing
//! a "sent from another task" card in the user's own thread.
//!
//! Only the four methods the CLI actually uses are implemented. Anything else is
//! answered with a JSON-RPC "method not found" rather than left hanging.

use serde_json::Value as JsonValue;
use serde_json::json;
use std::sync::Arc;

use super::host::ClaudeHost;

/// Name the bridge is declared under. Tools reach the child as
/// `mcp__codex__<tool>`.
pub(crate) const BRIDGE_SERVER_NAME: &str = "codex";

/// Prefix the CLI must be told to auto-allow, so bridge calls do not each raise
/// a `can_use_tool` question. These tools are already gated by Codex's own
/// allow-list, and asking twice for the same thing is what made the child feel
/// permission-bound.
pub(crate) const BRIDGE_ALLOWED_TOOLS: &str = "mcp__codex";

/// MCP protocol revision this bridge speaks.
const PROTOCOL_VERSION: &str = "2025-06-18";

/// Concurrent bridge calls allowed at once.
///
/// The dispatched tools reach the real router and the real session; a child that
/// fans out hard should queue rather than flood its parent.
const MAX_CONCURRENT_BRIDGE_CALLS: usize = 4;

/// Serves the JSON-RPC traffic of one hosted MCP server.
#[derive(Debug)]
pub(crate) struct McpBridge {
    host: Arc<dyn ClaudeHost>,
    inflight: Arc<tokio::sync::Semaphore>,
}

impl McpBridge {
    pub(crate) fn new(host: Arc<dyn ClaudeHost>) -> Self {
        Self {
            host,
            inflight: Arc::new(tokio::sync::Semaphore::new(MAX_CONCURRENT_BRIDGE_CALLS)),
        }
    }

    /// Handles one JSON-RPC message.
    ///
    /// Returns the response to send back, or `None` for a notification, which
    /// by JSON-RPC has no reply.
    pub(crate) async fn handle(&self, message: &JsonValue) -> Option<JsonValue> {
        let method = message.get("method").and_then(JsonValue::as_str)?;
        let id = message.get("id").cloned();
        // A message without an id is a notification: acknowledged by doing
        // nothing, never answered.
        let id = id?;

        let result = match method {
            "initialize" => Ok(json!({
                "protocolVersion": PROTOCOL_VERSION,
                "capabilities": { "tools": {} },
                "serverInfo": { "name": BRIDGE_SERVER_NAME, "version": "1.0.0" },
            })),
            "tools/list" => Ok(json!({ "tools": self.host.bridge_tool_specs().await })),
            "tools/call" => {
                let params = message.get("params").unwrap_or(&JsonValue::Null);
                let name = params
                    .get("name")
                    .and_then(JsonValue::as_str)
                    .unwrap_or_default()
                    .to_string();
                let arguments = params
                    .get("arguments")
                    .cloned()
                    .unwrap_or_else(|| json!({}));
                let _permit = self.inflight.clone().acquire_owned().await.ok();
                match self.host.call_bridge_tool(&name, arguments).await {
                    Ok(value) => Ok(tool_result(&value, /*is_error*/ false)),
                    // An MCP tool failure is reported *inside* a successful
                    // result, so the agent reads it as a tool that said no
                    // rather than as a broken transport.
                    Err(message) => Ok(tool_result(&json!(message), /*is_error*/ true)),
                }
            }
            "ping" => Ok(json!({})),
            other => Err(format!("method `{other}` is not supported by this server")),
        };

        Some(match result {
            Ok(result) => json!({ "jsonrpc": "2.0", "id": id, "result": result }),
            Err(message) => json!({
                "jsonrpc": "2.0",
                "id": id,
                "error": { "code": -32601, "message": message },
            }),
        })
    }
}

/// Wraps a tool's output in the MCP content shape the CLI expects.
fn tool_result(value: &JsonValue, is_error: bool) -> JsonValue {
    let text = match value {
        JsonValue::String(text) => text.clone(),
        other => other.to_string(),
    };
    json!({
        "content": [{ "type": "text", "text": text }],
        "isError": is_error,
    })
}

#[cfg(test)]
#[path = "bridge_tests.rs"]
mod tests;
