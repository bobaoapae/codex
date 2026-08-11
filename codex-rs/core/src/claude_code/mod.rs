//! Claude Code as a Codex model backend.
//!
//! A provider whose `wire_api` is [`WireApi::ClaudeCode`] does not talk to an
//! HTTP endpoint. Instead every request spawns the locally installed `claude`
//! binary in headless stream-json mode, hands it the part of the Codex
//! conversation it has not seen yet, and translates its event stream back into
//! [`ResponseEvent`]s.
//!
//! The consequence worth understanding: Claude Code is an *agent*, not a
//! completion endpoint. It runs its own tool loop against the real filesystem,
//! so one Codex request maps to one complete Claude run, and the tools Codex
//! advertises in the prompt are ignored — Claude uses its own. What Codex keeps
//! is everything around that: history ownership, forking, agent lifecycle,
//! transcripts, and the multi-agent tools.
//!
//! Authentication is whatever the `claude` binary is logged in as, so this path
//! spends the user's Claude Code subscription rather than an API key.

mod history;

use crate::client_common::Prompt;
use crate::client_common::ResponseEvent;
use crate::client_common::ResponseStream;
use codex_protocol::error::CodexErr;
use codex_protocol::error::Result;
use codex_protocol::models::ContentItem;
use codex_protocol::models::MessagePhase;
use codex_protocol::models::ReasoningItemReasoningSummary;
use codex_protocol::models::ResponseItem;
use codex_protocol::openai_models::ModelInfo;
use codex_protocol::openai_models::ReasoningEffort as ReasoningEffortConfig;
use codex_protocol::protocol::AskForApproval;
use codex_protocol::protocol::TokenUsage;
use serde_json::Value as JsonValue;
use std::path::PathBuf;

use std::process::Stdio;
use std::sync::Arc;
use std::sync::Mutex as StdMutex;
use tokio::io::AsyncBufReadExt;
use tokio::io::AsyncWriteExt;
use tokio::io::BufReader;
use tokio::process::Child;
use tokio::process::Command;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use tracing::debug;
use tracing::warn;

pub(crate) use history::ClaudeSessionContinuity;

/// Environment override for the CLI location, mirroring how the OSS providers
/// let the endpoint be pointed elsewhere.
const CLAUDE_BIN_ENV: &str = "CODEX_CLAUDE_CODE_BIN";
const DEFAULT_CLAUDE_BIN: &str = "claude";

/// Buffer for translated events. Claude emits one event per content block and
/// per tool call, so a turn produces tens of events, not thousands.
const EVENT_CHANNEL_SIZE: usize = 256;

/// Everything the CLI needs to know about the Codex session hosting it.
///
/// Resolved once when the client is built, because a turn-scoped session cannot
/// see the thread's workspace layout or approval settings.
#[derive(Debug, Clone)]
pub(crate) struct ClaudeCodeWorkspace {
    /// Directory the CLI runs in.
    pub(crate) cwd: PathBuf,
    /// Every other root the Codex session can reach. Without these the agent is
    /// confined to `cwd` and cannot open sibling repositories the task depends
    /// on.
    pub(crate) extra_roots: Vec<PathBuf>,
    /// Permission mode passed to the CLI, derived from the Codex approval policy.
    pub(crate) permission_mode: &'static str,
}

impl ClaudeCodeWorkspace {
    /// Reads the workspace layout a turn is actually running under.
    ///
    /// Roots and approval policy are materialized per turn
    /// (`Session::build_per_turn_config`), so reading them from the session's
    /// construction-time config yields an empty root list.
    pub(crate) fn from_config(config: &crate::config::Config) -> Self {
        Self {
            cwd: config.cwd.to_path_buf(),
            extra_roots: config
                .permissions
                .workspace_roots()
                .iter()
                .map(codex_utils_absolute_path::AbsolutePathBuf::to_path_buf)
                .collect(),
            permission_mode: permission_mode_for(config.permissions.approval_policy.value()),
        }
    }
}

/// Maps the Codex approval policy onto a Claude Code permission mode.
///
/// `acceptEdits` only auto-approves file edits; in headless mode every other
/// permission request is refused outright, which silently blocks builds and
/// tests. When Codex itself stopped asking, the child must not ask either.
pub(crate) fn permission_mode_for(approval_policy: AskForApproval) -> &'static str {
    match approval_policy {
        AskForApproval::Never => "bypassPermissions",
        _ => "acceptEdits",
    }
}

