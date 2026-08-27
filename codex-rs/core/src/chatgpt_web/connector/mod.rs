//! FORK: connector mode (`[chatgpt_web] tools = "connector"`).
//!
//! ChatGPT calls the Codex tools natively through a custom MCP connector that
//! points at the shared `codex chatgpt-web daemon`. This module has two halves:
//!
//! - the session side (this file + `client.rs` + `connector_attach.rs`): the
//!   `ConnectorBroker` seam the provider's turn loop talks to, the loopback
//!   client of the daemon's control API, and the browser-side @mention;
//! - the daemon side (`daemon/`): single shared instance owning the tunnel, the
//!   public MCP server with the fixed contract (`contract.rs`), the turn broker
//!   and the connector registry.

pub(crate) mod client;
pub(crate) mod connector_attach;
pub mod contract;
pub mod daemon;

use crate::chatgpt_web::connector::contract::CallTarget;
use crate::chatgpt_web::connector::contract::ExecTool;
use crate::chatgpt_web::connector::contract::ToolKind;
use crate::chatgpt_web::connector::contract::ToolSummary;
use codex_protocol::DEFAULT_FUNCTION_NAMESPACE;
use codex_protocol::ThreadId;
use codex_protocol::models::FunctionCallOutputPayload;
use codex_tools::ToolSpec;
use futures::future::BoxFuture;
use tokio::sync::mpsc;
use tokio::sync::oneshot;

/// The session-side half of the connector mode.
///
/// The provider's turn loop (`chatgpt_web::run_connector_turn`) drives one
/// implementation of this per turn: `begin_turn` opens a broker turn and
/// returns the `turn_token` plus the channel the daemon's tool calls arrive on,
/// `prompt_contract` yields the contract lines the prompt carries, and
/// `end_turn` retires the turn.
pub(crate) trait ConnectorBroker: Send + Sync + std::fmt::Debug {
    /// Registers a turn with the daemon and starts delivering the tool calls
    /// ChatGPT makes during it.
    fn begin_turn<'a>(&'a self, params: BeginTurn<'a>) -> BoxFuture<'a, Result<ConnectorTurn, String>>;

    /// The contract lines the prompt carries for this turn: the connector name,
    /// how to pass the `turn_token`, and what the six tools are for.
    fn prompt_contract(&self, turn: &ConnectorTurn) -> Vec<String>;

    /// Retires the turn (nothing more will be executed under this `turn_token`).
    fn end_turn<'a>(&'a self, turn_token: &'a str, reason: &'a str) -> BoxFuture<'a, ()>;
}

/// What `begin_turn` needs to register a turn.
pub(crate) struct BeginTurn<'a> {
    pub(crate) thread_id: ThreadId,
    pub(crate) turn_id: &'a str,
    pub(crate) conversation_id: Option<&'a str>,
    /// The tools the owning Codex turn announced, already reduced.
    pub(crate) tools: Vec<ToolSummary>,
    pub(crate) exec_tool: ExecTool,
    pub(crate) apply_patch: bool,
    pub(crate) ttl_ms: u64,
    /// Longest `begin_turn` waits for the connector to be `Verified`.
    pub(crate) ready_timeout: std::time::Duration,
}

/// A live connector turn: its token and the stream of tool calls.
pub(crate) struct ConnectorTurn {
    pub(crate) turn_token: String,
    pub(crate) connector_name: String,
    pub(crate) requests: mpsc::Receiver<ToolRequest>,
}

/// One tool call ChatGPT made, to run on the Codex side.
///
/// The provider turns `target` into a `FunctionCall`/`CustomToolCall` item, lets
/// Codex execute it, then sends the `FunctionCallOutputPayload` back through
/// `respond` — which the client posts to the daemon as the call result.
pub(crate) struct ToolRequest {
    pub(crate) call_id: String,
    pub(crate) target: CallTarget,
    pub(crate) respond: oneshot::Sender<FunctionCallOutputPayload>,
}

/// FORK: reduces the turn's announced tools to what the daemon's contract
/// resolver needs, and reports which exec tool and whether a free-form
/// `apply_patch` are available.
///
/// Namespaced tools (MCP servers) keep their namespace so `codex_tool_call`
/// can address them; the default function namespace is flattened to `None`, the
/// way `FunctionCall { namespace: None }` is dispatched.
pub(crate) fn tool_summaries(tools: &[ToolSpec]) -> (Vec<ToolSummary>, ExecTool, bool) {
    let mut summaries: Vec<ToolSummary> = Vec::new();
    let mut push = |name: &str, namespace: Option<String>, kind: ToolKind, description: String| {
        summaries.push(ToolSummary {
            name: name.to_string(),
            namespace,
            kind,
            description,
            schema: None,
        });
    };

    for tool in tools {
        match tool {
            ToolSpec::Function(function) => push(
                &function.name,
                None,
                ToolKind::Function,
                function.description.clone(),
            ),
            ToolSpec::Freeform(freeform) => push(
                &freeform.name,
                None,
                ToolKind::Freeform,
                freeform.description.clone(),
            ),
            ToolSpec::Namespace(namespace) => {
                let ns = (namespace.name != DEFAULT_FUNCTION_NAMESPACE)
                    .then(|| namespace.name.clone());
                for tool in &namespace.tools {
                    use codex_tools::ResponsesApiNamespaceTool;
                    match tool {
                        ResponsesApiNamespaceTool::Function(function) => push(
                            &function.name,
                            ns.clone(),
                            ToolKind::Function,
                            function.description.clone(),
                        ),
                        ResponsesApiNamespaceTool::Custom(freeform) => push(
                            &freeform.name,
                            ns.clone(),
                            ToolKind::Freeform,
                            freeform.description.clone(),
                        ),
                    }
                }
            }
            // `tool_search` and `web_search` are not offered to the connector:
            // ChatGPT has its own search and the search tool is a Codex-internal
            // affordance the contract does not expose.
            ToolSpec::ToolSearch { .. } | ToolSpec::WebSearch { .. } => {}
        }
    }

    // The exec tool the model would have used itself, so `codex_exec` maps onto
    // the same one. Unified exec (`exec_command`) is preferred; the legacy
    // `shell` is the fallback.
    let has = |name: &str| {
        summaries
            .iter()
            .any(|tool| tool.namespace.is_none() && tool.name == name)
    };
    let exec_tool = if has("exec_command") {
        ExecTool::ExecCommand
    } else {
        ExecTool::Shell
    };
    let apply_patch = summaries
        .iter()
        .any(|tool| tool.namespace.is_none() && tool.name == "apply_patch" && tool.kind == ToolKind::Freeform);

    (summaries, exec_tool, apply_patch)
}

#[cfg(test)]
#[path = "mod_tests.rs"]
mod tests;
