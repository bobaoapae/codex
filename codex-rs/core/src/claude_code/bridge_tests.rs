use super::*;
use crate::claude_code::control::CanUseTool;
use crate::claude_code::control::ToolPermissionDecision;
use futures::future::BoxFuture;
use std::sync::Mutex;

/// A host that records what the bridge asked it to run.
#[derive(Debug, Default)]
struct FakeHost {
    calls: Mutex<Vec<(String, JsonValue)>>,
    allowed: Vec<String>,
}

impl FakeHost {
    fn with_tools(allowed: &[&str]) -> Self {
        Self {
            calls: Mutex::new(Vec::new()),
            allowed: allowed.iter().map(|name| (*name).to_string()).collect(),
        }
    }
}

impl ClaudeHost for FakeHost {
    fn approve_tool<'a>(
        &'a self,
        _request: &'a CanUseTool,
    ) -> BoxFuture<'a, ToolPermissionDecision> {
        Box::pin(async {
            ToolPermissionDecision::Allow {
                updated_input: None,
                updated_permissions: None,
            }
        })
    }

    fn call_bridge_tool<'a>(
        &'a self,
        name: &'a str,
        arguments: JsonValue,
    ) -> BoxFuture<'a, Result<JsonValue, String>> {
        Box::pin(async move {
            if !self.allowed.iter().any(|allowed| allowed == name) {
                return Err(format!("tool `{name}` is not available to this agent"));
            }
            self.calls
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push((name.to_string(), arguments));
            Ok(json!({ "ok": true }))
        })
    }

    fn bridge_tool_specs(&self) -> BoxFuture<'_, Vec<JsonValue>> {
        Box::pin(async {
            self.allowed
                .iter()
                .map(|name| json!({ "name": name, "description": "", "inputSchema": {} }))
                .collect()
        })
    }
}

fn bridge(host: Arc<FakeHost>) -> McpBridge {
    McpBridge::new(host)
}

#[tokio::test]
async fn the_handshake_advertises_tools() {
    let host = Arc::new(FakeHost::with_tools(&["send_message", "list_agents"]));
    let bridge = bridge(Arc::clone(&host));

    let response = bridge
        .handle(&json!({ "jsonrpc": "2.0", "id": 1, "method": "initialize" }))
        .await
        .expect("a response");
    assert_eq!(response["result"]["protocolVersion"], PROTOCOL_VERSION);
    assert!(response["result"]["capabilities"]["tools"].is_object());

    let response = bridge
        .handle(&json!({ "jsonrpc": "2.0", "id": 2, "method": "tools/list" }))
        .await
        .expect("a response");
    let tools = response["result"]["tools"]
        .as_array()
        .expect("a tool array");
    assert_eq!(tools.len(), 2);
    assert_eq!(tools[0]["name"], "send_message");
}

/// The point of the bridge: a child that needs to tell its parent something
/// mid-task has a way to do it that is not a Desktop thread card.
#[tokio::test]
async fn an_allowed_tool_is_dispatched() {
    let host = Arc::new(FakeHost::with_tools(&["send_message"]));
    let bridge = bridge(Arc::clone(&host));

    let response = bridge
        .handle(&json!({
            "jsonrpc": "2.0",
            "id": 3,
            "method": "tools/call",
            "params": {
                "name": "send_message",
                "arguments": { "target": "..", "plaintext_message": "halfway done" },
            },
        }))
        .await
        .expect("a response");

    assert_eq!(response["result"]["isError"], false);
    let calls = host
        .calls
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].0, "send_message");
    assert_eq!(calls[0].1["target"], "..");
}

/// A denied tool must not reach the host at all, and the refusal must read as a
/// tool that said no rather than as a broken transport.
#[tokio::test]
async fn a_denied_tool_is_refused_without_dispatch() {
    let host = Arc::new(FakeHost::with_tools(&["send_message"]));
    let bridge = bridge(Arc::clone(&host));

    let response = bridge
        .handle(&json!({
            "jsonrpc": "2.0",
            "id": 4,
            "method": "tools/call",
            "params": { "name": "shell", "arguments": { "command": "rm -rf /" } },
        }))
        .await
        .expect("a response");

    assert_eq!(response["result"]["isError"], true);
    assert!(
        host.calls
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .is_empty()
    );
}

#[tokio::test]
async fn an_unknown_method_is_a_json_rpc_error() {
    let host = Arc::new(FakeHost::default());
    let response = bridge(host)
        .handle(&json!({ "jsonrpc": "2.0", "id": 5, "method": "resources/list" }))
        .await
        .expect("a response");
    assert_eq!(response["error"]["code"], -32601);
}

/// A notification has no id and by JSON-RPC gets no reply.
#[tokio::test]
async fn a_notification_produces_no_response() {
    let host = Arc::new(FakeHost::default());
    assert!(
        bridge(host)
            .handle(&json!({ "jsonrpc": "2.0", "method": "notifications/initialized" }))
            .await
            .is_none()
    );
}
