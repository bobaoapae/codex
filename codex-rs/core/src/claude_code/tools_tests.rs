use super::*;
use serde_json::json;

fn tracker() -> PendingToolUses {
    let cwd = codex_utils_absolute_path::AbsolutePathBuf::from_absolute_path(std::path::Path::new(
        if cfg!(windows) { "C:\\repo" } else { "/repo" },
    ))
    .expect("an absolute path");
    PendingToolUses::new(PathUri::from_abs_path(&cwd))
}

fn tool_use(id: &str, name: &str, input: JsonValue) -> JsonValue {
    json!({ "type": "tool_use", "id": id, "name": name, "input": input })
}

/// The cell the parent watches while a command runs, and the one it reads when
/// the command finishes. Before this mapping both were a single line of
/// reasoning text and the output was discarded.
#[test]
fn a_bash_call_becomes_an_exec_cell_with_its_output() {
    let mut pending = tracker();

    let started = pending
        .start(&tool_use(
            "toolu_1",
            "Bash",
            json!({ "command": "cargo test -p codex-core" }),
        ))
        .expect("a started call");
    assert_eq!(started.phase, ProviderExecutedToolPhase::Started);
    assert!(
        started.history_items.is_empty(),
        "history is written when the call completes"
    );
    let TurnItem::CommandExecution(item) = &started.turn_items[0] else {
        panic!("expected an exec cell, got {:?}", started.turn_items[0]);
    };
    assert_eq!(item.status, CommandExecutionStatus::InProgress);
    assert_eq!(
        item.command.last().map(String::as_str),
        Some("cargo test -p codex-core")
    );
    assert_eq!(item.source, ExecCommandSource::Agent);
    let started_item_id = item.id.clone();

    let completed = pending
        .complete(&json!({
            "type": "tool_result",
            "tool_use_id": "toolu_1",
            "content": "test result: ok. 12 passed",
            "tool_use_result": {
                "stdout": "test result: ok. 12 passed\n",
                "stderr": "",
                "interrupted": false,
            },
        }))
        .expect("a completed call");
    assert_eq!(completed.phase, ProviderExecutedToolPhase::Completed);
    let TurnItem::CommandExecution(item) = &completed.turn_items[0] else {
        panic!("expected an exec cell");
    };
    // Same id: the UI updates the cell it opened rather than adding a second.
    assert_eq!(item.id, started_item_id);
    assert_eq!(item.status, CommandExecutionStatus::Completed);
    assert_eq!(item.exit_code, Some(0));
    assert_eq!(item.stdout.as_deref(), Some("test result: ok. 12 passed\n"));

    // The pair is what the model sees later, under a namespace of its own.
    assert_eq!(completed.history_items.len(), 2);
    let ResponseItem::FunctionCall {
        name, namespace, ..
    } = &completed.history_items[0]
    else {
        panic!("expected a function call");
    };
    assert_eq!(name, "Bash");
    assert_eq!(namespace.as_deref(), Some(CLAUDE_TOOL_NAMESPACE));
}

#[test]
fn a_failed_bash_call_is_marked_failed() {
    let mut pending = tracker();
    pending
        .start(&tool_use("toolu_2", "Bash", json!({ "command": "false" })))
        .expect("a started call");

    let completed = pending
        .complete(&json!({
            "type": "tool_result",
            "tool_use_id": "toolu_2",
            "is_error": true,
            "content": "command failed",
        }))
        .expect("a completed call");
    let TurnItem::CommandExecution(item) = &completed.turn_items[0] else {
        panic!("expected an exec cell");
    };
    assert_eq!(item.status, CommandExecutionStatus::Failed);
    assert_eq!(item.exit_code, Some(1));
    let ResponseItem::FunctionCallOutput { output, .. } = &completed.history_items[1] else {
        panic!("expected a function call output");
    };
    assert_eq!(output.success, Some(false));
}

/// The CLI reports the pre-edit contents in `originalFile`; replaying the edit
/// against it is what produces a real before/after pair for the turn diff.
#[test]
fn an_edit_reconstructs_the_before_and_after_contents() {
    let mut pending = tracker();
    let path = if cfg!(windows) {
        "C:\\repo\\src\\lib.rs"
    } else {
        "/repo/src/lib.rs"
    };
    pending
        .start(&tool_use(
            "toolu_3",
            "Edit",
            json!({
                "file_path": path,
                "old_string": "let x = 1;",
                "new_string": "let x = 2;",
            }),
        ))
        .expect("a started call");

    let completed = pending
        .complete(&json!({
            "type": "tool_result",
            "tool_use_id": "toolu_3",
            "content": "edited",
            "tool_use_result": { "originalFile": "fn main() {\n    let x = 1;\n}\n" },
        }))
        .expect("a completed call");

    assert_eq!(completed.file_changes.len(), 1);
    let ProviderExecutedFileChangeKind::Update {
        old_content,
        new_content,
    } = &completed.file_changes[0].kind
    else {
        panic!("expected an update");
    };
    assert!(old_content.contains("let x = 1;"));
    assert!(new_content.contains("let x = 2;"));
    assert!(!new_content.contains("let x = 1;"));

    let TurnItem::FileChange(item) = &completed.turn_items[0] else {
        panic!("expected a file change cell");
    };
    assert_eq!(item.changes.len(), 1);
    assert_eq!(item.status, Some(PatchApplyStatus::Completed));
}

