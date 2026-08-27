use super::*;
use serde_json::json;

fn can_use_tool(tool_name: &str, input: JsonValue) -> CanUseTool {
    CanUseTool {
        tool_name: tool_name.to_string(),
        input,
        tool_use_id: Some("toolu_1".to_string()),
        permission_suggestions: None,
        decision_reason: None,
        blocked_path: None,
    }
}

/// The user has to see *what* is being asked. A `Bash` request becomes a
/// command approval, a write becomes a patch approval, and everything else still
/// gets a surface rather than an automatic refusal.
#[test]
fn a_tool_request_picks_the_matching_approval_surface() {
    let ApprovalShape::Command(command) =
        ApprovalShape::of(&can_use_tool("Bash", json!({ "command": "cargo test" })))
    else {
        panic!("Bash must map to a command approval");
    };
    assert_eq!(command.last().map(String::as_str), Some("cargo test"));

    let ApprovalShape::Patch(changes) = ApprovalShape::of(&can_use_tool(
        "Write",
        json!({ "file_path": "/repo/a.rs", "content": "fn main() {}" }),
    )) else {
        panic!("Write must map to a patch approval");
    };
    assert_eq!(changes.len(), 1);
    assert!(changes.contains_key(&PathBuf::from("/repo/a.rs")));

    let ApprovalShape::Patch(changes) = ApprovalShape::of(&can_use_tool(
        "Edit",
        json!({ "file_path": "/repo/a.rs", "new_string": "fn other() {}" }),
    )) else {
        panic!("Edit must map to a patch approval");
    };
    assert_eq!(changes.len(), 1);

    // An unknown tool is still shown, not silently refused.
    let ApprovalShape::Command(command) = ApprovalShape::of(&can_use_tool(
        "mcp__chrome__browser_click",
        json!({ "selector": "#go" }),
    )) else {
        panic!("an unknown tool must still reach an approval surface");
    };
    assert_eq!(command[0], "mcp__chrome__browser_click");
    assert!(command[1].contains("#go"));
}

/// A payload big enough to break the dialog is truncated, not dropped.
#[test]
fn oversized_tool_arguments_are_bounded() {
    let huge = "x".repeat(10_000);
    let ApprovalShape::Command(command) =
        ApprovalShape::of(&can_use_tool("Weird", json!({ "blob": huge })))
    else {
        panic!("expected a command approval");
    };
    assert!(command[1].chars().count() <= 401, "{}", command[1].len());
    assert!(command[1].ends_with('…'));
}

#[test]
fn the_reason_names_the_tool_and_the_blocked_path() {
    let mut request = can_use_tool("Bash", json!({ "command": "dotnet build" }));
    request.blocked_path = Some("C:/repo/bin".to_string());
    let reason = approval_reason(&request).expect("a reason");
    assert!(reason.contains("Bash"), "{reason}");
    assert!(reason.contains("C:/repo/bin"), "{reason}");
}

/// A write whose path the CLI did not send cannot be previewed; the request
/// still reaches an approval surface rather than being dropped.
#[test]
fn a_write_without_a_path_previews_nothing() {
    let changes = patch_preview(&can_use_tool("Write", json!({ "content": "x" })));
    assert!(changes.is_empty());
}

/// FORK: the bridge is a whitelist. The child already has its own `Bash` and
/// `Edit` against the same tree, so exposing Codex's would give it two ways to
/// do the same thing — and only one of them under the parent's sandbox.
#[test]
fn the_bridge_allow_list_admits_collaboration_and_refuses_execution() {
    let name = bridge_tool_name("mcp__codex__send_message", "collaboration")
        .expect("send_message is bridged");
    assert_eq!(name.namespace.as_deref(), Some("collaboration"));
    assert_eq!(name.name, "send_message");

    // The bare form works too: the CLI is not consistent about the prefix.
    assert!(bridge_tool_name("wait_agent", "collaboration").is_some());
    assert_eq!(
        bridge_tool_name("update_plan", "collaboration")
            .expect("update_plan is bridged")
            .namespace,
        None
    );

    for denied in [
        "shell",
        "unified_exec",
        "apply_patch",
        "read_file",
        "view_image",
    ] {
        assert!(
            bridge_tool_name(&format!("mcp__codex__{denied}"), "collaboration").is_none(),
            "{denied} must not be bridged"
        );
    }

    // A session MCP tool keeps its server namespace on the way through.
    let name = bridge_tool_name("mcp__codex__chrome__browser_click", "collaboration")
        .expect("an MCP tool");
    assert_eq!(name.namespace.as_deref(), Some("chrome"));
    assert_eq!(name.name, "browser_click");

    // Names that cannot be resolved are refused rather than guessed at.
    assert!(bridge_tool_name("nonsense", "collaboration").is_none());
}

/// Every name the child is offered must resolve back to the same tool.
#[test]
fn every_exposed_name_round_trips() {
    for (namespace, name) in [
        (Some("collaboration"), "send_message"),
        (Some("collaboration"), "wait_agent"),
        (None, "update_plan"),
        (Some("chrome"), "browser_click"),
    ] {
        let tool_name = ToolName::new(namespace.map(str::to_string), name);
        let exposed = bridge_exposed_name(&tool_name, "collaboration").expect("an exposed name");
        assert_eq!(
            bridge_tool_name(&exposed, "collaboration").as_ref(),
            Some(&tool_name)
        );
    }

    assert_eq!(
        bridge_exposed_name(&ToolName::plain("shell"), "collaboration"),
        None
    );
    assert_eq!(
        bridge_exposed_name(
            &ToolName::namespaced("collaboration", "close_agent"),
            "collaboration"
        ),
        None
    );
}

/// FORK: `features.multi_agent_v2.tool_namespace` renames the collaboration
/// namespace (this user's config sets `collab_agents`). Hardcoding it would make
/// every bridged `send_message` resolve to a tool that does not exist.
#[test]
fn the_bridge_follows_a_renamed_collaboration_namespace() {
    let name = bridge_tool_name("mcp__codex__send_message", "collab_agents")
        .expect("send_message is bridged");
    assert_eq!(name.namespace.as_deref(), Some("collab_agents"));

    let exposed = bridge_exposed_name(
        &ToolName::namespaced("collab_agents", "send_message"),
        "collab_agents",
    )
    .expect("an exposed name");
    assert_eq!(exposed, "send_message");

    // Under a rename, the old namespace is just another MCP server.
    assert_eq!(
        bridge_exposed_name(
            &ToolName::namespaced("collaboration", "send_message"),
            "collab_agents"
        ),
        Some("collaboration__send_message".to_string())
    );
}
