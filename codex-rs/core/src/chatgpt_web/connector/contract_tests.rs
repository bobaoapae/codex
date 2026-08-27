use super::*;
use pretty_assertions::assert_eq;

fn obj(value: Value) -> JsonObject {
    match value {
        Value::Object(object) => object,
        other => panic!("expected object, got {other}"),
    }
}

const TOKEN: &str = "turn_0123456789abcdef0123456789abcdef";

fn turn(exec_tool: ExecTool, apply_patch: bool, extra: Vec<ToolSummary>) -> TurnTools {
    let mut tools = vec![
        ToolSummary {
            name: match exec_tool {
                ExecTool::ExecCommand => "exec_command".into(),
                ExecTool::Shell => "shell".into(),
            },
            namespace: None,
            kind: ToolKind::Function,
            description: "run".into(),
            schema: None,
        },
        ToolSummary {
            name: "view_image".into(),
            namespace: None,
            kind: ToolKind::Function,
            description: "look".into(),
            schema: Some(json!({"type": "object"})),
        },
    ];
    if exec_tool == ExecTool::ExecCommand {
        tools.push(ToolSummary {
            name: "write_stdin".into(),
            namespace: None,
            kind: ToolKind::Function,
            description: "write".into(),
            schema: None,
        });
    }
    if apply_patch {
        tools.push(ToolSummary {
            name: "apply_patch".into(),
            namespace: None,
            kind: ToolKind::Freeform,
            description: "patch".into(),
            schema: None,
        });
    }
    tools.extend(extra);
    TurnTools {
        tools: tools.into(),
        exec_tool,
        apply_patch,
        exec_default_yield_ms: 10_000,
    }
}

#[test]
fn the_contract_advertises_six_tools_with_short_descriptions_and_a_required_turn_token() {
    let tools = tools();
    let names: Vec<&str> = tools.iter().map(|tool| tool.name.as_ref()).collect();
    assert_eq!(names, TOOL_NAMES.to_vec());
    for tool in &tools {
        let description = tool.description.as_deref().unwrap_or_default();
        assert!(
            description.len() <= 120,
            "{}: {} chars",
            tool.name,
            description.len()
        );
        let required = tool.input_schema["required"]
            .as_array()
            .expect("required array");
        assert!(
            required.iter().any(|value| value == "turn_token"),
            "{} must require turn_token",
            tool.name
        );
    }
}

#[test]
fn parse_rejects_unknown_tools_and_bad_tokens() {
    let err = parse("codex_nope", None).expect_err("unknown tool");
    assert!(err.message.contains("unknown tool"));

    let err = parse(CODEX_EXEC, Some(&obj(json!({"cmd": "ls"})))).expect_err("missing token");
    assert!(err.message.contains("turn_token is required"));

    let err = parse(
        CODEX_EXEC,
        Some(&obj(json!({"turn_token": "short", "cmd": "ls"}))),
    )
    .expect_err("short token");
    assert!(err.message.contains("malformed"));

    let err = parse(
        CODEX_EXEC,
        Some(&obj(json!({"turn_token": "x".repeat(300), "cmd": "ls"}))),
    )
    .expect_err("long token");
    assert!(err.message.contains("malformed"));

    let err = parse(
        CODEX_EXEC,
        Some(&obj(
            json!({"turn_token": "turn with spaces and more chars", "cmd": "ls"}),
        )),
    )
    .expect_err("bad chars");
    assert!(err.message.contains("malformed"));
}

#[test]
fn parse_strips_the_token_from_the_arguments() {
    let parsed = parse(
        CODEX_EXEC,
        Some(&obj(json!({"turn_token": TOKEN, "cmd": "ls"}))),
    )
    .expect("parses");
    assert_eq!(parsed.tool, ContractTool::Exec);
    assert_eq!(parsed.turn_token, TOKEN);
    assert!(!parsed.args.contains_key("turn_token"));
    assert_eq!(parsed.args["cmd"], "ls");
}

#[test]
fn codex_exec_maps_to_exec_command_with_a_clamped_yield() {
    let parsed = parse(
        CODEX_EXEC,
        Some(&obj(
            json!({"turn_token": TOKEN, "cmd": "echo hi", "workdir": "/tmp", "yield_time_ms": 90000, "tty": true}),
        )),
    )
    .expect("parses");
    match to_call(&parsed, &turn(ExecTool::ExecCommand, false, vec![])).expect("resolves") {
        Resolved::Forward(CallTarget::Function {
            namespace,
            name,
            arguments,
        }) => {
            assert_eq!(namespace, None);
            assert_eq!(name, "exec_command");
            assert_eq!(
                arguments,
                json!({"cmd": "echo hi", "workdir": "/tmp", "yield_time_ms": 30000, "tty": true})
            );
        }
        other => panic!("unexpected {other:?}"),
    }
}

#[test]
fn codex_exec_maps_to_shell_when_that_is_what_the_turn_announced() {
    let parsed = parse(
        CODEX_EXEC,
        Some(&obj(json!({"turn_token": TOKEN, "cmd": "echo hi"}))),
    )
    .expect("parses");
    match to_call(&parsed, &turn(ExecTool::Shell, false, vec![])).expect("resolves") {
        Resolved::Forward(CallTarget::Function {
            name, arguments, ..
        }) => {
            assert_eq!(name, "shell");
            assert_eq!(
                arguments,
                json!({"command": "echo hi", "timeout_ms": 10000})
            );
        }
        other => panic!("unexpected {other:?}"),
    }
}

