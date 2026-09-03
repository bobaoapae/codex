//! FORK: turning Claude's own tool calls into Codex transcript items.
//!
//! Claude Code runs its tool loop itself. Before this module the only trace of
//! that work was a line of reasoning text per call (`[Bash] cargo test`) and the
//! results were discarded outright, so the parent could not tell a child that
//! was compiling from one that was stuck. Measured consequence: 46% of
//! `claude-opus` sessions ended in an aborted turn, most of them interrupted by
//! an impatient parent.
//!
//! Two frames carry the information. `assistant` blocks of type `tool_use`
//! announce a call; a later `user` frame carries `tool_result` blocks keyed by
//! `tool_use_id`. This module pairs them and emits the same
//! [`TurnItem`]s Codex emits for its own tools, so exec cells, per-file diffs
//! and MCP call cells all light up for a Claude child.
//!
//! Mapping is deliberately tolerant. The shape of `tool_use_result` is
//! undocumented and varies per tool and per CLI version, so every field is
//! optional and anything unrecognized degrades to a `DynamicToolCall` carrying
//! the raw JSON. A cell with imperfect fields still beats no cell at all.

use codex_api::ProviderExecutedFileChange;
use codex_api::ProviderExecutedFileChangeKind;
use codex_api::ProviderExecutedTool;
use codex_api::ProviderExecutedToolPhase;
use codex_protocol::dynamic_tools::DynamicToolCallOutputContentItem;
use codex_protocol::dynamic_tools::PROVIDER_EXECUTED_TOOL_NAMESPACE;
use codex_protocol::items::CommandExecutionItem;
use codex_protocol::items::CommandExecutionStatus;
use codex_protocol::items::DynamicToolCallItem;
use codex_protocol::items::DynamicToolCallStatus;
use codex_protocol::items::FileChangeItem;
use codex_protocol::items::McpToolCallItem;
use codex_protocol::items::McpToolCallStatus;
use codex_protocol::items::TurnItem;
use codex_protocol::models::FunctionCallOutputBody;
use codex_protocol::models::FunctionCallOutputPayload;
use codex_protocol::models::ResponseItem;
use codex_protocol::protocol::ExecCommandSource;
use codex_protocol::protocol::FileChange;
use codex_protocol::protocol::PatchApplyStatus;
use codex_shell_command::parse_command::parse_command;
use codex_utils_path_uri::PathUri;
use serde_json::Value as JsonValue;
use std::collections::HashMap;
use std::path::PathBuf;

/// Namespace recorded on the history items produced here.
///
/// It is what tells the rest of the system these calls were already executed:
/// the router never sees them, the app-server never asks the client to run
/// them, and the history renderer drops them on replay so Claude is not fed its
/// own trace back.
pub(crate) const CLAUDE_TOOL_NAMESPACE: &str = PROVIDER_EXECUTED_TOOL_NAMESPACE;

/// Ceiling on tool calls tracked at once within a turn.
///
/// A well-behaved CLI closes each call before the turn ends. The cap exists so a
/// misbehaving one cannot grow this map without bound.
const MAX_PENDING_TOOL_USES: usize = 256;

/// A `tool_use` that has been announced but not yet resolved.
#[derive(Debug, Clone)]
struct PendingToolUse {
    name: String,
    input: JsonValue,
    /// The transcript item id, reused by the completion so the UI updates the
    /// cell it already opened rather than adding a second one.
    item_id: String,
}

/// Pairs `tool_use` announcements with the `tool_result` blocks that close them.
#[derive(Debug)]
pub(crate) struct PendingToolUses {
    by_id: HashMap<String, PendingToolUse>,
    /// Order of arrival, so the oldest entry is the one evicted at the cap.
    order: Vec<String>,
    /// Directory the CLI runs in. Claude does not report a working directory
    /// per `Bash` call, and an exec cell without one renders wrong.
    cwd: PathUri,
}

impl PendingToolUses {
    pub(crate) fn new(cwd: PathUri) -> Self {
        Self {
            by_id: HashMap::new(),
            order: Vec::new(),
            cwd,
        }
    }

