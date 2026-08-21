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

mod accounts;
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
    /// FORK: ordered Claude account config dirs; empty = inherit the ambient
    /// environment (the pre-fork behavior).
    pub(crate) account_dirs: Vec<PathBuf>,
    /// FORK: shared account-health state file under `CODEX_HOME`.
    pub(crate) accounts_state_path: Option<PathBuf>,
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
            account_dirs: config.claude_code_account_dirs.clone(),
            accounts_state_path: Some(
                config
                    .codex_home
                    .to_path_buf()
                    .join(accounts::ACCOUNTS_STATE_FILE_NAME),
            ),
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
        _ => "auto",
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

    fn record(
        &self,
        session_id: String,
        delivered_items: usize,
        delivered_fingerprint: u64,
        account_dir: Option<PathBuf>,
    ) {
        let mut continuity = self
            .continuity
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        continuity.session_id = Some(session_id);
        continuity.delivered_items = delivered_items;
        continuity.delivered_fingerprint = delivered_fingerprint;
        continuity.account_dir = account_dir;
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
            account_dirs: Vec::new(),
            accounts_state_path: None,
        },
    };

    let input = prompt.input.clone();
    let model_slug = model_info.slug.clone();
    let (tx_event, rx_event) = mpsc::channel(EVENT_CHANNEL_SIZE);
    let consumer_dropped = CancellationToken::new();
    let consumer_dropped_for_task = consumer_dropped.clone();

    tokio::spawn(async move {
        run_turn(
            input,
            model_slug,
            effort,
            workspace,
            state,
            tx_event,
            consumer_dropped_for_task,
        )
        .await;
    });

    Ok(ResponseStream {
        rx_event,
        consumer_dropped,
    })
}

/// How one attempt against one account ended.
enum AttemptOutcome {
    /// The turn completed and its `Completed` event was delivered.
    Completed,
    /// The consumer stopped listening; the turn is over regardless.
    ConsumerGone,
    /// The CLI failed.
    Failed {
        detail: String,
        /// Whether any user-visible event already reached the consumer.
        emitted_output: bool,
        /// True when the CLI itself reported the failure through an error
        /// `result` — a deliberate end of turn. The CLI streams limit/auth
        /// notices as assistant text first, so such attempts may fail over even
        /// though output was emitted; a mid-stream death may not, because a
        /// retry would duplicate a half-delivered answer.
        turn_reported: bool,
    },
}

