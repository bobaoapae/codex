//! FORK: what the Codex host still checks before a Claude child runs a tool.
//!
//! Path leases are gone; a subagent shares the root's dirty checkout and the
//! only workspace rule left is that it never resets, restores or stashes it.

use codex_utils_absolute_path::AbsolutePathBuf;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ClaudeProviderAccess {
    /// The root thread: the parent policy alone decides.
    Root,
    /// A multi-agent child sharing the root checkout.
    Subagent,
}

impl ClaudeProviderAccess {
    /// Whether the Claude CLI must route tool calls through the Codex host.
    ///
    /// A subagent is launched in `auto` mode even when the parent policy is
    /// `Never`, because only that mode reaches the host's `can_use_tool`
    /// callback where the destructive-Git denial lives.
    pub(crate) fn requires_tool_authorization(self) -> bool {
        matches!(self, Self::Subagent)
    }

    pub(crate) async fn authorize_claude_tool(
        &self,
        tool_name: &str,
        input: &serde_json::Value,
        _cwd: &AbsolutePathBuf,
    ) -> Result<(), String> {
        if !matches!(self, Self::Subagent) || tool_name != "Bash" {
            return Ok(());
        }
        let command = input
            .get("command")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| "Claude Bash request has no literal command".to_string())?;
        let words = vec!["bash".to_string(), "-lc".to_string(), command.to_string()];
        match codex_shell_command::classify_command(&words) {
            codex_shell_command::MutationIntent::DestructiveGit { .. } => {
                Err("subagents cannot execute destructive Git commands".to_string())
            }
            _ => Ok(()),
        }
    }
}
