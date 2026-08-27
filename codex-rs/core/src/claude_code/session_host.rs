//! FORK: the [`ClaudeHost`] implementation backed by a live Codex turn.
//!
//! Everything the CLI can ask for needs session state: the approval UI is a
//! `Session` event with a oneshot reply, and the bridge dispatches through the
//! turn's own tool router. This type is constructed inside
//! `try_run_sampling_request`, where all of it is in scope, and handed to the
//! adapter as an `Arc<dyn ClaudeHost>`.
//!
//! Why this matters: without a host, a `--permission-mode auto` CLI has no one
//! to ask, so every "ask" decision it reaches is final. That is the mechanism
//! behind the "dotnet requires approval" dead ends — the child was not blocked
//! by policy, it was blocked by having no prompt surface.

use codex_protocol::approvals::ExecApprovalKind;
use codex_protocol::protocol::FileChange;
use codex_protocol::protocol::ReviewDecision;
use futures::FutureExt;
use futures::future::BoxFuture;
use serde_json::Value as JsonValue;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

use codex_tools::ToolName;
use codex_tools::ToolSpec;

use super::bridge::BRIDGE_SERVER_NAME;
use super::control::CanUseTool;
use super::control::ToolPermissionDecision;
use super::host::APPROVAL_TIMEOUT;
use super::host::ClaudeHost;
use crate::session::session::Session;
use crate::session::step_context::StepContext;
use crate::tools::context::SharedTurnDiffTracker;
use crate::tools::context::ToolCallSource;
use crate::tools::context::ToolPayload;
use crate::tools::router::ToolCall;

pub(crate) struct SessionClaudeHost {
    session: Arc<Session>,
    step_context: Arc<StepContext>,
    tracker: SharedTurnDiffTracker,
    cancel: CancellationToken,
    /// Distinguishes the child's bridge calls from one another in the parent's
    /// transcript.
    bridge_calls: std::sync::atomic::AtomicU64,
}

impl std::fmt::Debug for SessionClaudeHost {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SessionClaudeHost").finish_non_exhaustive()
    }
}

impl SessionClaudeHost {
    pub(crate) fn new(
        session: Arc<Session>,
        step_context: Arc<StepContext>,
        tracker: SharedTurnDiffTracker,
        cancel: CancellationToken,
    ) -> Self {
        Self {
            session,
            step_context,
            tracker,
            cancel,
            bridge_calls: std::sync::atomic::AtomicU64::new(0),
        }
    }

    fn next_bridge_call_id(&self) -> u64 {
        self.bridge_calls
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    }

    /// Asks the user about a command the CLI wants to run.
    async fn approve_command(
        &self,
        call_id: String,
        command: Vec<String>,
        reason: Option<String>,
    ) -> ReviewDecision {
        let turn_context = self.step_context.turn.as_ref();
        #[allow(deprecated)]
        let cwd = turn_context.cwd.clone();
        self.session
            .request_command_approval(
                turn_context,
                ExecApprovalKind::Command,
                call_id,
                /*approval_id*/ None,
                /*environment_id*/ None,
                command,
                cwd.into(),
                reason,
                /*network_approval_context*/ None,
                /*proposed_execpolicy_amendment*/ None,
                /*additional_permissions*/ None,
                /*available_decisions*/ None,
                /*plugin_attribution_override*/ None,
            )
            .await
    }

    /// Asks the user about a file the CLI wants to write.
    async fn approve_patch(
        &self,
        call_id: String,
        changes: HashMap<PathBuf, FileChange>,
        reason: Option<String>,
    ) -> ReviewDecision {
        self.session
            .request_patch_approval(
                self.step_context.turn.as_ref(),
                call_id,
                changes,
                reason,
                /*grant_root*/ None,
            )
            .await
    }
}

