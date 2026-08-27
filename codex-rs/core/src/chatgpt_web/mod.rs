//! FORK: the `chatgpt_web` provider — ChatGPT Pro web (chatgpt.com), driven
//! through the chrome-mcp daemon and a real Chrome tab, as a model backend for
//! Codex threads (`wire_api = "chatgpt_web"`).
//!
//! Layout (see `docs/plans/2026-08-26-chatgpt-web/PLANO.md`):
//! - `driver/`  — talks to the chrome-mcp daemon: tabs, page scripts, the
//!   chatgpt.com backend API, and the send/stop/upload operations.
//! - the rest of this module is the provider: history continuity, prompt
//!   rendering, poll → `ResponseEvent` translation and `stream()`.

pub(crate) mod driver;

use crate::client_common::Prompt;
use crate::client_common::ResponseStream;
use crate::config::ChatGptWebSettings;
use codex_protocol::error::CodexErr;
use codex_protocol::error::Result;
use codex_protocol::openai_models::ModelInfo;
use codex_protocol::openai_models::ReasoningEffort as ReasoningEffortConfig;
use codex_protocol::protocol::SandboxPolicy;
use std::path::PathBuf;
use std::sync::Arc;

/// File name under `CODEX_HOME` of the durable conversation record.
pub(crate) const SESSIONS_STATE_FILE_NAME: &str = "chatgpt_web_sessions.json";

/// FORK: the session-side half of the connector mode (`tools = "connector"`).
///
/// Filled in by M6: `begin_turn` / `prompt_contract` / `end_turn`. Until then
/// the trait only marks the object the turn loop will attach.
pub(crate) trait ConnectorBroker: Send + Sync + std::fmt::Debug {}

/// Where and under which rules a `chatgpt_web` turn runs.
///
/// Resolved per turn like `ClaudeCodeWorkspace`: roots and approval settings are
/// materialized by `Session::build_per_turn_config`, so a construction-time
/// config has none of them.
// The provider (`run_turn`, M4) is what reads these; until it lands the
// workspace is only built and carried.
#[allow(dead_code)]
#[derive(Debug, Clone)]
pub(crate) struct ChatGptWebWorkspace {
    /// Working directory of the Codex session, named in the prompt so ChatGPT
    /// knows what the transcript's paths are relative to.
    pub(crate) cwd: PathBuf,
    /// Every other root the Codex session can reach.
    pub(crate) extra_roots: Vec<PathBuf>,
    /// Roots the Codex session considers writable.
    pub(crate) writable_roots: Vec<PathBuf>,
    /// The Codex sandbox this turn runs under.
    pub(crate) sandbox: SandboxPolicy,
    /// The agent role's own instructions, delivered in the prompt header.
    pub(crate) developer_instructions: Option<String>,
    /// Resolved `[chatgpt_web]` settings.
    pub(crate) settings: ChatGptWebSettings,
    /// `CODEX_HOME`, for attachments, the daemon state and the sessions file.
    pub(crate) codex_home: PathBuf,
    /// Durable conversation record, so an evicted agent resumes its ChatGPT
    /// conversation instead of replaying its transcript.
    pub(crate) sessions_state_path: Option<PathBuf>,
    /// The connector broker, attached per sampling request when
    /// `tools = "connector"`; `None` in `tools = "none"`.
    pub(crate) connector: Option<Arc<dyn ConnectorBroker>>,
}

impl ChatGptWebWorkspace {
    /// Reads the workspace layout a turn is actually running under.
    pub(crate) fn from_config(config: &crate::config::Config) -> Self {
        let sandbox = config.legacy_sandbox_policy();
        Self {
            cwd: config.cwd.to_path_buf(),
            extra_roots: config
                .permissions
                .workspace_roots()
                .iter()
                .map(codex_utils_absolute_path::AbsolutePathBuf::to_path_buf)
                .collect(),
            writable_roots: writable_roots(&sandbox),
            sandbox,
            developer_instructions: config.developer_instructions.clone(),
            settings: config.chatgpt_web.clone(),
            codex_home: config.codex_home.to_path_buf(),
            sessions_state_path: Some(
                config
                    .codex_home
                    .to_path_buf()
                    .join(SESSIONS_STATE_FILE_NAME),
            ),
            connector: None,
        }
    }
}

/// Roots the sandbox lets the session write to.
fn writable_roots(sandbox: &SandboxPolicy) -> Vec<PathBuf> {
    match sandbox {
        SandboxPolicy::WorkspaceWrite { writable_roots, .. } => writable_roots
            .iter()
            .map(codex_utils_absolute_path::AbsolutePathBuf::to_path_buf)
            .collect(),
        SandboxPolicy::DangerFullAccess => Vec::new(),
        SandboxPolicy::ReadOnly { .. } => Vec::new(),
        SandboxPolicy::ExternalSandbox { .. } => Vec::new(),
    }
}

/// Cross-turn state of one Codex thread served by ChatGPT Web.
///
/// Lives in the client state (not the per-turn session) so consecutive turns
/// extend the same ChatGPT conversation. Filled in by M4 (continuity, live
/// connector turn).
#[derive(Debug, Default)]
pub(crate) struct ChatGptWebThreadState {}

/// Streams one Codex request through the ChatGPT web app.
pub(crate) async fn stream(
    _prompt: &Prompt,
    _model_info: &ModelInfo,
    _effort: Option<ReasoningEffortConfig>,
    _workspace: Option<&ChatGptWebWorkspace>,
    _state: Arc<ChatGptWebThreadState>,
    _thread_id: codex_protocol::ThreadId,
) -> Result<ResponseStream> {
    Err(CodexErr::UnsupportedOperation(
        "chatgpt_web provider is not implemented yet".to_string(),
    ))
}