    /// Records a `tool_use` block and returns the event that opens its cell.
    ///
    /// Returns `None` for a block that carries no id: without one the result
    /// could never be matched, and a cell that can never complete is worse than
    /// none.
    pub(crate) fn start(&mut self, block: &JsonValue) -> Option<ProviderExecutedTool> {
        let tool_use_id = block.get("id").and_then(JsonValue::as_str)?.to_string();
        let name = block
            .get("name")
            .and_then(JsonValue::as_str)
            .unwrap_or("tool")
            .to_string();
        let input = block.get("input").cloned().unwrap_or(JsonValue::Null);
        let item_id = format!("claude-{tool_use_id}");

        if self.by_id.len() >= MAX_PENDING_TOOL_USES
            && let Some(oldest) = self.order.first().cloned()
        {
            self.forget(&oldest);
        }
        self.by_id.insert(
            tool_use_id.clone(),
            PendingToolUse {
                name: name.clone(),
                input: input.clone(),
                item_id: item_id.clone(),
            },
        );
        self.order.push(tool_use_id.clone());

        Some(ProviderExecutedTool {
            call_id: tool_use_id,
            phase: ProviderExecutedToolPhase::Started,
            turn_items: vec![started_item(&item_id, &name, &input, &self.cwd)],
            // The call and its output are recorded together on completion, so a
            // turn that dies mid-tool leaves no dangling call in history.
            history_items: Vec::new(),
            file_changes: Vec::new(),
        })
    }

    /// Closes the call a `tool_result` block belongs to.
    pub(crate) fn complete(&mut self, block: &JsonValue) -> Option<ProviderExecutedTool> {
        let tool_use_id = block
            .get("tool_use_id")
            .and_then(JsonValue::as_str)?
            .to_string();
        let pending = self.forget(&tool_use_id)?;
        let is_error = block
            .get("is_error")
            .and_then(JsonValue::as_bool)
            .unwrap_or(false);
        // The CLI reports the structured form under `tool_use_result` and the
        // model-visible form under `content`; older builds send only the latter.
        let structured = block.get("tool_use_result");
        let content_text = result_content_text(block.get("content"));

        let (turn_item, file_changes) = completed_item(
            &pending,
            structured,
            content_text.as_deref(),
            is_error,
            &self.cwd,
        );

        Some(ProviderExecutedTool {
            call_id: tool_use_id.clone(),
            phase: ProviderExecutedToolPhase::Completed,
            turn_items: vec![turn_item],
            history_items: history_items(&pending, &tool_use_id, content_text, is_error),
            file_changes,
        })
    }

    /// Drops everything still open, at the end of a turn.
    pub(crate) fn clear(&mut self) {
        self.by_id.clear();
        self.order.clear();
    }

    fn forget(&mut self, tool_use_id: &str) -> Option<PendingToolUse> {
        self.order.retain(|id| id != tool_use_id);
        self.by_id.remove(tool_use_id)
    }
}

/// The cell shown while a call is running.
fn started_item(item_id: &str, name: &str, input: &JsonValue, cwd: &PathUri) -> TurnItem {
    match ClaudeTool::classify(name) {
        ClaudeTool::Bash => {
            let command = bash_command(input);
            TurnItem::CommandExecution(CommandExecutionItem {
                id: item_id.to_string(),
                plugin_id: None,
                script_path: None,
                process_id: None,
                parsed_cmd: parse_command(&command),
                command,
                cwd: bash_cwd(input, cwd),
                source: ExecCommandSource::Agent,
                interaction_input: None,
                status: CommandExecutionStatus::InProgress,
                stdout: None,
                stderr: None,
                aggregated_output: None,
                exit_code: None,
                duration: None,
                formatted_output: None,
            })
        }
        ClaudeTool::Edit | ClaudeTool::Write | ClaudeTool::NotebookEdit => {
            TurnItem::FileChange(FileChangeItem {
                id: item_id.to_string(),
                changes: HashMap::new(),
                status: None,
                auto_approved: Some(true),
                stdout: None,
                stderr: None,
            })
        }
        ClaudeTool::Mcp { server, tool } => TurnItem::McpToolCall(McpToolCallItem {
            id: item_id.to_string(),
            server,
            tool,
            arguments: input.clone(),
            connector_id: None,
            mcp_app_resource_uri: None,
            link_id: None,
            app_name: None,
            action_name: None,
            plugin_id: None,
            read_only_hint: None,
            status: McpToolCallStatus::InProgress,
            result: None,
            error: None,
            duration: None,
        }),
        ClaudeTool::Other => TurnItem::DynamicToolCall(DynamicToolCallItem {
            id: item_id.to_string(),
            namespace: Some(CLAUDE_TOOL_NAMESPACE.to_string()),
            tool: name.to_string(),
            arguments: input.clone(),
            status: DynamicToolCallStatus::InProgress,
            content_items: None,
            success: None,
            error: None,
            duration: None,
        }),
    }
}