/// Cross-turn state for the Claude session backing one Codex thread.
///
/// Lives in the client state (not the per-turn session) so consecutive turns can
/// `--resume` the same Claude session and reuse its prompt cache.
#[derive(Debug, Default)]
pub(crate) struct ClaudeCodeThreadState {
    continuity: StdMutex<ClaudeSessionContinuity>,
}

impl ClaudeCodeThreadState {
    fn snapshot(&self) -> ClaudeSessionContinuity {
        self.continuity
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    fn record(&self, session_id: String, delivered_items: usize, delivered_fingerprint: u64) {
        let mut continuity = self
            .continuity
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        continuity.session_id = Some(session_id);
        continuity.delivered_items = delivered_items;
        continuity.delivered_fingerprint = delivered_fingerprint;
    }

    /// Forgets the resume point after a failed turn, so the next request replays
    /// the conversation instead of extending a session in an unknown state.
    fn invalidate(&self) {
        let mut continuity = self
            .continuity
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        *continuity = ClaudeSessionContinuity::default();
    }
}

/// Streams one Codex request through the Claude Code CLI.
pub(crate) async fn stream(
    prompt: &Prompt,
    model_info: &ModelInfo,
    effort: Option<ReasoningEffortConfig>,
    workspace: Option<&ClaudeCodeWorkspace>,
    state: Arc<ClaudeCodeThreadState>,
) -> Result<ResponseStream> {
    let continuity = state.snapshot();
    let plan = history::plan_request(&prompt.input, &continuity);
    let resume_session_id = if plan.restart_session {
        None
    } else {
        continuity.session_id.clone()
    };

    let workspace = match workspace {
        Some(workspace) => workspace.clone(),
        None => ClaudeCodeWorkspace {
            cwd: std::env::current_dir().map_err(|err| {
                CodexErr::UnsupportedOperation(format!(
                    "claude_code provider could not resolve a workspace: {err}"
                ))
            })?,
            extra_roots: Vec::new(),
            permission_mode: permission_mode_for(AskForApproval::OnRequest),
        },
    };

    let mut child = spawn_claude(
        &model_info.slug,
        effort.as_ref(),
        resume_session_id.as_deref(),
        &workspace,
    )?;

    let mut stdin = child.stdin.take().ok_or_else(|| {
        CodexErr::UnsupportedOperation(
            "claude_code provider could not open the CLI stdin".to_string(),
        )
    })?;
    let turn_line = serde_json::json!({
        "type": "user",
        "message": { "role": "user", "content": plan.turn_text },
    })
    .to_string();
    // Written concurrently with reading stdout: a replayed transcript easily
    // exceeds the pipe buffer, and the CLI starts emitting events immediately, so
    // writing to completion first would deadlock both sides on a full pipe.
    // Closing stdin afterwards makes the CLI exit once it finishes this turn; the
    // session is resumed by id on the next request rather than held open.
    tokio::spawn(async move {
        if let Err(err) = stdin.write_all(format!("{turn_line}\n").as_bytes()).await {
            warn!("claude_code: failed to write the turn to the CLI: {err}");
        }
        let _ = stdin.shutdown().await;
    });

    let (tx_event, rx_event) = mpsc::channel(EVENT_CHANNEL_SIZE);
    let consumer_dropped = CancellationToken::new();
    let consumer_dropped_for_task = consumer_dropped.clone();

    tokio::spawn(async move {
        let outcome = translate_stream(
            &mut child,
            &tx_event,
            &consumer_dropped_for_task,
            plan.delivered_items,
            plan.delivered_fingerprint,
            &state,
        )
        .await;

        if let Err(err) = outcome {
            state.invalidate();
            let _ = tx_event.send(Err(err)).await;
        }

        // The consumer stopped polling (an interrupt, or an error upstream):
        // the CLI would otherwise keep running its tool loop unattended.
        if consumer_dropped_for_task.is_cancelled() {
            let _ = child.start_kill();
        }
        let _ = child.wait().await;
    });

    Ok(ResponseStream {
        rx_event,
        consumer_dropped,
    })
}

fn spawn_claude(
    model_slug: &str,
    effort: Option<&ReasoningEffortConfig>,
    resume_session_id: Option<&str>,
    workspace: &ClaudeCodeWorkspace,
) -> Result<Child> {
    let bin = std::env::var(CLAUDE_BIN_ENV)
        .ok()
        .filter(|value| !value.trim().is_empty())
        .unwrap_or_else(|| DEFAULT_CLAUDE_BIN.to_string());

    let mut command = Command::new(&bin);
    command
        .arg("--print")
        // stream-json output is rejected without --verbose under --print.
        .arg("--verbose")
        .args(["--input-format", "stream-json"])
        .args(["--output-format", "stream-json"])
        .args(["--model", model_slug])
        .args(["--permission-mode", workspace.permission_mode])
        // The agent's MCP surface is Codex's business, not the CLI's user config.
        .arg("--strict-mcp-config");

    // Every root the Codex session can reach, so a task spanning sibling
    // repositories is not confined to the thread's cwd.
    command.arg("--add-dir").arg(&workspace.cwd);
    for root in &workspace.extra_roots {
        if root != &workspace.cwd {
            command.arg("--add-dir").arg(root);
        }
    }

    if let Some(effort) = effort {
        command.args(["--effort", &effort.to_string()]);
    }
    match resume_session_id {
        Some(session_id) => {
            command.args(["--resume", session_id]);
        }
        None => {
            command.args(["--session-id", &uuid::Uuid::new_v4().to_string()]);
        }
    }

    command
        .current_dir(&workspace.cwd)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);

