use super::*;

fn channel() -> (ControlChannel, mpsc::Receiver<String>) {
    let (tx, rx) = mpsc::channel(16);
    (ControlChannel::new(tx), rx)
}

/// The CLI stalls its own turn while a request goes unanswered, so every frame
/// we recognize must produce bytes on stdin.
#[tokio::test]
async fn a_tool_permission_decision_becomes_a_control_response() {
    let (control, mut rx) = channel();

    control
        .respond_tool_permission(
            "req-1",
            &ToolPermissionDecision::Allow {
                updated_input: None,
                updated_permissions: Some(json!([{ "type": "addRules" }])),
            },
        )
        .await;

    let frame: JsonValue = serde_json::from_str(&rx.recv().await.expect("a frame")).expect("json");
    assert_eq!(frame["type"], "control_response");
    assert_eq!(frame["response"]["subtype"], "success");
    assert_eq!(frame["response"]["request_id"], "req-1");
    assert_eq!(frame["response"]["response"]["behavior"], "allow");
    assert!(frame["response"]["response"]["updatedPermissions"].is_array());

    control
        .respond_tool_permission(
            "req-2",
            &ToolPermissionDecision::Deny {
                message: "the user declined".to_string(),
                interrupt: false,
            },
        )
        .await;

    let frame: JsonValue = serde_json::from_str(&rx.recv().await.expect("a frame")).expect("json");
    assert_eq!(frame["response"]["response"]["behavior"], "deny");
    assert_eq!(
        frame["response"]["response"]["message"],
        "the user declined"
    );
    assert_eq!(frame["response"]["response"]["interrupt"], false);
}

/// FORK: an outbound request is sent and its answer recognized later, because
/// only the task reading stdout can deliver one. Awaiting it here would deadlock
/// that task against itself.
#[tokio::test]
async fn an_outbound_request_is_recognized_by_its_id() {
    let (control, mut rx) = channel();

    let request_id = control
        .send_request("initialize", json!({ "appendSystemPrompt": "hello" }))
        .await
        .expect("the frame should be queued");

    let frame: JsonValue = serde_json::from_str(&rx.recv().await.expect("a frame")).expect("json");
    assert_eq!(frame["type"], "control_request");
    assert_eq!(frame["request"]["subtype"], "initialize");
    assert_eq!(frame["request"]["appendSystemPrompt"], "hello");
    assert_eq!(frame["request_id"], request_id);

    let outcome = control
        .resolve_response(&json!({
            "type": "control_response",
            "response": {
                "subtype": "success",
                "request_id": request_id,
                "response": { "ok": true },
            },
        }))
        .expect("a well-formed response");
    assert_eq!(outcome.request_id, request_id);
    assert_eq!(outcome.result, Ok(json!({ "ok": true })));
}

/// A CLI that does not know a subtype answers with an error; the caller must see
/// that and carry on without the feature rather than fail the turn.
#[tokio::test]
async fn an_error_response_is_reported_as_a_failure() {
    let (control, mut rx) = channel();
    let request_id = control
        .send_request("initialize", json!({}))
        .await
        .expect("the frame should be queued");
    let _ = rx.recv().await.expect("a frame");

    let outcome = control
        .resolve_response(&json!({
            "type": "control_response",
            "response": {
                "subtype": "error",
                "request_id": request_id,
                "error": "unknown subtype",
            },
        }))
        .expect("a well-formed response");
    assert_eq!(outcome.result, Err("unknown subtype".to_string()));
}

#[test]
fn a_malformed_response_is_not_an_outcome() {
    let (control, _rx) = channel();
    // No request id: nothing to attribute the answer to.
    assert!(
        control
            .resolve_response(&json!({
                "type": "control_response",
                "response": { "subtype": "success" },
            }))
            .is_none()
    );
    // A response for someone else's request is still well-formed; the caller
    // decides whether it recognizes the id.
    let outcome = control
        .resolve_response(&json!({
            "type": "control_response",
            "response": { "subtype": "success", "request_id": "someone-else" },
        }))
        .expect("a well-formed response");
    assert_eq!(outcome.request_id, "someone-else");
}

#[test]
fn inbound_requests_are_classified_by_subtype() {
    let (request_id, classified) = classify_request(&json!({
        "type": "control_request",
        "request_id": "cli-1",
        "request": {
            "subtype": "can_use_tool",
            "tool_name": "Bash",
            "input": { "command": "cargo test" },
            "tool_use_id": "toolu_1",
            "blocked_path": "C:/repo",
        },
    }))
    .expect("a classified request");
    assert_eq!(request_id, "cli-1");
    let InboundControl::CanUseTool(can_use_tool) = classified else {
        panic!("expected can_use_tool");
    };
    assert_eq!(can_use_tool.tool_name, "Bash");
    assert_eq!(can_use_tool.input["command"], "cargo test");
    assert_eq!(can_use_tool.tool_use_id.as_deref(), Some("toolu_1"));
    assert_eq!(can_use_tool.blocked_path.as_deref(), Some("C:/repo"));

    let (_, classified) = classify_request(&json!({
        "request_id": "cli-2",
        "request": {
            "subtype": "mcp_message",
            "server_name": "codex",
            "message": { "jsonrpc": "2.0", "id": 1, "method": "tools/list" },
        },
    }))
    .expect("a classified request");
    assert_eq!(
        classified,
        InboundControl::McpMessage {
            server: "codex".to_string(),
            message: json!({ "jsonrpc": "2.0", "id": 1, "method": "tools/list" }),
        }
    );

    let (_, classified) = classify_request(&json!({
        "request_id": "cli-3",
        "request": { "subtype": "something_new" },
    }))
    .expect("a classified request");
    assert_eq!(
        classified,
        InboundControl::Unknown {
            subtype: "something_new".to_string()
        }
    );

    // A frame without a request id cannot be answered, so it is not ours.
    assert!(classify_request(&json!({ "request": { "subtype": "can_use_tool" } })).is_none());
}

#[test]
fn the_initialize_payload_carries_the_system_prompt_and_bridge() {
    let payload = initialize_payload(Some("  you are a subagent  "), &["codex"]);
    assert_eq!(payload["appendSystemPrompt"], "you are a subagent");
    assert_eq!(payload["sdkMcpServers"], json!(["codex"]));

    // Nothing to say and no bridge: an empty object, not null fields.
    assert_eq!(initialize_payload(Some("   "), &[]), json!({}));
}