/// The cell shown once a call has returned, plus any file mutations it made.
fn completed_item(
    pending: &PendingToolUse,
    structured: Option<&JsonValue>,
    content_text: Option<&str>,
    is_error: bool,
    cwd: &PathUri,
) -> (TurnItem, Vec<ProviderExecutedFileChange>) {
    let item_id = pending.item_id.clone();
    match ClaudeTool::classify(&pending.name) {
        ClaudeTool::Bash => {
            let stdout = structured
                .and_then(|result| result.get("stdout"))
                .and_then(JsonValue::as_str)
                .map(str::to_string);
            let stderr = structured
                .and_then(|result| result.get("stderr"))
                .and_then(JsonValue::as_str)
                .map(str::to_string);
            let interrupted = structured
                .and_then(|result| result.get("interrupted"))
                .and_then(JsonValue::as_bool)
                .unwrap_or(false);
            // The CLI does not report an exit code; `is_error` is the only
            // success signal it gives, so it stands in for one.
            let exit_code = Some(i32::from(is_error));
            let aggregated = match (stdout.as_deref(), stderr.as_deref()) {
                (Some(out), Some(err)) if !err.is_empty() => Some(format!("{out}{err}")),
                (Some(out), _) => Some(out.to_string()),
                (None, Some(err)) => Some(err.to_string()),
                (None, None) => content_text.map(str::to_string),
            };
            let command = bash_command(&pending.input);
            (
                TurnItem::CommandExecution(CommandExecutionItem {
                    id: item_id,
                    plugin_id: None,
                    script_path: None,
                    process_id: None,
                    parsed_cmd: parse_command(&command),
                    command,
                    cwd: bash_cwd(&pending.input, cwd),
                    source: ExecCommandSource::Agent,
                    interaction_input: None,
                    status: if is_error || interrupted {
                        CommandExecutionStatus::Failed
                    } else {
                        CommandExecutionStatus::Completed
                    },
                    stdout,
                    stderr,
                    aggregated_output: aggregated,
                    exit_code,
                    duration: None,
                    formatted_output: None,
                }),
                Vec::new(),
            )
        }
        ClaudeTool::Edit | ClaudeTool::Write | ClaudeTool::NotebookEdit => {
            let file_changes = if is_error {
                Vec::new()
            } else {
                file_changes_for(&pending.name, &pending.input, structured)
            };
            let changes = file_changes
                .iter()
                .map(|change| (change.path.clone(), display_change(change)))
                .collect();
            (
                TurnItem::FileChange(FileChangeItem {
                    id: item_id,
                    changes,
                    status: Some(if is_error {
                        PatchApplyStatus::Failed
                    } else {
                        PatchApplyStatus::Completed
                    }),
                    auto_approved: Some(true),
                    stdout: None,
                    stderr: is_error.then(|| content_text.unwrap_or_default().to_string()),
                }),
                file_changes,
            )
        }
        ClaudeTool::Mcp { server, tool } => (
            TurnItem::McpToolCall(McpToolCallItem {
                id: item_id,
                server,
                tool,
                arguments: pending.input.clone(),
                connector_id: None,
                mcp_app_resource_uri: None,
                link_id: None,
                app_name: None,
                action_name: None,
                plugin_id: None,
                read_only_hint: None,
                status: if is_error {
                    McpToolCallStatus::Failed
                } else {
                    McpToolCallStatus::Completed
                },
                // The CLI does not hand back the MCP envelope, only rendered
                // text, so the structured result stays empty rather than faked.
                result: None,
                error: is_error.then(|| codex_protocol::items::McpToolCallError {
                    message: content_text.unwrap_or_default().to_string(),
                }),
                duration: None,
            }),
            Vec::new(),
        ),
        ClaudeTool::Other => (
            TurnItem::DynamicToolCall(DynamicToolCallItem {
                id: item_id,
                namespace: Some(CLAUDE_TOOL_NAMESPACE.to_string()),
                tool: pending.name.clone(),
                arguments: pending.input.clone(),
                status: if is_error {
                    DynamicToolCallStatus::Failed
                } else {
                    DynamicToolCallStatus::Completed
                },
                content_items: content_text.map(|text| {
                    vec![DynamicToolCallOutputContentItem::InputText {
                        text: text.to_string(),
                    }]
                }),
                success: Some(!is_error),
                error: is_error.then(|| content_text.unwrap_or_default().to_string()),
                duration: None,
            }),
            Vec::new(),
        ),
    }
}