    command.spawn().map_err(|err| {
        CodexErr::UnsupportedOperation(format!(
            "claude_code provider could not start `{bin}`: {err}. \
Install Claude Code and log in, or set {CLAUDE_BIN_ENV} to its path."
        ))
    })
}

/// Reads the CLI's stream-json output and republishes it as Codex events.
async fn translate_stream(
    child: &mut Child,
    tx_event: &mpsc::Sender<Result<ResponseEvent>>,
    consumer_dropped: &CancellationToken,
    delivered_items: usize,
    delivered_fingerprint: u64,
    state: &ClaudeCodeThreadState,
) -> Result<()> {
    let stdout = child.stdout.take().ok_or_else(|| {
        CodexErr::UnsupportedOperation(
            "claude_code provider could not open the CLI stdout".to_string(),
        )
    })?;
    let stderr = child.stderr.take();
    let mut lines = BufReader::new(stdout).lines();

    if tx_event.send(Ok(ResponseEvent::Created)).await.is_err() {
        return Ok(());
    }

    let mut session_id: Option<String> = None;
    let mut assembler = StreamAssembler::new(tx_event);
    let mut completed = false;

    loop {
        let line = tokio::select! {
            _ = consumer_dropped.cancelled() => return Ok(()),
            line = lines.next_line() => line,
        };
        let Some(line) = line.map_err(|err| {
            CodexErr::UnsupportedOperation(format!("claude_code provider read failed: {err}"))
        })?
        else {
            break;
        };
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let Ok(event) = serde_json::from_str::<JsonValue>(line) else {
            debug!("claude_code: skipping non-JSON output line");
            continue;
        };

        match event.get("type").and_then(JsonValue::as_str) {
            Some("system") => {
                if let Some(id) = event.get("session_id").and_then(JsonValue::as_str) {
                    session_id = Some(id.to_string());
                }
            }
            Some("assistant") => {
                let blocks = event
                    .get("message")
                    .and_then(|message| message.get("content"))
                    .and_then(JsonValue::as_array)
                    .cloned()
                    .unwrap_or_default();
                for block in blocks {
                    let delivered = match block.get("type").and_then(JsonValue::as_str) {
                        Some("text") => {
                            let text = block
                                .get("text")
                                .and_then(JsonValue::as_str)
                                .unwrap_or_default();
                            if text.is_empty() {
                                continue;
                            }
                            assembler.push_text(text).await
                        }
                        Some("thinking") => {
                            let text = block
                                .get("thinking")
                                .and_then(JsonValue::as_str)
                                .unwrap_or_default();
                            if text.is_empty() {
                                continue;
                            }
                            assembler.push_reasoning(text).await
                        }
                        Some("tool_use") => {
                            // Claude executes its own tools; surface the activity
                            // as reasoning so the turn is not a silent black box.
                            assembler.push_reasoning(&describe_tool_use(&block)).await
                        }
                        _ => true,
                    };
                    if !delivered {
                        return Ok(());
                    }
                }
            }
            Some("result") => {
                if let Some(id) = event.get("session_id").and_then(JsonValue::as_str) {
                    session_id = Some(id.to_string());
                }
                let is_error = event
                    .get("is_error")
                    .and_then(JsonValue::as_bool)
                    .unwrap_or(false);
                let result_text = event
                    .get("result")
                    .and_then(JsonValue::as_str)
                    .unwrap_or_default();
                if is_error {
                    return Err(CodexErr::UnsupportedOperation(format!(
                        "claude_code turn failed: {}",
                        if result_text.is_empty() {
                            event
                                .get("subtype")
                                .and_then(JsonValue::as_str)
                                .unwrap_or("unknown error")
                        } else {
                            result_text
                        }
                    )));
                }

                // Close whatever block run was in flight; a trailing answer is the
                // turn's final answer.
                if !assembler.close(MessagePhase::FinalAnswer).await {
                    return Ok(());
                }
                // The CLI reports the answer once more on `result`. If nothing was
                // streamed (a turn that only ran tools, or output we could not
                // parse), that report is the only assistant text we have.
                if !assembler.streamed_any_text()
                    && !result_text.trim().is_empty()
                    && !assembler
                        .emit_message(result_text.to_string(), MessagePhase::FinalAnswer)
                        .await
                {
                    return Ok(());
                }

                if let Some(session_id) = session_id.clone() {
                    state.record(session_id, delivered_items, delivered_fingerprint);
                } else {
                    state.invalidate();
                }

                let response_id = session_id.clone().unwrap_or_default();
                let token_usage = parse_token_usage(event.get("usage"));
                if tx_event
                    .send(Ok(ResponseEvent::Completed {
                        response_id,
                        token_usage,
                        end_turn: Some(true),
                    }))
                    .await
                    .is_err()
                {
                    return Ok(());
                }
                completed = true;
                break;
            }
            _ => {}
        }
    }

    if completed {
        return Ok(());
    }

    // The CLI exited without a terminal `result`: surface whatever it said on
    // stderr, which is where startup and auth failures land.
    let detail = match stderr {
        Some(stderr) => {
            let mut buffer = String::new();
            let mut reader = BufReader::new(stderr).lines();
            while let Ok(Some(line)) = reader.next_line().await {
                if buffer.len() > 2_000 {
                    break;
                }
                buffer.push_str(line.trim());
                buffer.push('\n');
            }
            buffer.trim().to_string()
        }
        None => String::new(),
    };
    warn!("claude_code: CLI ended without a result event");
    Err(CodexErr::UnsupportedOperation(format!(
        "claude_code turn ended without a result{}",
        if detail.is_empty() {
            String::new()
        } else {
            format!(": {detail}")
        }
    )))
}