/// Runs one Codex turn, failing over across configured accounts.
///
/// Each attempt replans the request: a Claude session id only resumes on the
/// account that produced it, so switching accounts replays the conversation
/// into a fresh session (`plan_request` with default continuity).
#[allow(clippy::too_many_arguments)]
async fn run_turn(
    input: Vec<ResponseItem>,
    model_slug: String,
    effort: Option<ReasoningEffortConfig>,
    workspace: ClaudeCodeWorkspace,
    state: Arc<ClaudeCodeThreadState>,
    tx_event: mpsc::Sender<Result<ResponseEvent>>,
    consumer_dropped: CancellationToken,
) {
    if tx_event.send(Ok(ResponseEvent::Created)).await.is_err() {
        return;
    }

    // FORK: decide the attempt order. The account already serving this thread
    // is sticky, so an ongoing conversation stays on its Claude session until
    // that account is spent (usage refresh may hit the network; it runs after
    // `Created` so stream startup never blocks on it).
    let sticky = state.snapshot().account_dir;
    let turn_accounts = accounts::TurnAccounts::resolve(
        &workspace.account_dirs,
        workspace.accounts_state_path.as_deref(),
        sticky.as_deref(),
    )
    .await;

    let candidates = turn_accounts.candidates.clone();
    let total = candidates.len();
    let mut failures: Vec<String> = Vec::new();

    for (index, account_dir) in candidates.into_iter().enumerate() {
        let continuity = state.snapshot();
        let continuity = if continuity_matches_account(&continuity, account_dir.as_deref()) {
            continuity
        } else {
            // The recorded Claude session lives in another account's history;
            // replay the conversation into a fresh session instead.
            ClaudeSessionContinuity::default()
        };
        let plan = history::plan_request(&input, &continuity);
        let resume_session_id = if plan.restart_session {
            None
        } else {
            continuity.session_id.clone()
        };

        let mut child = match spawn_claude(
            &model_slug,
            effort.as_ref(),
            resume_session_id.as_deref(),
            &workspace,
            account_dir.as_deref(),
        ) {
            Ok(child) => child,
            Err(err) => {
                // Startup failures (missing binary) are account-independent:
                // retrying elsewhere would fail identically.
                let _ = tx_event.send(Err(err)).await;
                return;
            }
        };

        let Some(mut stdin) = child.stdin.take() else {
            let _ = tx_event
                .send(Err(CodexErr::UnsupportedOperation(
                    "claude_code provider could not open the CLI stdin".to_string(),
                )))
                .await;
            return;
        };
        let turn_line = serde_json::json!({
            "type": "user",
            "message": { "role": "user", "content": plan.turn_text },
        })
        .to_string();
        // Written concurrently with reading stdout: a replayed transcript easily
        // exceeds the pipe buffer, and the CLI starts emitting events immediately,
        // so writing to completion first would deadlock both sides on a full pipe.
        // Closing stdin afterwards makes the CLI exit once it finishes this turn;
        // the session is resumed by id on the next request rather than held open.
        tokio::spawn(async move {
            if let Err(err) = stdin.write_all(format!("{turn_line}\n").as_bytes()).await {
                warn!("claude_code: failed to write the turn to the CLI: {err}");
            }
            let _ = stdin.shutdown().await;
        });

        let outcome = translate_stream(
            &mut child,
            &tx_event,
            &consumer_dropped,
            &plan,
            account_dir.clone(),
            &state,
        )
        .await;

        // The consumer stopped polling (an interrupt, or an error upstream):
        // the CLI would otherwise keep running its tool loop unattended.
        if consumer_dropped.is_cancelled() {
            let _ = child.start_kill();
        }
        let _ = child.wait().await;

        match outcome {
            AttemptOutcome::Completed => {
                turn_accounts.mark_success(account_dir.as_deref());
                return;
            }
            AttemptOutcome::ConsumerGone => return,
            AttemptOutcome::Failed {
                detail,
                emitted_output,
                turn_reported,
            } => {
                state.invalidate();
                let class = accounts::classify_failure(&detail);
                let label = accounts::account_label(account_dir.as_deref());
                if let Some(dir) = account_dir.as_deref() {
                    turn_accounts.record_failure(dir, &class, &detail);
                }
                let detail_line = detail.lines().next().unwrap_or(&detail).trim();
                failures.push(format!("{label}: {detail_line}"));

                let can_fail_over = class.is_account_level()
                    && (turn_reported || !emitted_output)
                    && account_dir.is_some()
                    && index + 1 < total;
                if can_fail_over {
                    warn!(
                        "claude_code: account {label} failed ({detail_line}); trying the next account"
                    );
                    continue;
                }

                let message = if failures.len() > 1 {
                    format!(
                        "claude_code turn failed on every configured account: {}",
                        failures.join("; ")
                    )
                } else {
                    format!("claude_code turn failed [{label}]: {detail}")
                };
                let _ = tx_event
                    .send(Err(CodexErr::UnsupportedOperation(message)))
                    .await;
                return;
            }
        }
    }

    // Every candidate failed with an account-level error.
    let _ = tx_event
        .send(Err(CodexErr::UnsupportedOperation(format!(
            "claude_code turn failed on every configured account: {}",
            failures.join("; ")
        ))))
        .await;
}

/// True when the recorded Claude session belongs to the account we are about to
/// spawn, so `--resume` will find it.
fn continuity_matches_account(
    continuity: &ClaudeSessionContinuity,
    account_dir: Option<&std::path::Path>,
) -> bool {
    if continuity.session_id.is_none() {
        return true;
    }
    match (continuity.account_dir.as_deref(), account_dir) {
        (None, None) => true,
        (Some(recorded), Some(selected)) => {
            accounts::dir_key(recorded) == accounts::dir_key(selected)
        }
        _ => false,
    }
}

