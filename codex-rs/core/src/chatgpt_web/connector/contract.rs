//! FORK: the fixed tool contract the connector exposes to ChatGPT.
//!
//! ChatGPT caches a connector's tool set by connector identity, and one
//! connector serves every Codex session and agent on this machine, so the set
//! cannot mirror whatever tools a given turn happens to announce. Instead six
//! stable tools are published, and every call carries the `turn_token` of the
//! Codex turn that should execute it. The daemon resolves each call against the
//! tools that turn announced (`ToolSummary`) and forwards a concrete
//! `CallTarget` to the owning session, which runs it through the normal Codex
//! sandbox and approval path.
//!
//! Changing the contract means changing `CONTRACT_VERSION`: the registry then
//! creates a connector under a new name so ChatGPT's cached copy is left behind.

use rmcp::ErrorData as McpError;
use rmcp::model::CallToolResult;
use rmcp::model::ContentBlock;
use rmcp::model::JsonObject;
use rmcp::model::Tool;
use serde::Deserialize;
use serde::Serialize;
use serde_json::Value;
use serde_json::json;
use std::borrow::Cow;
use std::sync::Arc;

/// Bumped whenever a tool name, argument, or semantics changes.
pub const CONTRACT_VERSION: u32 = 1;

/// Shortest and longest `turn_token` the contract accepts.
pub const TURN_TOKEN_MIN_LEN: usize = 20;
pub const TURN_TOKEN_MAX_LEN: usize = 256;

/// Upper bound on `yield_time_ms` for `codex_exec`/`codex_write_stdin`; ChatGPT
/// serializes tool calls within a response, so a long yield stalls the whole
/// answer (spike S6).
pub const MAX_YIELD_TIME_MS: u64 = 30_000;
pub const MIN_YIELD_TIME_MS: u64 = 250;

/// Largest page `codex_tool_inventory` returns.
pub const MAX_INVENTORY_LIMIT: usize = 50;
const DEFAULT_INVENTORY_LIMIT: usize = 20;

pub const CODEX_EXEC: &str = "codex_exec";
pub const CODEX_WRITE_STDIN: &str = "codex_write_stdin";
pub const CODEX_APPLY_PATCH: &str = "codex_apply_patch";
pub const CODEX_VIEW_IMAGE: &str = "codex_view_image";
pub const CODEX_TOOL_INVENTORY: &str = "codex_tool_inventory";
pub const CODEX_TOOL_CALL: &str = "codex_tool_call";

/// Every tool name, in the order they are advertised.
pub const TOOL_NAMES: [&str; 6] = [
    CODEX_EXEC,
    CODEX_WRITE_STDIN,
    CODEX_APPLY_PATCH,
    CODEX_VIEW_IMAGE,
    CODEX_TOOL_INVENTORY,
    CODEX_TOOL_CALL,
];

/// A tool the owning Codex turn announced, reduced to what resolution needs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolSummary {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub namespace: Option<String>,
    #[serde(default)]
    pub kind: ToolKind,
    #[serde(default)]
    pub description: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub schema: Option<Value>,
}

/// How a tool is invoked on the Codex side.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolKind {
    /// A JSON-arguments function call (`ResponseItem::FunctionCall`).
    #[default]
    Function,
    /// A free-form text tool such as `apply_patch` (`ResponseItem::CustomToolCall`).
    Freeform,
}

/// The concrete call the daemon hands to the owning session.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum CallTarget {
    Function {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        namespace: Option<String>,
        name: String,
        arguments: Value,
    },
    Custom {
        name: String,
        input: String,
    },
}

impl CallTarget {
    /// Name shown in errors and logs.
    pub fn display_name(&self) -> String {
        match self {
            Self::Function {
                namespace: Some(namespace),
                name,
                ..
            } => format!("{namespace}.{name}"),
            Self::Function { name, .. } | Self::Custom { name, .. } => name.clone(),
        }
    }
}

/// Which contract tool a request named.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ContractTool {
    Exec,
    WriteStdin,
    ApplyPatch,
    ViewImage,
    ToolInventory,
    ToolCall,
}

