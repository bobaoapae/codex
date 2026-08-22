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

pub(crate) mod accounts;
mod history;
mod sessions;

use crate::client_common::Prompt;
use crate::client_common::ResponseEvent;
use crate::client_common::ResponseStream;
use codex_config::config_toml::ClaudeCodeAccountSelection;
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
use std::path::Path;
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

/// How much of a failed child's stderr is worth reporting back.
const MAX_STDERR_CHARS: usize = 2_000;

/// Pause before the single retry of a failed process spawn.
const SPAWN_RETRY_DELAY: std::time::Duration = std::time::Duration::from_millis(250);

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
    /// FORK: durable Claude-session record under `CODEX_HOME`, so an evicted
    /// agent can resume its session instead of replaying its transcript.
    pub(crate) sessions_state_path: Option<PathBuf>,
    /// FORK: how to order the accounts for this turn.
    pub(crate) selection: ClaudeCodeAccountSelection,
    /// FORK: headroom the thread's current account must keep to stay sticky.
    pub(crate) sticky_min_headroom_pct: f64,
    /// FORK: account this agent was pinned to when it was spawned. It is tried
    /// first and still fails over, so a spent pin cannot strand the agent.
    pub(crate) pinned_account: Option<PathBuf>,
    /// FORK: how long the CLI may produce nothing before the turn is abandoned.
    /// `None` disables the watchdog.
    pub(crate) idle_timeout: Option<std::time::Duration>,
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
            sessions_state_path: Some(
                config
                    .codex_home
                    .to_path_buf()
                    .join(sessions::SESSIONS_STATE_FILE_NAME),
            ),
            selection: config.claude_code_selection,
            sticky_min_headroom_pct: config.claude_code_sticky_min_headroom_pct,
            pinned_account: config.claude_code_account_override.clone(),
            idle_timeout: config
                .claude_code_idle_timeout_ms
                .map(std::time::Duration::from_millis),
        }
    }
}

/// FORK: how a caller named a Claude account.
pub(crate) enum AccountAlias {
    /// Let the selection policy decide, as if no account had been named.
    Auto,
    /// A specific configured account directory.
    Dir(PathBuf),
}

/// FORK: one line per configured account, for tool descriptions and errors.
///
/// The index is what a caller is most likely to use, so it leads.
pub(crate) fn account_options(account_dirs: &[PathBuf]) -> Vec<String> {
    account_dirs
        .iter()
        .enumerate()
        .map(|(index, dir)| format!("{}: {}", index + 1, accounts::account_label(Some(dir))))
        .collect()
}