fn spawn_claude(
    model_slug: &str,
    effort: Option<&ReasoningEffortConfig>,
    resume_session_id: Option<&str>,
    workspace: &ClaudeCodeWorkspace,
    config_dir: Option<&std::path::Path>,
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

    // FORK: pin the CLI to one account instead of inheriting the environment.
    if let Some(config_dir) = config_dir {
        command.env("CLAUDE_CONFIG_DIR", config_dir);
    }

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
///
/// `Created` is emitted by the caller (once per turn, not per attempt); this
/// reports how the attempt ended so the caller can decide whether another
/// account may retry it.
async fn translate_stream(
    child: &mut Child,
    tx_event: &mpsc::Sender<Result<ResponseEvent>>,
    consumer_dropped: &CancellationToken,
    plan: &history::RequestPlan,
    account_dir: Option<PathBuf>,
    state: &ClaudeCodeThreadState,
) -> AttemptOutcome {
    let mut emitted_output = false;
    let Some(stdout) = child.stdout.take() else {
        return AttemptOutcome::Failed {
            detail: "claude_code provider could not open the CLI stdout".to_string(),
            emitted_output,
            turn_reported: false,
        };
    };
    let stderr = child.stderr.take();
    let mut lines = BufReader::new(stdout).lines();

    let mut session_id: Option<String> = None;
    let mut assembler = StreamAssembler::new(tx_event);

    loop {
        let line = tokio::select! {
            _ = consumer_dropped.cancelled() => return AttemptOutcome::ConsumerGone,
            line = lines.next_line() => line,
        };
        let line = match line {
            Ok(Some(line)) => line,
            Ok(None) => break,
            Err(err) => {
                return AttemptOutcome::Failed {
                    detail: format!("claude_code provider read failed: {err}"),
                    emitted_output,
                    turn_reported: false,
                };
            }
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
                    let (consumer_alive, pushed) =
                        match block.get("type").and_then(JsonValue::as_str) {
                            Some("text") => {
                                let text = block
                                    .get("text")
                                    .and_then(JsonValue::as_str)
                                    .unwrap_or_default();
                                if text.is_empty() {
                                    continue;
                                }
                                (assembler.push_text(text).await, true)
                            }
                            Some("thinking") => {
                                let text = block
                                    .get("thinking")
                                    .and_then(JsonValue::as_str)
                                    .unwrap_or_default();
                                if text.is_empty() {
                                    continue;
                                }
                                (assembler.push_reasoning(text).await, true)
                            }
                            Some("tool_use") => {
                                // Claude executes its own tools; surface the
                                // activity as reasoning so the turn is not a
                                // silent black box.
                                (
                                    assembler.push_reasoning(&describe_tool_use(&block)).await,
                                    true,
                                )
                            }
                            _ => (true, false),
                        };
                    if pushed {
                        emitted_output = true;
                    }
                    if !consumer_alive {
                        return AttemptOutcome::ConsumerGone;
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
                    // Close whatever item the error prelude opened (the CLI
                    // streams "hit your limit" style notices as assistant text)
                    // so a failover attempt starts from a clean stream.
                    if !assembler.close(MessagePhase::Commentary).await {
                        return AttemptOutcome::ConsumerGone;
                    }
                    let detail = if result_text.is_empty() {
                        event
                            .get("subtype")
                            .and_then(JsonValue::as_str)
                            .unwrap_or("unknown error")
                            .to_string()
                    } else {
                        result_text.to_string()
                    };
                    return AttemptOutcome::Failed {
                        detail,
                        emitted_output,
                        turn_reported: true,
                    };
                }

                // Close whatever block run was in flight; a trailing answer is the
                // turn's final answer.
                if !assembler.close(MessagePhase::FinalAnswer).await {
                    return AttemptOutcome::ConsumerGone;
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
                    return AttemptOutcome::ConsumerGone;
                }

                if let Some(session_id) = session_id.clone() {
                    state.record(
                        session_id,
                        plan.delivered_items,
                        plan.delivered_fingerprint,
                        account_dir,
                    );
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
                    return AttemptOutcome::ConsumerGone;
                }
                return AttemptOutcome::Completed;
            }
            _ => {}
        }
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
    AttemptOutcome::Failed {
        detail: if detail.is_empty() {
            "claude_code turn ended without a result".to_string()
        } else {
            format!("claude_code turn ended without a result: {detail}")
        },
        emitted_output,
        turn_reported: false,
    }
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
                .send(ResponseEvent::OutputItemAdded(
                    reasoning_item(String::new()),
                ))
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn headless_claude_uses_auto_for_interactive_codex_policies() {
        assert_eq!(permission_mode_for(AskForApproval::OnRequest), "auto");
        assert_eq!(permission_mode_for(AskForApproval::UnlessTrusted), "auto");
        assert_eq!(
            permission_mode_for(AskForApproval::Never),
            "bypassPermissions"
        );
    }
}