impl ContractTool {
    fn from_name(name: &str) -> Option<Self> {
        match name {
            CODEX_EXEC => Some(Self::Exec),
            CODEX_WRITE_STDIN => Some(Self::WriteStdin),
            CODEX_APPLY_PATCH => Some(Self::ApplyPatch),
            CODEX_VIEW_IMAGE => Some(Self::ViewImage),
            CODEX_TOOL_INVENTORY => Some(Self::ToolInventory),
            CODEX_TOOL_CALL => Some(Self::ToolCall),
            _ => None,
        }
    }
}

/// A request that passed name and `turn_token` validation.
#[derive(Debug, Clone)]
pub struct ParsedCall {
    pub tool: ContractTool,
    pub turn_token: String,
    /// The arguments without `turn_token`.
    pub args: JsonObject,
}

/// What a contract call resolves to.
#[derive(Debug)]
pub enum Resolved {
    /// Answered by the daemon itself (`codex_tool_inventory`).
    Local(CallToolResult),
    /// Forwarded to the owning session.
    Forward(CallTarget),
}

/// Which exec tool the turn announced, so `codex_exec` maps onto the one the
/// model would have used itself.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExecTool {
    ExecCommand,
    Shell,
}

fn schema(properties: Value, required: &[&str]) -> Arc<JsonObject> {
    let mut object = JsonObject::new();
    object.insert("type".into(), json!("object"));
    object.insert("properties".into(), properties);
    object.insert("required".into(), json!(required));
    object.insert("additionalProperties".into(), json!(false));
    Arc::new(object)
}

fn turn_token_property() -> Value {
    json!({
        "type": "string",
        "minLength": TURN_TOKEN_MIN_LEN,
        "maxLength": TURN_TOKEN_MAX_LEN,
        "description": "The turn_token from the Codex prompt, passed unchanged."
    })
}

fn tool(name: &'static str, description: &'static str, input_schema: Arc<JsonObject>) -> Tool {
    debug_assert!(description.len() <= 120, "{name}: description too long");
    Tool::new(
        Cow::Borrowed(name),
        Cow::Borrowed(description),
        input_schema,
    )
}

/// The advertised tool list, identical for every session.
pub fn tools() -> Vec<Tool> {
    vec![
        tool(
            CODEX_EXEC,
            "Run a shell command in the Codex workspace; returns output so far after yield_time_ms.",
            schema(
                json!({
                    "turn_token": turn_token_property(),
                    "cmd": {"type": "string", "description": "Command line to run."},
                    "workdir": {"type": "string", "description": "Working directory (default: workspace)."},
                    "yield_time_ms": {"type": "integer", "minimum": MIN_YIELD_TIME_MS, "maximum": MAX_YIELD_TIME_MS,
                        "description": "How long to wait for output before returning (keep ≤ 30000)."},
                    "max_output_tokens": {"type": "integer", "minimum": 1},
                    "tty": {"type": "boolean"}
                }),
                &["turn_token", "cmd"],
            ),
        ),
        tool(
            CODEX_WRITE_STDIN,
            "Send input to a running codex_exec session and read more output.",
            schema(
                json!({
                    "turn_token": turn_token_property(),
                    "session_id": {"type": "integer", "description": "Session id returned by codex_exec."},
                    "chars": {"type": "string", "description": "Characters to write; empty just polls."},
                    "yield_time_ms": {"type": "integer", "minimum": MIN_YIELD_TIME_MS, "maximum": MAX_YIELD_TIME_MS},
                    "max_output_tokens": {"type": "integer", "minimum": 1}
                }),
                &["turn_token", "session_id"],
            ),
        ),
        tool(
            CODEX_APPLY_PATCH,
            "Apply a Codex apply_patch envelope (*** Begin Patch … *** End Patch) to the workspace.",
            schema(
                json!({
                    "turn_token": turn_token_property(),
                    "patch": {"type": "string", "description": "The full apply_patch text."}
                }),
                &["turn_token", "patch"],
            ),
        ),
        tool(
            CODEX_VIEW_IMAGE,
            "Attach a local image file from the workspace to the conversation.",
            schema(
                json!({
                    "turn_token": turn_token_property(),
                    "path": {"type": "string", "description": "Path to the image file."},
                    "detail": {"type": "string", "enum": ["low", "high", "original"]}
                }),
                &["turn_token", "path"],
            ),
        ),
        tool(
            CODEX_TOOL_INVENTORY,
            "List the tools available to this Codex turn (names, descriptions, optional schemas).",
            schema(
                json!({
                    "turn_token": turn_token_property(),
                    "query": {"type": "string", "description": "Substring filter on name/description."},
                    "offset": {"type": "integer", "minimum": 0},
                    "limit": {"type": "integer", "minimum": 1, "maximum": MAX_INVENTORY_LIMIT},
                    "include_schema": {"type": "boolean"}
                }),
                &["turn_token"],
            ),
        ),
        tool(
            CODEX_TOOL_CALL,
            "Call any tool from codex_tool_inventory by name (MCP tools need their namespace).",
            schema(
                json!({
                    "turn_token": turn_token_property(),
                    "namespace": {"type": "string"},
                    "name": {"type": "string"},
                    "arguments": {"type": "object", "description": "JSON arguments for function tools."},
                    "input": {"type": "string", "description": "Raw input for free-form tools."}
                }),
                &["turn_token", "name"],
            ),
        ),
    ]
}