impl ClaudeHost for SessionClaudeHost {
    fn approve_tool<'a>(
        &'a self,
        request: &'a CanUseTool,
    ) -> BoxFuture<'a, ToolPermissionDecision> {
        async move {
            // `tool_use_id` is what the CLI will use to correlate the answer;
            // reusing it as the approval id keeps the two views in step.
            let call_id = request
                .tool_use_id
                .clone()
                .unwrap_or_else(|| format!("claude-{}", request.tool_name));
            let reason = approval_reason(request);

            let decision = {
                let pending = match ApprovalShape::of(request) {
                    ApprovalShape::Command(command) => {
                        self.approve_command(call_id, command, reason).boxed()
                    }
                    ApprovalShape::Patch(changes) => {
                        self.approve_patch(call_id, changes, reason).boxed()
                    }
                };
                tokio::select! {
                    // A cancelled turn will never produce an answer.
                    _ = self.cancel.cancelled() => ReviewDecision::Abort,
                    decision = tokio::time::timeout(APPROVAL_TIMEOUT, pending) => {
                        decision.unwrap_or(ReviewDecision::TimedOut)
                    }
                }
            };

            match decision {
                ReviewDecision::Approved
                | ReviewDecision::ApprovedExecpolicyAmendment { .. }
                | ReviewDecision::ApprovedMcpPolicyAmendment
                | ReviewDecision::NetworkPolicyAmendment { .. } => ToolPermissionDecision::Allow {
                    updated_input: None,
                    updated_permissions: None,
                },
                // Approving for the session means the CLI should stop asking.
                // Handing back its own suggestions is how it records that.
                ReviewDecision::ApprovedForSession => ToolPermissionDecision::Allow {
                    updated_input: None,
                    updated_permissions: request.permission_suggestions.clone(),
                },
                ReviewDecision::Denied { rejection } => ToolPermissionDecision::Deny {
                    message: rejection,
                    interrupt: false,
                },
                // Denying without an interrupt on a timeout is deliberate: the
                // agent loses one command and can work around it, where an
                // interrupt would lose the whole turn.
                ReviewDecision::TimedOut => ToolPermissionDecision::Deny {
                    message: "No decision arrived in time; this tool call was not approved."
                        .to_string(),
                    interrupt: false,
                },
                ReviewDecision::Abort => ToolPermissionDecision::Deny {
                    message: "The parent session ended this turn.".to_string(),
                    interrupt: true,
                },
            }
        }
        .boxed()
    }

    fn call_bridge_tool<'a>(
        &'a self,
        name: &'a str,
        arguments: JsonValue,
    ) -> BoxFuture<'a, Result<JsonValue, String>> {
        async move {
            let collaboration_namespace = collaboration_namespace(&self.step_context.turn.config);
            let Some(tool_name) = bridge_tool_name(name, &collaboration_namespace) else {
                return Err(format!("tool `{name}` is not available to this agent"));
            };
            let router = Arc::clone(&self.step_context.tool_router);
            if !router
                .model_visible_specs()
                .iter()
                .any(|spec| spec_matches(spec, &tool_name))
            {
                return Err(format!("tool `{name}` is not available in this session"));
            }

            let call = ToolCall {
                tool_name,
                // A call id of its own keeps the child's bridge traffic from
                // colliding with the parent's own tool calls in the transcript.
                call_id: format!("claude-bridge-{}", self.next_bridge_call_id()),
                payload: ToolPayload::Function {
                    arguments: arguments.to_string(),
                },
                // Empty, not absent: the marker that says this payload is
                // plaintext rather than an encrypted backend blob.
                encrypted_function_args: Some(Vec::new()),
            };

            let result = router
                .dispatch_tool_call_with_code_mode_result(
                    Arc::clone(&self.session),
                    Arc::clone(&self.step_context),
                    self.cancel.child_token(),
                    Arc::clone(&self.tracker),
                    call.clone(),
                    // The child speaks plaintext: it cannot decrypt the
                    // encrypted `message` form the backend agents use.
                    ToolCallSource::DirectPlaintextMessage,
                )
                .await
                .map_err(|err| err.to_string())?;
            Ok(result.result.code_mode_result(&call.payload))
        }
        .boxed()
    }

    fn bridge_tool_specs(&self) -> BoxFuture<'_, Vec<JsonValue>> {
        async move {
            bridge_spec_entries(
                &self.step_context.tool_router.model_visible_specs(),
                &collaboration_namespace(&self.step_context.turn.config),
            )
        }
        .boxed()
    }
}