/// FORK: resolves a `spawn_agent(account = …)` value against the configured
/// accounts, accepting an index, a path, or part of the account's email.
pub(crate) fn resolve_account_alias(
    account_dirs: &[PathBuf],
    alias: &str,
) -> std::result::Result<AccountAlias, String> {
    let alias = alias.trim();
    if alias.is_empty() || alias.eq_ignore_ascii_case("auto") {
        return Ok(AccountAlias::Auto);
    }
    if account_dirs.is_empty() {
        return Err("no Claude accounts are configured; omit `account`".to_string());
    }

    if let Ok(index) = alias.parse::<usize>()
        && index >= 1
        && let Some(dir) = account_dirs.get(index - 1)
    {
        return Ok(AccountAlias::Dir(dir.clone()));
    }

    let alias_key = accounts::dir_key(Path::new(alias));
    if let Some(dir) = account_dirs
        .iter()
        .find(|dir| accounts::dir_key(dir) == alias_key)
    {
        return Ok(AccountAlias::Dir(dir.clone()));
    }

    let needle = alias.to_lowercase();
    let mut matches = account_dirs.iter().filter(|dir| {
        accounts::account_label(Some(dir))
            .to_lowercase()
            .contains(&needle)
    });
    match (matches.next(), matches.next()) {
        (Some(dir), None) => Ok(AccountAlias::Dir(dir.clone())),
        (Some(_), Some(_)) => Err(format!(
            "`{alias}` matches more than one Claude account. Configured accounts: {}",
            account_options(account_dirs).join("; ")
        )),
        _ => Err(format!(
            "unknown Claude account `{alias}`. Configured accounts: {}",
            account_options(account_dirs).join("; ")
        )),
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
    /// FORK: where this thread's continuity is persisted, once a turn has told
    /// us which thread we are. `None` until then.
    store: StdMutex<Option<SessionStore>>,
}

/// FORK: identity of a thread's on-disk continuity record.
#[derive(Debug, Clone)]
struct SessionStore {
    path: PathBuf,
    thread_key: String,
}

impl ClaudeCodeThreadState {
    fn snapshot(&self) -> ClaudeSessionContinuity {
        self.continuity
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    /// FORK: binds this state to its durable record, loading it the first time.
    ///
    /// Returns the account the agent was pinned to at spawn, which the rebuilt
    /// config no longer carries. Later calls only re-bind: in-memory continuity
    /// is always fresher than the file.
    fn hydrate(&self, path: &Path, thread_key: String) -> Option<PathBuf> {
        let mut store = self
            .store
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if store.is_some() {
            return None;
        }
        *store = Some(SessionStore {
            path: path.to_path_buf(),
            thread_key: thread_key.clone(),
        });
        drop(store);

        let (recorded, pinned) = sessions::load(path, &thread_key)?;
        let mut continuity = self
            .continuity
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if continuity.session_id.is_none() {
            *continuity = recorded;
        }
        pinned
    }

    fn persist(&self, continuity: &ClaudeSessionContinuity, pinned_account: Option<&Path>) {
        let store = self
            .store
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        if let Some(store) = store {
            sessions::store(&store.path, &store.thread_key, continuity, pinned_account);
        }
    }

    fn record(
        &self,
        session_id: String,
        delivered_items: usize,
        delivered_fingerprint: u64,
        account_dir: Option<PathBuf>,
        echoed: Vec<u64>,
        pinned_account: Option<&Path>,
    ) {
        let snapshot = {
            let mut continuity = self
                .continuity
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            continuity.session_id = Some(session_id);
            continuity.delivered_items = delivered_items;
            continuity.delivered_fingerprint = delivered_fingerprint;
            continuity.account_dir = account_dir;
            continuity.echoed = echoed;
            continuity.clone()
        };
        self.persist(&snapshot, pinned_account);
    }

    /// Forgets the resume point, so the next request replays the conversation
    /// instead of extending a session in an unknown state.
    fn invalidate(&self) {
        {
            let mut continuity = self
                .continuity
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            *continuity = ClaudeSessionContinuity::default();
        }
        self.persist(&ClaudeSessionContinuity::default(), None);
    }
}

/// Streams one Codex request through the Claude Code CLI.
pub(crate) async fn stream(
    prompt: &Prompt,
    model_info: &ModelInfo,
    effort: Option<ReasoningEffortConfig>,
    workspace: Option<&ClaudeCodeWorkspace>,
    state: Arc<ClaudeCodeThreadState>,
    thread_id: codex_protocol::ThreadId,
) -> Result<ResponseStream> {
    let mut workspace = match workspace {
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
            sessions_state_path: None,
            selection: ClaudeCodeAccountSelection::default(),
            sticky_min_headroom_pct: 0.0,
            pinned_account: None,
            idle_timeout: None,
        },
    };

    // FORK: recover the Claude session this thread was using, in case the agent
    // was evicted and rebuilt since its last turn.
    if let Some(path) = workspace.sessions_state_path.clone()
        && let Some(pinned) = state.hydrate(&path, thread_id.to_string())
    {
        workspace.pinned_account.get_or_insert(pinned);
    }

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
        accounts::AccountPolicy {
            selection: workspace.selection,
            sticky_min_headroom_pct: workspace.sticky_min_headroom_pct,
            sticky: sticky.as_deref(),
            pinned: workspace.pinned_account.as_deref(),
        },
    )
    .await;

    let candidates = turn_accounts.candidates.clone();
    let total = candidates.len();
    let mut failures: Vec<String> = Vec::new();

    for (index, account_dir) in candidates.into_iter().enumerate() {
        // Held for the whole attempt so a concurrent spawn can see this account
        // is already busy and pick a quieter one.
        let _in_flight = accounts::InFlightGuard::acquire(account_dir.as_deref());
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

        let spawned = match spawn_claude(
            &model_slug,
            effort.as_ref(),
            resume_session_id.as_deref(),
            &workspace,
            account_dir.as_deref(),
        ) {
            Ok(child) => Ok(child),
            // Nothing has run yet, so one retry is free of side effects. It
            // covers the transient cases — a locked binary right after an
            // upgrade, a momentarily exhausted handle table — without papering
            // over a missing install, which fails again immediately.
            Err(_) => {
                tokio::time::sleep(SPAWN_RETRY_DELAY).await;
                spawn_claude(
                    &model_slug,
                    effort.as_ref(),
                    resume_session_id.as_deref(),
                    &workspace,
                    account_dir.as_deref(),
                )
            }
        };
        let mut child = match spawned {
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
            AttemptContext {
                plan: &plan,
                account_dir: account_dir.clone(),
                state: &state,
                pinned_account: workspace.pinned_account.as_deref(),
                idle_timeout: workspace.idle_timeout,
            },
        )
        .await;

        // The consumer stopped polling (an interrupt, or an error upstream):
        // the CLI would otherwise keep running its tool loop unattended.
        if consumer_dropped.is_cancelled() {
            kill_process_tree(&mut child);
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
                // Only a session that cannot be resumed is worth forgetting.
                // `delivered_items` advances on success alone, so keeping the
                // resume point after a transient failure just re-sends the same
                // tail — while dropping it replays the entire transcript and
                // throws away the prompt cache with it.
                if session_lost(&detail) {
                    state.invalidate();
                }
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

/// FORK: kills the CLI *and* everything it started.
///
/// `Child::start_kill` only signals the direct child, so Claude's shells, build
/// and test processes keep running against the workspace. On Unix the child is
/// its own process group; on Windows `taskkill /T` walks the tree.
fn kill_process_tree(child: &mut Child) {
    let pid = child.id();
    let _ = child.start_kill();
    let Some(pid) = pid else {
        return;
    };
    #[cfg(unix)]
    {
        // Negative pid = the whole process group created with `process_group(0)`.
        unsafe { libc::kill(-(pid as i32), libc::SIGKILL) };
    }
    #[cfg(windows)]
    {
        let _ = std::process::Command::new("taskkill")
            .args(["/T", "/F", "/PID", &pid.to_string()])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn();
    }
}

/// FORK: reads a child's stderr to completion in the background.
///
/// The handle resolves to what the child said, capped, once the pipe closes.
fn drain_stderr(stderr: tokio::process::ChildStderr) -> tokio::task::JoinHandle<String> {
    tokio::spawn(async move {
        let mut buffer = String::new();
        let mut reader = BufReader::new(stderr).lines();
        while let Ok(Some(line)) = reader.next_line().await {
            // Keep reading past the cap so the pipe never fills; just stop
            // growing the buffer we report.
            if buffer.len() <= MAX_STDERR_CHARS {
                buffer.push_str(line.trim());
                buffer.push('\n');
            }
        }
        buffer.trim().to_string()
    })
}

/// FORK: whether a failure means the recorded Claude session is unusable.
///
/// The CLI refuses an unknown id outright ("No conversation found with session
/// ID: …"), and a corrupt or partially written session file surfaces the same
/// way. Everything else — a crash, a killed process, a bad flag — leaves the
/// session resumable.
fn session_lost(detail: &str) -> bool {
    let lower = detail.to_lowercase();
    lower.contains("no conversation found")
        || (lower.contains("session") && lower.contains("not found"))
        || lower.contains("--resume")
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

/// The configured CLI location, or plain `claude` from `PATH`.
fn claude_bin() -> String {
    std::env::var(CLAUDE_BIN_ENV)
        .ok()
        .filter(|value| !value.trim().is_empty())
        .unwrap_or_else(|| DEFAULT_CLAUDE_BIN.to_string())
}

/// Builds the CLI invocation for one attempt.
///
/// Separate from spawning so the command line — the entire contract with the
/// CLI, and invisible at runtime — can be asserted on.
fn build_claude_command(
    model_slug: &str,
    effort: Option<&ReasoningEffortConfig>,
    resume_session_id: Option<&str>,
    workspace: &ClaudeCodeWorkspace,
    config_dir: Option<&std::path::Path>,
) -> Command {
    let mut command = Command::new(claude_bin());
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

    // FORK: Claude spawns its own shells, builds and test runners. Killing only
    // the CLI leaves those running against the workspace after Codex has moved
    // on, so put the whole run in one killable group.
    #[cfg(unix)]
    command.process_group(0);

    command
}

fn spawn_claude(
    model_slug: &str,
    effort: Option<&ReasoningEffortConfig>,
    resume_session_id: Option<&str>,
    workspace: &ClaudeCodeWorkspace,
    config_dir: Option<&std::path::Path>,
) -> Result<Child> {
    build_claude_command(model_slug, effort, resume_session_id, workspace, config_dir)
        .spawn()
        .map_err(|err| {
            let bin = claude_bin();
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
/// Everything one attempt needs beyond the process and the event channel.
struct AttemptContext<'a> {
    plan: &'a history::RequestPlan,
    /// Account serving this attempt; `None` = the ambient environment.
    account_dir: Option<PathBuf>,
    state: &'a ClaudeCodeThreadState,
    /// Account the agent was pinned to at spawn, recorded with the session.
    pinned_account: Option<&'a Path>,
    /// How long the CLI may stay silent before the turn is abandoned.
    idle_timeout: Option<std::time::Duration>,
}

async fn translate_stream(
    child: &mut Child,
    tx_event: &mpsc::Sender<Result<ResponseEvent>>,
    consumer_dropped: &CancellationToken,
    attempt: AttemptContext<'_>,
) -> AttemptOutcome {
    let AttemptContext {
        plan,
        account_dir,
        state,
        pinned_account,
        idle_timeout,
    } = attempt;
    let mut emitted_output = false;
    let Some(stdout) = child.stdout.take() else {
        return AttemptOutcome::Failed {
            detail: "claude_code provider could not open the CLI stdout".to_string(),
            emitted_output,
            turn_reported: false,
        };
    };
    // Drained continuously, not only when the turn fails: a chatty child that
    // fills the stderr pipe buffer would otherwise block on its own write and
    // hang the turn.
    let stderr = child.stderr.take().map(drain_stderr);
    let mut lines = BufReader::new(stdout).lines();

    let mut session_id: Option<String> = None;
    let account_label_dir = account_dir.clone();
    let mut assembler = StreamAssembler::new(tx_event);

    loop {
        let next_line = async {
            match idle_timeout {
                // A wedged CLI produces nothing at all: no events, no exit. Time
                // out on silence rather than pinning the turn open forever.
                Some(idle_timeout) => tokio::time::timeout(idle_timeout, lines.next_line())
                    .await
                    .map_err(|_| ()),
                None => Ok(lines.next_line().await),
            }
        };
        let line = tokio::select! {
            _ = consumer_dropped.cancelled() => return AttemptOutcome::ConsumerGone,
            line = next_line => line,
        };
        let line = match line {
            Ok(Ok(Some(line))) => line,
            Ok(Ok(None)) => break,
            Ok(Err(err)) => {
                return AttemptOutcome::Failed {
                    detail: format!("claude_code provider read failed: {err}"),
                    emitted_output,
                    turn_reported: false,
                };
            }
            Err(()) => {
                let seconds = idle_timeout.map(|idle| idle.as_secs()).unwrap_or_default();
                kill_process_tree(child);
                return AttemptOutcome::Failed {
                    detail: format!(
                        "claude_code turn produced no output for {seconds}s and was stopped"
                    ),
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
                        assembler.take_authored(),
                        pinned_account,
                    );
                } else {
                    state.invalidate();
                }

                let response_id = session_id.clone().unwrap_or_default();
                let token_usage = parse_token_usage(event.get("usage"));
                // FORK: the one line that shows whether the session/cache work
                // is paying off — and the only place the chosen account is
                // visible after the fact.
                match token_usage.as_ref() {
                    Some(usage) => tracing::info!(
                        account = %accounts::account_label(account_label_dir.as_deref()),
                        resumed = !plan.restart_session,
                        input_tokens = usage.input_tokens,
                        cached_input_tokens = usage.cached_input_tokens,
                        cache_write_input_tokens = usage.cache_write_input_tokens,
                        output_tokens = usage.output_tokens,
                        "claude_code turn completed"
                    ),
                    None => tracing::info!(
                        account = %accounts::account_label(account_label_dir.as_deref()),
                        resumed = !plan.restart_session,
                        "claude_code turn completed without usage; context accounting will not advance"
                    ),
                }
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
        Some(stderr) => stderr.await.unwrap_or_default(),
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
    /// FORK: fingerprints of every item this turn produced, so the next request
    /// can drop them from its tail instead of reading them back to Claude.
    authored: Vec<u64>,
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
            authored: Vec::new(),
        }
    }

    fn streamed_any_text(&self) -> bool {
        self.streamed_any_text
    }

    fn take_authored(&mut self) -> Vec<u64> {
        std::mem::take(&mut self.authored)
    }

    /// Sends a finished item and remembers it as this turn's own output.
    async fn send_done(&mut self, item: ResponseItem) -> bool {
        self.authored.push(history::item_fingerprint(&item));
        self.send(ResponseEvent::OutputItemDone(item)).await
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
            Some(ActiveItem::Reasoning(text)) => self.send_done(reasoning_item(text)).await,
            Some(ActiveItem::Message(text)) => self.send_done(message_item(text, &phase)).await,
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
        self.send_done(message_item(text, &phase)).await
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

    fn test_workspace(temp: &tempfile::TempDir) -> ClaudeCodeWorkspace {
        ClaudeCodeWorkspace {
            cwd: temp.path().join("repo"),
            extra_roots: vec![temp.path().join("sibling"), temp.path().join("repo")],
            permission_mode: "bypassPermissions",
            account_dirs: Vec::new(),
            accounts_state_path: None,
            sessions_state_path: None,
            selection: ClaudeCodeAccountSelection::default(),
            sticky_min_headroom_pct: 20.0,
            pinned_account: None,
            idle_timeout: None,
        }
    }

    /// The command line is the whole contract with the CLI, and none of it is
    /// observable at runtime — a wrong flag just produces a worse agent.
    #[test]
    fn a_resumed_turn_pins_the_account_and_reaches_every_root() {
        let temp = tempfile::tempdir().expect("tempdir");
        let workspace = test_workspace(&temp);
        let account = temp.path().join("account-a");

        let child = spawn_claude(
            "claude-opus-5",
            Some(&ReasoningEffortConfig::High),
            Some("session-42"),
            &workspace,
            Some(&account),
        );
        // Without the CLI installed the spawn fails, but the command was already
        // built; keep the assertions on what we control.
        let command = build_claude_command(
            "claude-opus-5",
            Some(&ReasoningEffortConfig::High),
            Some("session-42"),
            &workspace,
            Some(&account),
        );
        drop(child);

        let args: Vec<String> = command
            .as_std()
            .get_args()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect();
        assert!(
            args.windows(2)
                .any(|pair| pair == ["--model", "claude-opus-5"])
        );
        assert!(
            args.windows(2)
                .any(|pair| pair == ["--permission-mode", "bypassPermissions"])
        );
        assert!(args.windows(2).any(|pair| pair == ["--effort", "high"]));
        assert!(
            args.windows(2)
                .any(|pair| pair == ["--resume", "session-42"])
        );
        assert!(!args.iter().any(|arg| arg == "--session-id"));
        // The cwd plus each distinct sibling root, and no duplicate for the cwd
        // appearing in both lists.
        let add_dirs = args
            .iter()
            .enumerate()
            .filter(|(index, _)| *index > 0 && args[index - 1] == "--add-dir")
            .count();
        assert_eq!(add_dirs, 2, "{args:?}");

        let config_dir = command
            .as_std()
            .get_envs()
            .find(|(key, _)| *key == std::ffi::OsStr::new("CLAUDE_CONFIG_DIR"))
            .and_then(|(_, value)| value)
            .map(|value| value.to_string_lossy().into_owned());
        assert_eq!(
            config_dir.as_deref(),
            Some(account.to_string_lossy().as_ref())
        );
    }

    /// A turn with no session to resume must get a fresh id, not resume nothing.
    #[test]
    fn a_first_turn_asks_for_a_new_session_id() {
        let temp = tempfile::tempdir().expect("tempdir");
        let workspace = test_workspace(&temp);

        let command = build_claude_command("claude-sonnet-5", None, None, &workspace, None);

        let args: Vec<String> = command
            .as_std()
            .get_args()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect();
        assert!(args.iter().any(|arg| arg == "--session-id"));
        assert!(!args.iter().any(|arg| arg == "--resume"));
        assert!(!args.iter().any(|arg| arg == "--effort"));
        assert!(
            command
                .as_std()
                .get_envs()
                .all(|(key, _)| key != std::ffi::OsStr::new("CLAUDE_CONFIG_DIR"))
        );
    }
}