/// Validates the tool name and the `turn_token` argument.
pub fn parse(name: &str, arguments: Option<&JsonObject>) -> Result<ParsedCall, McpError> {
    let tool = ContractTool::from_name(name)
        .ok_or_else(|| McpError::invalid_params(format!("unknown tool `{name}`"), None))?;
    let mut args = arguments.cloned().unwrap_or_default();
    let turn_token = match args.remove("turn_token") {
        Some(Value::String(token)) => token,
        Some(_) => {
            return Err(McpError::invalid_params(
                "turn_token must be a string",
                None,
            ));
        }
        None => {
            return Err(McpError::invalid_params(
                "turn_token is required: pass the turn_token from the Codex prompt unchanged",
                None,
            ));
        }
    };
    let token_len = turn_token.len();
    if !(TURN_TOKEN_MIN_LEN..=TURN_TOKEN_MAX_LEN).contains(&token_len)
        || !turn_token
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_')
    {
        return Err(McpError::invalid_params(
            "turn_token is malformed; pass it exactly as it appears in the Codex prompt",
            None,
        ));
    }
    Ok(ParsedCall {
        tool,
        turn_token,
        args,
    })
}

/// Everything resolution needs to know about the owning turn.
#[derive(Debug, Clone)]
pub struct TurnTools {
    pub tools: Arc<[ToolSummary]>,
    pub exec_tool: ExecTool,
    /// Whether the turn announced a free-form `apply_patch` tool.
    pub apply_patch: bool,
    pub exec_default_yield_ms: u64,
}

impl TurnTools {
    fn find(&self, namespace: Option<&str>, name: &str) -> Option<&ToolSummary> {
        self.tools
            .iter()
            .find(|tool| tool.name == name && tool.namespace.as_deref() == namespace)
    }

    fn has_function(&self, name: &str) -> bool {
        self.find(None, name)
            .is_some_and(|tool| tool.kind == ToolKind::Function)
    }
}

fn take_u64(args: &JsonObject, key: &str) -> Result<Option<u64>, McpError> {
    match args.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(value) => value
            .as_u64()
            .or_else(|| value.as_f64().filter(|f| *f >= 0.0).map(|f| f as u64))
            .map(Some)
            .ok_or_else(|| {
                McpError::invalid_params(format!("{key} must be a non-negative integer"), None)
            }),
    }
}

fn take_str<'a>(args: &'a JsonObject, key: &str) -> Result<Option<&'a str>, McpError> {
    match args.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(text)) => Ok(Some(text.as_str())),
        Some(_) => Err(McpError::invalid_params(
            format!("{key} must be a string"),
            None,
        )),
    }
}

fn require_str<'a>(args: &'a JsonObject, key: &str) -> Result<&'a str, McpError> {
    take_str(args, key)?.ok_or_else(|| McpError::invalid_params(format!("{key} is required"), None))
}