/// Turns Claude's block stream into Codex items.
///
/// Codex's turn loop refuses a delta with no item open (`error_or_panic`) and
/// closes the open item on `OutputItemDone`. Claude interleaves thinking, tool
/// calls and answer text freely, so each run of same-kind blocks becomes one
/// Codex item: open on the first block of a run, close when the kind changes.
struct StreamAssembler<'a> {
    tx: &'a mpsc::Sender<Result<ResponseEvent>>,
    active: Option<ActiveItem>,
    streamed_any_text: bool,
}

enum ActiveItem {
    Reasoning(String),
    Message(String),
}

impl<'a> StreamAssembler<'a> {
    fn new(tx: &'a mpsc::Sender<Result<ResponseEvent>>) -> Self {
        Self {
            tx,
            active: None,
            streamed_any_text: false,
        }
    }

    fn streamed_any_text(&self) -> bool {
        self.streamed_any_text
    }

    /// Sends one event; `false` means the consumer is gone and we should stop.
    async fn send(&self, event: ResponseEvent) -> bool {
        self.tx.send(Ok(event)).await.is_ok()
    }

    async fn push_text(&mut self, text: &str) -> bool {
        if !matches!(self.active, Some(ActiveItem::Message(_))) {
            if !self.close(MessagePhase::Commentary).await {
                return false;
            }
            if !self
                .send(ResponseEvent::OutputItemAdded(message_item(
                    String::new(),
                    &MessagePhase::Commentary,
                )))
                .await
            {
                return false;
            }
            self.active = Some(ActiveItem::Message(String::new()));
        }
        if let Some(ActiveItem::Message(buffer)) = self.active.as_mut() {
            buffer.push_str(text);
        }
        self.streamed_any_text = true;
        self.send(ResponseEvent::OutputTextDelta(text.to_string()))
            .await
    }

