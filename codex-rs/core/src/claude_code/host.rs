//! FORK: what the Claude CLI can ask of the Codex session hosting it.
//!
//! The adapter runs inside a stream task that owns nothing: no `Session`, no
//! turn context, no diff tracker. Answering a `can_use_tool` request or a bridge
//! tool call needs all of them. This trait is the seam — the adapter holds an
//! `Arc<dyn ClaudeHost>` and the turn supplies the implementation that has the
//! session in scope (`session_host.rs`).
//!
//! Keeping it a trait also keeps the adapter testable: the tests drive the
//! translation with a fake host instead of standing up a whole session.
//!
//! The methods return boxed futures rather than using `async fn`, because the
//! adapter holds the host behind `dyn` and async-in-trait is not object safe.

use futures::future::BoxFuture;
use serde_json::Value as JsonValue;

use super::control::CanUseTool;
use super::control::ToolPermissionDecision;

/// How long the host may take to answer a permission request.
///
/// A parent that is itself blocked waiting on this child cannot answer, so an
/// unbounded wait would deadlock the pair. On timeout the tool is denied without
/// an interrupt: the agent loses one command and keeps its turn.
pub(crate) const APPROVAL_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(300);

pub(crate) trait ClaudeHost: Send + Sync + std::fmt::Debug {
    /// Decides whether a tool call the CLI is about to make may proceed.
    fn approve_tool<'a>(&'a self, request: &'a CanUseTool)
    -> BoxFuture<'a, ToolPermissionDecision>;

    /// Runs one tool from the in-process MCP bridge and returns its result
    /// payload, or a message explaining why it could not run.
    fn call_bridge_tool<'a>(
        &'a self,
        name: &'a str,
        arguments: JsonValue,
    ) -> BoxFuture<'a, Result<JsonValue, String>>;

    /// The tools the bridge advertises, as MCP `tools/list` entries.
    fn bridge_tool_specs(&self) -> BoxFuture<'_, Vec<JsonValue>>;

    /// FORK: tells the user the turn is being retried after an Anthropic-side
    /// failure, so a two-minute pause does not read as a frozen agent.
    ///
    /// Defaulted to nothing: a host that has no UI (the adapter's tests, the
    /// bridge-only hosts) has nothing to say.
    fn notify_retry<'a>(&'a self, message: String, detail: String) -> BoxFuture<'a, ()> {
        let _ = (message, detail);
        Box::pin(async {})
    }
}
