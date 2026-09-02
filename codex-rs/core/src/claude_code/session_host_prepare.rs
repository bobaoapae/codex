//! Construction and pre-launch admission for a Claude session host.

use super::ClaudeCodeWorkspace;
use super::SessionClaudeHost;
use crate::ownership::authorize_claude_provider;
use crate::session::session::Session;
use crate::session::step_context::StepContext;
use crate::tools::context::SharedTurnDiffTracker;
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

pub(super) async fn prepare(
    session: Arc<Session>,
    step_context: Arc<StepContext>,
    tracker: SharedTurnDiffTracker,
    cancel: CancellationToken,
) -> Result<(ClaudeCodeWorkspace, Arc<SessionClaudeHost>), String> {
    let turn = step_context.turn.as_ref();
    let environment = turn
        .environments
        .primary()
        .ok_or_else(|| "Claude provider has no selected execution environment".to_string())?;
    let provider_access = authorize_claude_provider(session.as_ref(), turn, environment).await?;
    let cwd = turn.config.cwd.clone();
    let mut workspace = ClaudeCodeWorkspace::from_config(turn.config.as_ref());
    if provider_access.is_read_only() {
        workspace.sandbox = codex_protocol::protocol::SandboxPolicy::new_read_only_policy();
        workspace.writable_roots.clear();
    }
    // A degraded request says why, so the agent reports instead of retrying a
    // write it cannot make. The next request re-runs this and may recover.
    workspace.ownership_notice = provider_access.ownership_notice().map(str::to_string);
    workspace.permission_mode = super::super::permission_mode_for_access(
        &workspace.sandbox,
        turn.config.permissions.approval_policy.value(),
        provider_access.requires_tool_authorization(),
    );
    let host = Arc::new(SessionClaudeHost::new(
        session,
        step_context,
        tracker,
        cancel,
        provider_access,
        cwd,
    ));
    Ok((workspace, host))
}