/// The pair of history items recorded for one completed call.
fn history_items(
    pending: &PendingToolUse,
    tool_use_id: &str,
    content_text: Option<String>,
    is_error: bool,
) -> Vec<ResponseItem> {
    vec![
        ResponseItem::FunctionCall {
            id: None,
            name: pending.name.clone(),
            // A namespace of its own is what keeps these out of the router and
            // out of the transcript replayed back to Claude.
            namespace: Some(CLAUDE_TOOL_NAMESPACE.to_string()),
            arguments: pending.input.to_string(),
            encrypted_function_args: None,
            call_id: tool_use_id.to_string(),
            internal_chat_message_metadata_passthrough: None,
        },
        ResponseItem::FunctionCallOutput {
            id: None,
            call_id: Some(tool_use_id.to_string()),
            name: Some(pending.name.clone()),
            namespace: Some(CLAUDE_TOOL_NAMESPACE.to_string()),
            output: FunctionCallOutputPayload {
                body: FunctionCallOutputBody::Text(super::history::truncate_tool_output(
                    content_text.as_deref().unwrap_or_default(),
                )),
                success: Some(!is_error),
            },
            internal_chat_message_metadata_passthrough: None,
        },
    ]
}

/// Which Codex cell a Claude tool name maps to.
enum ClaudeTool {
    Bash,
    Edit,
    Write,
    NotebookEdit,
    Mcp { server: String, tool: String },
    Other,
}

impl ClaudeTool {
    fn classify(name: &str) -> Self {
        if let Some(rest) = name.strip_prefix("mcp__") {
            let mut parts = rest.splitn(2, "__");
            let server = parts.next().unwrap_or_default().to_string();
            let tool = parts.next().unwrap_or_default().to_string();
            return Self::Mcp { server, tool };
        }
        match name {
            "Bash" | "BashOutput" | "KillShell" => Self::Bash,
            "Edit" | "MultiEdit" => Self::Edit,
            "Write" => Self::Write,
            "NotebookEdit" => Self::NotebookEdit,
            _ => Self::Other,
        }
    }
}

fn bash_command(input: &JsonValue) -> Vec<String> {
    let command = input
        .get("command")
        .and_then(JsonValue::as_str)
        .unwrap_or_default()
        .to_string();
    // Claude sends one shell string, the same shape Codex's own shell tool
    // records, so the existing parser and renderer apply unchanged.
    vec!["bash".to_string(), "-lc".to_string(), command]
}

fn bash_cwd(input: &JsonValue, cwd: &PathUri) -> PathUri {
    // Claude does not report the working directory of a `Bash` call, so the
    // caller-supplied one is used when present and the workspace root otherwise.
    input
        .get("cwd")
        .and_then(JsonValue::as_str)
        .and_then(|cwd| PathUri::parse(cwd).ok())
        .unwrap_or_else(|| cwd.clone())
}