/// `replace_all` changes the answer, and a `MultiEdit` applies its edits in
/// order against the same original.
#[test]
fn multi_edit_applies_every_edit_in_order() {
    let mut pending = tracker();
    let path = if cfg!(windows) {
        "C:\\repo\\a.txt"
    } else {
        "/repo/a.txt"
    };
    pending
        .start(&tool_use(
            "toolu_4",
            "MultiEdit",
            json!({
                "file_path": path,
                "edits": [
                    { "old_string": "a", "new_string": "b", "replace_all": true },
                    { "old_string": "c", "new_string": "d" },
                ],
            }),
        ))
        .expect("a started call");

    let completed = pending
        .complete(&json!({
            "type": "tool_result",
            "tool_use_id": "toolu_4",
            "tool_use_result": { "originalFile": "a a c c" },
        }))
        .expect("a completed call");
    let ProviderExecutedFileChangeKind::Update { new_content, .. } =
        &completed.file_changes[0].kind
    else {
        panic!("expected an update");
    };
    assert_eq!(new_content, "b b d c");
}

#[test]
fn a_write_to_a_new_file_is_an_add() {
    let mut pending = tracker();
    let path = if cfg!(windows) {
        "C:\\repo\\new.txt"
    } else {
        "/repo/new.txt"
    };
    pending
        .start(&tool_use(
            "toolu_5",
            "Write",
            json!({ "file_path": path, "content": "hello\n" }),
        ))
        .expect("a started call");

    let completed = pending
        .complete(&json!({ "type": "tool_result", "tool_use_id": "toolu_5" }))
        .expect("a completed call");
    assert!(matches!(
        completed.file_changes[0].kind,
        ProviderExecutedFileChangeKind::Add { .. }
    ));
}

#[test]
fn an_mcp_call_becomes_an_mcp_cell() {
    let mut pending = tracker();
    let started = pending
        .start(&tool_use(
            "toolu_6",
            "mcp__codex__send_message",
            json!({ "target": ".." }),
        ))
        .expect("a started call");
    let TurnItem::McpToolCall(item) = &started.turn_items[0] else {
        panic!("expected an MCP cell");
    };
    assert_eq!(item.server, "codex");
    assert_eq!(item.tool, "send_message");
}

/// Anything unrecognized still gets a cell rather than vanishing.
#[test]
fn an_unknown_tool_degrades_to_a_dynamic_cell() {
    let mut pending = tracker();
    pending
        .start(&tool_use(
            "toolu_7",
            "WebFetch",
            json!({ "url": "https://example.com" }),
        ))
        .expect("a started call");
    let completed = pending
        .complete(&json!({
            "type": "tool_result",
            "tool_use_id": "toolu_7",
            "content": [{ "type": "text", "text": "fetched" }],
        }))
        .expect("a completed call");
    let TurnItem::DynamicToolCall(item) = &completed.turn_items[0] else {
        panic!("expected a dynamic cell");
    };
    assert_eq!(item.tool, "WebFetch");
    assert_eq!(item.namespace.as_deref(), Some("claude"));
    assert_eq!(item.status, DynamicToolCallStatus::Completed);
    assert_eq!(item.success, Some(true));
}

/// A result that arrives without a matching start (a reconnect, a CLI quirk) is
/// dropped rather than opening a cell that was never started.
#[test]
fn an_unmatched_result_is_ignored() {
    let mut pending = tracker();
    assert!(
        pending
            .complete(&json!({ "type": "tool_result", "tool_use_id": "never-started" }))
            .is_none()
    );
    // A `tool_use` with no id could never be matched, so it opens nothing.
    assert!(
        pending
            .start(&json!({ "type": "tool_use", "name": "Bash" }))
            .is_none()
    );
}

#[test]
fn the_pending_map_is_capped_and_cleared() {
    let mut pending = tracker();
    for index in 0..(MAX_PENDING_TOOL_USES + 10) {
        pending
            .start(&tool_use(&format!("toolu_{index}"), "Read", json!({})))
            .expect("a started call");
    }
    assert!(pending.by_id.len() <= MAX_PENDING_TOOL_USES);
    // The oldest entries are the ones evicted.
    assert!(!pending.by_id.contains_key("toolu_0"));

    pending.clear();
    assert!(pending.by_id.is_empty());
    assert!(pending.order.is_empty());
}