#[test]
fn write_stdin_needs_the_unified_exec_tool() {
    let parsed = parse(
        CODEX_WRITE_STDIN,
        Some(&obj(
            json!({"turn_token": TOKEN, "session_id": 7, "chars": "y\n"}),
        )),
    )
    .expect("parses");
    let err = to_call(&parsed, &turn(ExecTool::Shell, false, vec![])).expect_err("no write_stdin");
    assert!(err.message.contains("write_stdin"));

    match to_call(&parsed, &turn(ExecTool::ExecCommand, false, vec![])).expect("resolves") {
        Resolved::Forward(CallTarget::Function {
            name, arguments, ..
        }) => {
            assert_eq!(name, "write_stdin");
            assert_eq!(
                arguments,
                json!({"session_id": 7, "chars": "y\n", "yield_time_ms": 10000})
            );
        }
        other => panic!("unexpected {other:?}"),
    }
}

#[test]
fn apply_patch_is_a_custom_call_only_when_announced() {
    let parsed = parse(
        CODEX_APPLY_PATCH,
        Some(&obj(
            json!({"turn_token": TOKEN, "patch": "*** Begin Patch\n*** End Patch"}),
        )),
    )
    .expect("parses");
    let err =
        to_call(&parsed, &turn(ExecTool::ExecCommand, false, vec![])).expect_err("not announced");
    assert!(err.message.contains("apply_patch"));

    match to_call(&parsed, &turn(ExecTool::ExecCommand, true, vec![])).expect("resolves") {
        Resolved::Forward(CallTarget::Custom { name, input }) => {
            assert_eq!(name, "apply_patch");
            assert_eq!(input, "*** Begin Patch\n*** End Patch");
        }
        other => panic!("unexpected {other:?}"),
    }
}

#[test]
fn view_image_forwards_path_and_detail() {
    let parsed = parse(
        CODEX_VIEW_IMAGE,
        Some(&obj(
            json!({"turn_token": TOKEN, "path": "a.png", "detail": "high"}),
        )),
    )
    .expect("parses");
    match to_call(&parsed, &turn(ExecTool::ExecCommand, false, vec![])).expect("resolves") {
        Resolved::Forward(CallTarget::Function {
            name, arguments, ..
        }) => {
            assert_eq!(name, "view_image");
            assert_eq!(arguments, json!({"path": "a.png", "detail": "high"}));
        }
        other => panic!("unexpected {other:?}"),
    }
}

#[test]
fn inventory_filters_pages_and_optionally_includes_schemas() {
    let mcp = ToolSummary {
        name: "search".into(),
        namespace: Some("docs".into()),
        kind: ToolKind::Function,
        description: "Search the docs".into(),
        schema: Some(json!({"type": "object", "properties": {"q": {"type": "string"}}})),
    };
    let turn = turn(ExecTool::ExecCommand, true, vec![mcp]);

    let parsed = parse(
        CODEX_TOOL_INVENTORY,
        Some(&obj(
            json!({"turn_token": TOKEN, "query": "docs", "include_schema": true}),
        )),
    )
    .expect("parses");
    let Resolved::Local(result) = to_call(&parsed, &turn).expect("resolves") else {
        panic!("inventory is local");
    };
    let text = result.content[0].as_text().expect("text").text.clone();
    let body: Value = serde_json::from_str(&text).expect("json");
    assert_eq!(body["total"], 1);
    assert_eq!(body["tools"][0]["name"], "search");
    assert_eq!(body["tools"][0]["namespace"], "docs");
    assert!(body["tools"][0]["schema"].is_object());

    let parsed = parse(
        CODEX_TOOL_INVENTORY,
        Some(&obj(json!({"turn_token": TOKEN, "offset": 1, "limit": 2}))),
    )
    .expect("parses");
    let Resolved::Local(result) = to_call(&parsed, &turn).expect("resolves") else {
        panic!("inventory is local");
    };
    let text = result.content[0].as_text().expect("text").text.clone();
    let body: Value = serde_json::from_str(&text).expect("json");
    assert_eq!(body["total"], 5);
    assert_eq!(body["tools"].as_array().map(Vec::len), Some(2));
    assert!(body["tools"][0]["schema"].is_null());
}

#[test]
fn tool_call_resolves_namespaced_functions_and_freeform_tools() {
    let mcp = ToolSummary {
        name: "search".into(),
        namespace: Some("docs".into()),
        kind: ToolKind::Function,
        description: "Search".into(),
        schema: None,
    };
    let turn = turn(ExecTool::ExecCommand, true, vec![mcp]);

    let parsed = parse(
        CODEX_TOOL_CALL,
        Some(&obj(json!({
            "turn_token": TOKEN, "namespace": "docs", "name": "search", "arguments": {"q": "x"}
        }))),
    )
    .expect("parses");
    match to_call(&parsed, &turn).expect("resolves") {
        Resolved::Forward(CallTarget::Function {
            namespace,
            name,
            arguments,
        }) => {
            assert_eq!(namespace.as_deref(), Some("docs"));
            assert_eq!(name, "search");
            assert_eq!(arguments, json!({"q": "x"}));
        }
        other => panic!("unexpected {other:?}"),
    }

    let parsed = parse(
        CODEX_TOOL_CALL,
        Some(&obj(
            json!({"turn_token": TOKEN, "name": "apply_patch", "input": "PATCH"}),
        )),
    )
    .expect("parses");
    match to_call(&parsed, &turn).expect("resolves") {
        Resolved::Forward(CallTarget::Custom { name, input }) => {
            assert_eq!(name, "apply_patch");
            assert_eq!(input, "PATCH");
        }
        other => panic!("unexpected {other:?}"),
    }

    let parsed = parse(
        CODEX_TOOL_CALL,
        Some(&obj(json!({"turn_token": TOKEN, "name": "search"}))),
    )
    .expect("parses");
    let err = to_call(&parsed, &turn).expect_err("namespace required to match");
    assert!(err.message.contains("not available"));
}