fn clamp_yield(requested: Option<u64>, default: u64) -> u64 {
    requested
        .unwrap_or(default)
        .clamp(MIN_YIELD_TIME_MS, MAX_YIELD_TIME_MS)
}

/// Resolves a parsed call against the tools the owning turn announced.
pub fn to_call(parsed: &ParsedCall, turn: &TurnTools) -> Result<Resolved, McpError> {
    let args = &parsed.args;
    match parsed.tool {
        ContractTool::Exec => {
            let cmd = require_str(args, "cmd")?;
            let workdir = take_str(args, "workdir")?;
            let yield_time_ms =
                clamp_yield(take_u64(args, "yield_time_ms")?, turn.exec_default_yield_ms);
            let max_output_tokens = take_u64(args, "max_output_tokens")?;
            let tty = args.get("tty").and_then(Value::as_bool);
            let arguments = match turn.exec_tool {
                ExecTool::ExecCommand => {
                    let mut object = JsonObject::new();
                    object.insert("cmd".into(), json!(cmd));
                    if let Some(workdir) = workdir {
                        object.insert("workdir".into(), json!(workdir));
                    }
                    object.insert("yield_time_ms".into(), json!(yield_time_ms));
                    if let Some(max) = max_output_tokens {
                        object.insert("max_output_tokens".into(), json!(max));
                    }
                    if let Some(tty) = tty {
                        object.insert("tty".into(), json!(tty));
                    }
                    Value::Object(object)
                }
                ExecTool::Shell => {
                    let mut object = JsonObject::new();
                    object.insert("command".into(), json!(cmd));
                    if let Some(workdir) = workdir {
                        object.insert("workdir".into(), json!(workdir));
                    }
                    object.insert("timeout_ms".into(), json!(yield_time_ms));
                    Value::Object(object)
                }
            };
            let name = match turn.exec_tool {
                ExecTool::ExecCommand => "exec_command",
                ExecTool::Shell => "shell",
            };
            Ok(Resolved::Forward(CallTarget::Function {
                namespace: None,
                name: name.to_string(),
                arguments,
            }))
        }
        ContractTool::WriteStdin => {
            if turn.exec_tool != ExecTool::ExecCommand || !turn.has_function("write_stdin") {
                return Err(McpError::invalid_params(
                    "this Codex turn has no write_stdin tool; use codex_exec instead",
                    None,
                ));
            }
            let session_id = take_u64(args, "session_id")?
                .ok_or_else(|| McpError::invalid_params("session_id is required", None))?;
            let mut object = JsonObject::new();
            object.insert("session_id".into(), json!(session_id));
            object.insert(
                "chars".into(),
                json!(take_str(args, "chars")?.unwrap_or_default()),
            );
            object.insert(
                "yield_time_ms".into(),
                json!(clamp_yield(
                    take_u64(args, "yield_time_ms")?,
                    turn.exec_default_yield_ms
                )),
            );
            if let Some(max) = take_u64(args, "max_output_tokens")? {
                object.insert("max_output_tokens".into(), json!(max));
            }
            Ok(Resolved::Forward(CallTarget::Function {
                namespace: None,
                name: "write_stdin".to_string(),
                arguments: Value::Object(object),
            }))
        }
        ContractTool::ApplyPatch => {
            if !turn.apply_patch {
                return Err(McpError::invalid_params(
                    "this Codex turn does not offer apply_patch; edit files through codex_exec instead",
                    None,
                ));
            }
            let patch = require_str(args, "patch")?;
            Ok(Resolved::Forward(CallTarget::Custom {
                name: "apply_patch".to_string(),
                input: patch.to_string(),
            }))
        }
        ContractTool::ViewImage => {
            if !turn.has_function("view_image") {
                return Err(McpError::invalid_params(
                    "this Codex turn has no view_image tool",
                    None,
                ));
            }
            let path = require_str(args, "path")?;
            let mut object = JsonObject::new();
            object.insert("path".into(), json!(path));
            if let Some(detail) = take_str(args, "detail")? {
                object.insert("detail".into(), json!(detail));
            }
            Ok(Resolved::Forward(CallTarget::Function {
                namespace: None,
                name: "view_image".to_string(),
                arguments: Value::Object(object),
            }))
        }
        ContractTool::ToolInventory => {
            let query = take_str(args, "query")?;
            let offset = take_u64(args, "offset")?.unwrap_or(0) as usize;
            let limit = take_u64(args, "limit")?
                .map(|limit| limit as usize)
                .unwrap_or(DEFAULT_INVENTORY_LIMIT)
                .clamp(1, MAX_INVENTORY_LIMIT);
            let include_schema = args
                .get("include_schema")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            Ok(Resolved::Local(inventory(
                &turn.tools,
                query,
                offset,
                limit,
                include_schema,
            )))
        }
        ContractTool::ToolCall => {
            let name = require_str(args, "name")?;
            let namespace = take_str(args, "namespace")?;
            let Some(summary) = turn.find(namespace, name) else {
                return Err(McpError::invalid_params(
                    format!(
                        "tool `{}` is not available in this Codex turn; check codex_tool_inventory",
                        match namespace {
                            Some(namespace) => format!("{namespace}.{name}"),
                            None => name.to_string(),
                        }
                    ),
                    None,
                ));
            };
            match summary.kind {
                ToolKind::Function => {
                    let arguments = match args.get("arguments") {
                        None | Some(Value::Null) => Value::Object(JsonObject::new()),
                        Some(Value::Object(object)) => Value::Object(object.clone()),
                        Some(Value::String(text)) => serde_json::from_str(text).map_err(|err| {
                            McpError::invalid_params(
                                format!("arguments must be a JSON object: {err}"),
                                None,
                            )
                        })?,
                        Some(_) => {
                            return Err(McpError::invalid_params(
                                "arguments must be a JSON object",
                                None,
                            ));
                        }
                    };
                    Ok(Resolved::Forward(CallTarget::Function {
                        namespace: summary.namespace.clone(),
                        name: summary.name.clone(),
                        arguments,
                    }))
                }
                ToolKind::Freeform => {
                    let input = match (take_str(args, "input")?, args.get("arguments")) {
                        (Some(input), _) => input.to_string(),
                        (None, Some(Value::String(text))) => text.clone(),
                        (None, Some(other)) if !other.is_null() => other.to_string(),
                        _ => {
                            return Err(McpError::invalid_params(
                                format!("tool `{name}` takes free-form text: pass it in `input`"),
                                None,
                            ));
                        }
                    };
                    Ok(Resolved::Forward(CallTarget::Custom {
                        name: summary.name.clone(),
                        input,
                    }))
                }
            }
        }
    }
}