    async fn push_reasoning(&mut self, text: &str) -> bool {
        if !matches!(self.active, Some(ActiveItem::Reasoning(_))) {
            if !self.close(MessagePhase::Commentary).await {
                return false;
            }
            if !self
                .send(ResponseEvent::OutputItemAdded(reasoning_item(String::new())))
                .await
            {
                return false;
            }
            self.active = Some(ActiveItem::Reasoning(String::new()));
        }
        if let Some(ActiveItem::Reasoning(buffer)) = self.active.as_mut() {
            buffer.push_str(text);
        }
        self.send(ResponseEvent::ReasoningSummaryDelta {
            delta: text.to_string(),
            // One summary part per item: Claude's blocks are a single narrative,
            // not indexed summary sections.
            summary_index: 0,
        })
        .await
    }

    /// Closes the open item, if any. `phase` applies only to assistant text.
    async fn close(&mut self, phase: MessagePhase) -> bool {
        match self.active.take() {
            None => true,
            Some(ActiveItem::Reasoning(text)) => {
                self.send(ResponseEvent::OutputItemDone(reasoning_item(text)))
                    .await
            }
            Some(ActiveItem::Message(text)) => {
                self.send(ResponseEvent::OutputItemDone(message_item(text, &phase)))
                    .await
            }
        }
    }

    /// Emits a complete assistant message that was never streamed.
    async fn emit_message(&mut self, text: String, phase: MessagePhase) -> bool {
        if !self
            .send(ResponseEvent::OutputItemAdded(message_item(
                text.clone(),
                &phase,
            )))
            .await
        {
            return false;
        }
        self.streamed_any_text = true;
        self.send(ResponseEvent::OutputItemDone(message_item(text, &phase)))
            .await
    }
}

fn message_item(text: String, phase: &MessagePhase) -> ResponseItem {
    ResponseItem::Message {
        id: None,
        role: "assistant".to_string(),
        content: vec![ContentItem::OutputText { text }],
        phase: Some(phase.clone()),
        internal_chat_message_metadata_passthrough: None,
    }
}

fn reasoning_item(text: String) -> ResponseItem {
    ResponseItem::Reasoning {
        id: None,
        summary: vec![ReasoningItemReasoningSummary::SummaryText { text }],
        content: None,
        encrypted_content: None,
        internal_chat_message_metadata_passthrough: None,
    }
}

fn describe_tool_use(block: &JsonValue) -> String {
    let name = block
        .get("name")
        .and_then(JsonValue::as_str)
        .unwrap_or("tool");
    let input = block.get("input");
    let detail = match name {
        "Bash" => input
            .and_then(|input| input.get("command"))
            .and_then(JsonValue::as_str)
            .unwrap_or_default()
            .to_string(),
        "Read" | "Write" | "Edit" | "NotebookEdit" => input
            .and_then(|input| input.get("file_path"))
            .and_then(JsonValue::as_str)
            .unwrap_or_default()
            .to_string(),
        "Grep" | "Glob" => input
            .and_then(|input| input.get("pattern"))
            .and_then(JsonValue::as_str)
            .unwrap_or_default()
            .to_string(),
        _ => String::new(),
    };
    let detail: String = detail.chars().take(200).collect();
    if detail.is_empty() {
        format!("[{name}]\n")
    } else {
        format!("[{name}] {detail}\n")
    }
}

fn parse_token_usage(usage: Option<&JsonValue>) -> Option<TokenUsage> {
    let usage = usage?;
    let field = |key: &str| usage.get(key).and_then(JsonValue::as_i64).unwrap_or(0);
    let input_tokens = field("input_tokens");
    let cached_input_tokens = field("cache_read_input_tokens");
    let cache_write_input_tokens = field("cache_creation_input_tokens");
    let output_tokens = field("output_tokens");
    Some(TokenUsage {
        input_tokens: input_tokens + cached_input_tokens + cache_write_input_tokens,
        cached_input_tokens,
        cache_write_input_tokens,
        output_tokens,
        reasoning_output_tokens: 0,
        total_tokens: input_tokens + cached_input_tokens + cache_write_input_tokens + output_tokens,
        codex_rollout_budget_units: None,
    })
}