/// Tools a Claude child may reach through the bridge.
///
/// Deliberately a *whitelist*. The child already has its own `Bash`, `Read`,
/// `Edit` and friends running against the same working tree, so exposing
/// Codex's equivalents would only give it two ways to do the same thing — and
/// the Codex ones would run under the parent's sandbox while the child's do not.
/// What it genuinely lacks is a way to talk to its parent and to reach the
/// session's connected MCP servers.
const BRIDGE_COLLABORATION_TOOLS: &[&str] = &[
    "send_message",
    "followup_task",
    "list_agents",
    "wait_agent",
    "spawn_agent",
    "interrupt_agent",
];

/// Bare-named Codex tools the child may reach.
const BRIDGE_PLAIN_TOOLS: &[&str] = &["update_plan", "claude_accounts", "claude_account_select"];

/// Tools that are never bridged: the child has its own.
const BRIDGE_DENIED_TOOLS: &[&str] = &[
    "shell",
    "unified_exec",
    "apply_patch",
    "read_file",
    "view_image",
    "tool_search",
    "exec",
    "wait",
];

/// Resolves the name the CLI used (`mcp__codex__<tool>`) to a Codex tool.
///
/// Returns `None` for anything outside the allow-list, so an unexpected name is
/// refused by identity rather than by whatever the router happens to do with it.
fn bridge_tool_name(name: &str, collaboration_namespace: &str) -> Option<ToolName> {
    let bare = name
        .strip_prefix("mcp__")
        .and_then(|rest| rest.strip_prefix(BRIDGE_SERVER_NAME))
        .and_then(|rest| rest.strip_prefix("__"))
        .unwrap_or(name);
    if BRIDGE_DENIED_TOOLS.contains(&bare) {
        return None;
    }
    if BRIDGE_COLLABORATION_TOOLS.contains(&bare) {
        return Some(ToolName::namespaced(collaboration_namespace, bare));
    }
    if BRIDGE_PLAIN_TOOLS.contains(&bare) {
        return Some(ToolName::plain(bare));
    }
    // A session MCP tool, named `<server>__<tool>` by the caller.
    let (namespace, tool) = bare.split_once("__")?;
    if namespace.is_empty() || tool.is_empty() || BRIDGE_DENIED_TOOLS.contains(&namespace) {
        return None;
    }
    Some(ToolName::namespaced(namespace, tool))
}

/// FORK: the namespace the collaboration tools live under for this session.
///
/// `features.multi_agent_v2.tool_namespace` renames it (this user's config sets
/// `collab_agents`), so hardcoding `collaboration` would make every bridged
/// `send_message` resolve to a tool that does not exist.
fn collaboration_namespace(config: &crate::config::Config) -> String {
    config
        .multi_agent_v2
        .tool_namespace
        .clone()
        .unwrap_or_else(|| "collaboration".to_string())
}

/// The name a Codex tool is advertised under to the child.
///
/// The inverse of [`bridge_tool_name`]: allow-listed tools keep their bare
/// name, everything else is namespaced so the two can be told apart.
fn bridge_exposed_name(tool_name: &ToolName, collaboration_namespace: &str) -> Option<String> {
    if BRIDGE_DENIED_TOOLS.contains(&tool_name.name.as_str()) {
        return None;
    }
    match tool_name.namespace.as_deref() {
        Some(namespace) if namespace == collaboration_namespace => BRIDGE_COLLABORATION_TOOLS
            .contains(&tool_name.name.as_str())
            .then(|| tool_name.name.clone()),
        None => BRIDGE_PLAIN_TOOLS
            .contains(&tool_name.name.as_str())
            .then(|| tool_name.name.clone()),
        Some(namespace) => Some(format!("{namespace}__{}", tool_name.name)),
    }
}

/// The `tools/list` entries for everything the child may reach.
fn bridge_spec_entries(specs: &[ToolSpec], collaboration_namespace: &str) -> Vec<JsonValue> {
    let mut entries = Vec::new();
    for spec in specs {
        match spec {
            ToolSpec::Function(function) => {
                push_bridge_entry(&mut entries, None, function, collaboration_namespace);
            }
            ToolSpec::Namespace(namespace) => {
                for tool in &namespace.tools {
                    if let codex_tools::ResponsesApiNamespaceTool::Function(function) = tool {
                        push_bridge_entry(
                            &mut entries,
                            Some(namespace.name.as_str()),
                            function,
                            collaboration_namespace,
                        );
                    }
                }
            }
            // Namespaces aside, hosted tools and tool search have no direct
            // call shape a bridge could forward.
            _ => {}
        }
    }
    entries
}