/// Serves `codex_tool_inventory` from the turn's announced tools.
pub fn inventory(
    tools: &[ToolSummary],
    query: Option<&str>,
    offset: usize,
    limit: usize,
    include_schema: bool,
) -> CallToolResult {
    let needle = query.map(str::to_ascii_lowercase);
    let matching: Vec<&ToolSummary> = tools
        .iter()
        .filter(|tool| match needle.as_deref() {
            None => true,
            Some(needle) => {
                tool.name.to_ascii_lowercase().contains(needle)
                    || tool.description.to_ascii_lowercase().contains(needle)
                    || tool
                        .namespace
                        .as_deref()
                        .is_some_and(|namespace| namespace.to_ascii_lowercase().contains(needle))
            }
        })
        .collect();
    let total = matching.len();
    let page: Vec<Value> = matching
        .into_iter()
        .skip(offset)
        .take(limit)
        .map(|tool| {
            let mut entry = json!({
                "name": tool.name,
                "kind": tool.kind,
                "description": tool.description,
            });
            if let Some(namespace) = &tool.namespace {
                entry["namespace"] = json!(namespace);
            }
            if include_schema && let Some(schema) = &tool.schema {
                entry["schema"] = schema.clone();
            }
            entry
        })
        .collect();
    let body = json!({
        "total": total,
        "offset": offset,
        "limit": limit,
        "tools": page,
        "hint": "Call a listed tool with codex_tool_call {name, namespace?, arguments|input}.",
    });
    CallToolResult::success(vec![ContentBlock::text(body.to_string())])
}

#[cfg(test)]
#[path = "contract_tests.rs"]
mod tests;