/// Reads the model-visible text out of a `tool_result` block.
fn result_content_text(content: Option<&JsonValue>) -> Option<String> {
    match content? {
        JsonValue::String(text) => Some(text.clone()),
        JsonValue::Array(blocks) => {
            let text = blocks
                .iter()
                .filter_map(|block| block.get("text").and_then(JsonValue::as_str))
                .collect::<Vec<_>>()
                .join("\n");
            (!text.is_empty()).then_some(text)
        }
        other => Some(other.to_string()),
    }
}

/// Reconstructs what an edit actually did to each file.
///
/// The CLI reports the pre-edit contents in `originalFile`, which is what lets
/// the before/after pair be rebuilt exactly. Without it the change is still
/// shown, with an empty original — a diff of the whole file rather than nothing.
fn file_changes_for(
    name: &str,
    input: &JsonValue,
    structured: Option<&JsonValue>,
) -> Vec<ProviderExecutedFileChange> {
    let Some(path) = input
        .get("file_path")
        .or_else(|| input.get("notebook_path"))
        .and_then(JsonValue::as_str)
        .map(PathBuf::from)
    else {
        return Vec::new();
    };
    let original = structured
        .and_then(|result| result.get("originalFile"))
        .and_then(JsonValue::as_str);

    match name {
        "Write" => {
            let content = input
                .get("content")
                .and_then(JsonValue::as_str)
                .unwrap_or_default()
                .to_string();
            let kind = match original {
                Some(original) => ProviderExecutedFileChangeKind::Update {
                    old_content: original.to_string(),
                    new_content: content,
                },
                None => ProviderExecutedFileChangeKind::Add { content },
            };
            vec![ProviderExecutedFileChange { path, kind }]
        }
        "Edit" | "MultiEdit" => {
            let Some(original) = original else {
                return Vec::new();
            };
            let edits = edit_operations(input);
            let mut new_content = original.to_string();
            for (old_string, new_string, replace_all) in edits {
                new_content = if replace_all {
                    new_content.replace(&old_string, &new_string)
                } else {
                    new_content.replacen(&old_string, &new_string, 1)
                };
            }
            vec![ProviderExecutedFileChange {
                path,
                kind: ProviderExecutedFileChangeKind::Update {
                    old_content: original.to_string(),
                    new_content,
                },
            }]
        }
        // A notebook edit is structured, not textual: showing the cell is worth
        // it, but there is no faithful line diff to compute.
        _ => Vec::new(),
    }
}

/// The `(old, new, replace_all)` triples of an `Edit` or `MultiEdit` call.
fn edit_operations(input: &JsonValue) -> Vec<(String, String, bool)> {
    let single = |value: &JsonValue| {
        let old_string = value
            .get("old_string")
            .and_then(JsonValue::as_str)?
            .to_string();
        let new_string = value
            .get("new_string")
            .and_then(JsonValue::as_str)
            .unwrap_or_default()
            .to_string();
        let replace_all = value
            .get("replace_all")
            .and_then(JsonValue::as_bool)
            .unwrap_or(false);
        Some((old_string, new_string, replace_all))
    };

    if let Some(edits) = input.get("edits").and_then(JsonValue::as_array) {
        return edits.iter().filter_map(single).collect();
    }
    single(input).into_iter().collect()
}

/// The display form of a committed change.
fn display_change(change: &ProviderExecutedFileChange) -> FileChange {
    match &change.kind {
        ProviderExecutedFileChangeKind::Add { content } => FileChange::Add {
            content: content.clone(),
        },
        ProviderExecutedFileChangeKind::Delete { content } => FileChange::Delete {
            content: content.clone(),
        },
        ProviderExecutedFileChangeKind::Update {
            old_content,
            new_content,
        } => FileChange::Update {
            unified_diff: unified_diff(old_content, new_content),
            move_path: None,
        },
    }
}

fn unified_diff(old_content: &str, new_content: &str) -> String {
    similar::TextDiff::from_lines(old_content, new_content)
        .unified_diff()
        .context_radius(3)
        .to_string()
}

#[cfg(test)]
#[path = "tools_tests.rs"]
mod tests;