fn push_bridge_entry(
    entries: &mut Vec<JsonValue>,
    namespace: Option<&str>,
    function: &codex_tools::ResponsesApiTool,
    collaboration_namespace: &str,
) {
    let tool_name = ToolName::new(namespace.map(str::to_string), function.name.as_str());
    let Some(exposed) = bridge_exposed_name(&tool_name, collaboration_namespace) else {
        return;
    };
    entries.push(serde_json::json!({
        "name": exposed,
        "description": function.description,
        "inputSchema": function.parameters,
    }));
}

/// Whether an advertised tool spec is the one this call names.
fn spec_matches(spec: &ToolSpec, tool_name: &ToolName) -> bool {
    match spec {
        ToolSpec::Function(function) => {
            tool_name.namespace.is_none() && function.name == tool_name.name
        }
        ToolSpec::Namespace(namespace) => {
            tool_name.namespace.as_deref() == Some(namespace.name.as_str())
                && namespace.tools.iter().any(|tool| match tool {
                    codex_tools::ResponsesApiNamespaceTool::Function(function) => {
                        function.name == tool_name.name
                    }
                    codex_tools::ResponsesApiNamespaceTool::Custom(custom) => {
                        custom.name == tool_name.name
                    }
                })
        }
        _ => false,
    }
}

/// Which approval surface a Claude tool maps onto.
enum ApprovalShape {
    Command(Vec<String>),
    Patch(HashMap<PathBuf, FileChange>),
}

impl ApprovalShape {
    fn of(request: &CanUseTool) -> Self {
        match request.tool_name.as_str() {
            "Bash" => Self::Command(vec![
                "bash".to_string(),
                "-lc".to_string(),
                request
                    .input
                    .get("command")
                    .and_then(JsonValue::as_str)
                    .unwrap_or_default()
                    .to_string(),
            ]),
            "Edit" | "MultiEdit" | "Write" | "NotebookEdit" => Self::Patch(patch_preview(request)),
            // Everything else — `Read`, `WebFetch`, an MCP tool — has no
            // dedicated surface. A synthesized command still shows the user what
            // is being asked and what it would touch.
            other => Self::Command(vec![other.to_string(), compact_json(&request.input)]),
        }
    }
}

/// What the approval dialog shows for a write.
///
/// The CLI does not hand over the pre-edit contents at permission time, so the
/// preview describes the intended change rather than a true diff. Showing the
/// file and the new text is what the user needs to decide.
fn patch_preview(request: &CanUseTool) -> HashMap<PathBuf, FileChange> {
    let Some(path) = request
        .input
        .get("file_path")
        .or_else(|| request.input.get("notebook_path"))
        .and_then(JsonValue::as_str)
        .map(PathBuf::from)
    else {
        return HashMap::new();
    };
    let content = request
        .input
        .get("content")
        .or_else(|| request.input.get("new_string"))
        .and_then(JsonValue::as_str)
        .unwrap_or_default()
        .to_string();
    HashMap::from([(path, FileChange::Add { content })])
}

/// Why the CLI is asking, in one line for the approval dialog.
fn approval_reason(request: &CanUseTool) -> Option<String> {
    let mut parts = vec![format!("Claude agent tool `{}`", request.tool_name)];
    if let Some(blocked_path) = request.blocked_path.as_deref() {
        parts.push(format!("blocked path: {blocked_path}"));
    }
    if let Some(reason) = request
        .decision_reason
        .as_ref()
        .and_then(|reason| reason.get("description"))
        .and_then(JsonValue::as_str)
    {
        parts.push(reason.to_string());
    }
    Some(parts.join(" — "))
}

/// A one-line rendering of tool arguments, bounded so a huge payload cannot
/// blow up the approval dialog.
fn compact_json(value: &JsonValue) -> String {
    const MAX_CHARS: usize = 400;
    let rendered = value.to_string();
    if rendered.chars().count() <= MAX_CHARS {
        return rendered;
    }
    rendered.chars().take(MAX_CHARS).collect::<String>() + "…"
}

#[cfg(test)]
#[path = "session_host_tests.rs"]
mod tests;
