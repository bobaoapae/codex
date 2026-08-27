//! FORK: tests for the connector seam's tool reduction.

use super::*;
use codex_tools::FreeformTool;
use codex_tools::FreeformToolFormat;
use codex_tools::JsonSchema;
use codex_tools::ResponsesApiNamespace;
use codex_tools::ResponsesApiNamespaceTool;
use codex_tools::ResponsesApiTool;

fn function(name: &str) -> ToolSpec {
    ToolSpec::Function(ResponsesApiTool {
        name: name.to_string(),
        description: format!("{name} does a thing"),
        strict: false,
        defer_loading: None,
        parameters: JsonSchema::default(),
        output_schema: None,
    })
}

fn freeform(name: &str) -> ToolSpec {
    ToolSpec::Freeform(FreeformTool {
        name: name.to_string(),
        description: format!("{name} freeform"),
        defer_loading: None,
        format: FreeformToolFormat {
            r#type: "grammar".to_string(),
            syntax: "lark".to_string(),
            definition: "start: /.*/".to_string(),
        },
    })
}

#[test]
fn a_unified_exec_turn_maps_to_exec_command_and_freeform_apply_patch() {
    let tools = vec![function("exec_command"), freeform("apply_patch")];
    let (summaries, exec_tool, apply_patch) = tool_summaries(&tools);

    assert_eq!(summaries.len(), 2);
    assert_eq!(exec_tool, ExecTool::ExecCommand);
    assert!(apply_patch);
    let exec = summaries.iter().find(|t| t.name == "exec_command").unwrap();
    assert_eq!(exec.namespace, None);
    assert_eq!(exec.kind, ToolKind::Function);
}

#[test]
fn a_legacy_shell_turn_maps_to_shell_and_no_apply_patch() {
    let (_, exec_tool, apply_patch) = tool_summaries(&[function("shell")]);
    assert_eq!(exec_tool, ExecTool::Shell);
    assert!(!apply_patch);
}

#[test]
fn namespaced_tools_keep_their_namespace_and_the_default_is_flattened() {
    let tools = vec![
        function("exec_command"),
        ToolSpec::Namespace(ResponsesApiNamespace {
            name: "figma".to_string(),
            description: "Figma tools".to_string(),
            tools: vec![
                ResponsesApiNamespaceTool::Function(ResponsesApiTool {
                    name: "get_file".to_string(),
                    description: "read a file".to_string(),
                    strict: false,
                    defer_loading: None,
                    parameters: JsonSchema::default(),
                    output_schema: None,
                }),
                ResponsesApiNamespaceTool::Custom(FreeformTool {
                    name: "edit".to_string(),
                    description: "edit".to_string(),
                    defer_loading: None,
                    format: FreeformToolFormat {
                        r#type: "grammar".to_string(),
                        syntax: "lark".to_string(),
                        definition: "start: /.*/".to_string(),
                    },
                }),
            ],
        }),
    ];
    let (summaries, _, _) = tool_summaries(&tools);

    let namespaced = summaries.iter().find(|t| t.name == "get_file").unwrap();
    assert_eq!(namespaced.namespace.as_deref(), Some("figma"));
    let custom = summaries.iter().find(|t| t.name == "edit").unwrap();
    assert_eq!(custom.namespace.as_deref(), Some("figma"));
    assert_eq!(custom.kind, ToolKind::Freeform);
    // The default function namespace stays flattened to `None`.
    let exec = summaries.iter().find(|t| t.name == "exec_command").unwrap();
    assert_eq!(exec.namespace, None);
}
