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
// FORK: shared with the `chatgpt_web` provider.
pub(crate) mod assembler;
mod bridge;
mod control;
// FORK: shared with the `chatgpt_web` provider.
pub(crate) mod history;
mod host;
mod session_host;
mod sessions;
mod teardown;
// FORK: shared with the `chatgpt_web` provider.
pub(crate) mod state_file;
mod tools;

use crate::client_common::Prompt;
use crate::client_common::ResponseEvent;
use crate::client_common::ResponseStream;
use codex_config::config_toml::ClaudeCodeAccountSelection;
use codex_protocol::error::CodexErr;
use codex_protocol::error::Result;
use codex_protocol::models::MessagePhase;
use codex_protocol::models::ResponseItem;
use codex_protocol::openai_models::ModelInfo;
use codex_protocol::openai_models::ReasoningEffort as ReasoningEffortConfig;
use codex_protocol::protocol::AskForApproval;
use codex_protocol::protocol::SandboxPolicy;
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

pub(crate) use assembler::StreamAssembler;
pub(crate) use history::ClaudeSessionContinuity;
pub(crate) use host::ClaudeHost;
pub(crate) use session_host::SessionClaudeHost;

/// Environment override for the CLI location, mirroring how the OSS providers
/// let the endpoint be pointed elsewhere.
const CLAUDE_BIN_ENV: &str = "CODEX_CLAUDE_CODE_BIN";
const DEFAULT_CLAUDE_BIN: &str = "claude";

/// Buffer for translated events. Claude emits one event per content block and
/// per tool call, so a turn produces tens of events, not thousands.
const EVENT_CHANNEL_SIZE: usize = 256;

/// FORK: outstanding control frames waiting to be written to the CLI's stdin.
/// Bounded: a runaway producer should apply back-pressure, not grow forever.
const CONTROL_CHANNEL_SIZE: usize = 64;

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
    /// FORK: the same directory as a URI, for the exec cells of the commands
    /// Claude runs — the CLI does not report a working directory per call.
    pub(crate) cwd_uri: codex_utils_path_uri::PathUri,
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
    /// FORK: pauses before each in-place retry of an Anthropic-side failure.
    /// A field so a test can drive the retry path without sleeping.
    pub(crate) transient_retry_delays: &'static [std::time::Duration],
    /// FORK: the CLI to run — program plus any leading arguments — when it is
    /// not the one `CODEX_CLAUDE_CODE_BIN` names.
    ///
    /// Per workspace so a test can point one turn at a scripted CLI without
    /// mutating process-wide environment, and a list because on Windows a
    /// script is reached through `cmd.exe /D /Q /C`.
    pub(crate) claude_command: Option<Vec<String>>,
    /// FORK: the agent role's own instructions.
    ///
    /// These used to reach the child as part of the rendered transcript, where
    /// they sat behind tens of thousands of characters of harness scaffolding
    /// and were sometimes cut by it. They belong in the system prompt.
    pub(crate) developer_instructions: Option<String>,
    /// FORK: whether to attempt the CLI's stdio control protocol
    /// (`Feature::ClaudeCodeControlProtocol`). Attempting it is still
    /// conditional on the installed CLI answering `initialize`.
    pub(crate) control_protocol: bool,
    /// FORK: roots the Codex session considers writable, named in the system
    /// prompt so the agent does not have to discover them by trial and error.
    pub(crate) writable_roots: Vec<PathBuf>,
    /// FORK: the Codex sandbox this turn runs under, which together with the
    /// approval policy decides the CLI's permission mode.
    pub(crate) sandbox: SandboxPolicy,
    /// FORK: the session that can answer the CLI's permission requests.
    ///
    /// Attached per sampling request rather than per turn, because it needs the
    /// live `Session` and step context, which only exist there.
    pub(crate) host: Option<Arc<dyn host::ClaudeHost>>,
    /// FORK: stream partial assistant text and thinking as it is produced.
    pub(crate) stream_partial_messages: bool,
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
            cwd_uri: codex_utils_path_uri::PathUri::from_abs_path(&config.cwd),
            extra_roots: config
                .permissions
                .workspace_roots()
                .iter()
                .map(codex_utils_absolute_path::AbsolutePathBuf::to_path_buf)
                .collect(),
            permission_mode: permission_mode_for(
                &config.legacy_sandbox_policy(),
                config.permissions.approval_policy.value(),
            ),
            writable_roots: writable_roots(&config.legacy_sandbox_policy()),
            sandbox: config.legacy_sandbox_policy(),
            // Attached later, by the sampling request that has a session.
            host: None,
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
            transient_retry_delays: TRANSIENT_RETRY_DELAYS,
            claude_command: None,
            developer_instructions: config.developer_instructions.clone(),
            control_protocol: config
                .features
                .enabled(codex_features::Feature::ClaudeCodeControlProtocol),
            // Shares the control-protocol flag: both are the same bet on the
            // installed CLI's newer stdio surface.
            stream_partial_messages: config
                .features
                .enabled(codex_features::Feature::ClaudeCodeControlProtocol),
            // Decided per sampling request, once the session is known.
        }
    }
}

/// FORK: the system prompt handed to the CLI for one turn.
///
/// Three parts, in the order the child needs them: who it is and how to report,
/// what it is allowed to touch, and finally the role's own instructions. It is
/// delivered through the control protocol's `initialize.appendSystemPrompt`
/// rather than a command-line flag — Windows caps a command line at 32k, and
/// role instructions alone can exceed that.
pub(crate) fn claude_system_prompt(workspace: &ClaudeCodeWorkspace) -> String {
    let mut sections: Vec<String> = Vec::new();

    sections.push(
        "You are a subagent running inside a Codex session, under the direction of a parent \
agent. You do not share the parent's conversation: everything you need is in the task you \
were given. If a premise is missing, ask the parent rather than inventing one.\n\n\
Report your result in your final message of this turn. To say something before you are \
done, call `mcp__codex__send_message` with `target: \"..\"`. Do not try to reach the user \
directly and do not create or fork threads."
            .to_string(),
    );

    let mut environment = format!(
        "Working directory: {}\nPermission mode: {}",
        workspace.cwd.display(),
        workspace.permission_mode
    );
    let extra_roots: Vec<String> = workspace
        .extra_roots
        .iter()
        .filter(|root| *root != &workspace.cwd)
        .map(|root| root.display().to_string())
        .collect();
    if !extra_roots.is_empty() {
        environment.push_str(&format!(
            "
Other readable roots: {}",
            extra_roots.join(", ")
        ));
    }
    // Saying which roots are writable up front saves the agent a round of
    // trial-and-error edits the sandbox would refuse anyway.
    let writable: Vec<String> = workspace
        .writable_roots
        .iter()
        .map(|root| root.display().to_string())
        .collect();
    if !writable.is_empty() {
        environment.push_str(&format!(
            "
Writable roots: {}",
            writable.join(", ")
        ));
    } else if workspace.permission_mode == "plan" {
        environment.push_str(
            "
This turn is read-only: inspect and report, do not modify files.",
        );
    }
    // FORK: `plan` is the CLI's read-only mode and it does not hand the agent a
    // `Bash` tool at all. Children spent a dozen calls each rediscovering that
    // by failure; say it once instead.
    if workspace.permission_mode == "plan" {
        environment.push_str(
            "
Bash is unavailable in this read-only session; use Read, Glob and Grep, and report commands you would have run.",
        );
    }
    // Saying so up front avoids a turn spent discovering it by failure.
    if !workspace.sandbox.has_full_network_access() {
        environment.push_str(
            "\nNetwork access is restricted for this turn; prefer what is already vendored.",
        );
    }
    environment.push_str(
        "\nThe working tree is shared with other agents and is dirty on purpose. Never run \
`git reset`, `checkout`, `clean`, `stash`, or `commit`.",
    );
    sections.push(environment);

    if let Some(instructions) = workspace
        .developer_instructions
        .as_deref()
        .map(str::trim)
        .filter(|instructions| !instructions.is_empty())
    {
        sections.push(instructions.to_string());
    }

    sections.join("\n\n")
}

/// FORK: how a caller named a Claude account.
pub(crate) enum AccountAlias {
    /// Let the selection policy decide, as if no account had been named.
    Auto,
    /// A specific configured account directory.
    Dir(PathBuf),
}

/// FORK: which Claude account a thread is currently spending, for `list_agents`
/// and `wait_agent`.
///
/// Reads the persisted session record rather than any live state: the parent
/// asking is a different thread, and the account is exactly the thing that is
/// not visible from there.
pub(crate) fn thread_account_label(
    codex_home: &std::path::Path,
    thread_id: codex_protocol::ThreadId,
) -> Option<String> {
    let path = codex_home.join(sessions::SESSIONS_STATE_FILE_NAME);
    let (continuity, pinned) = sessions::load(&path, &thread_id.to_string())?;
    let account = continuity.account_dir.or(pinned)?;
    Some(accounts::account_label(Some(&account)))
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

/// FORK: maps the Codex sandbox and approval policy onto a Claude Code
/// permission mode.
///
/// | sandbox | approval | mode |
/// |---|---|---|
/// | read-only | anything | `plan` (+ a read-only tool set) |
/// | anything writable | never | `bypassPermissions` |
/// | anything writable | anything else | `auto` (+ the prompt tool) |
///
/// Two rules, both learned the hard way.
///
/// `acceptEdits` looks like the right fit for `workspace-write`, and it is not:
/// it auto-approves file edits but, headless, *refuses* every other request
/// outright. Tried against the real CLI, the agent's `Write` succeeded and its
/// `cat` came back "This command requires approval" — it silently loses its
/// shell, and with it every build and test. The mode is not in this table at
/// all. Nor does it buy the confinement its name suggests: what the child may
/// touch is decided by `--add-dir`, not by the permission mode.
///
/// `bypassPermissions` makes the CLI suppress `can_use_tool` entirely, so it
/// must never be paired with the permission prompt tool — there would be nothing
/// to answer. The session-host preparation path upgrades a writable subagent to
/// `auto` after it acquires a mutation guard; this mapping remains the root and
/// no-host policy mapping so root `Never` keeps its existing behavior.
pub(crate) fn permission_mode_for(
    sandbox: &SandboxPolicy,
    approval_policy: AskForApproval,
) -> &'static str {
    match sandbox {
        // Nothing is writable, so the agent may look but not touch.
        SandboxPolicy::ReadOnly { .. } => "plan",
        SandboxPolicy::DangerFullAccess
        | SandboxPolicy::ExternalSandbox { .. }
        | SandboxPolicy::WorkspaceWrite { .. } => match approval_policy {
            // Codex has stopped asking, so the child must not ask either.
            AskForApproval::Never => "bypassPermissions",
            _ => "auto",
        },
    }
}

/// Resolve the Claude mode after provider-side ownership admission.
///
/// A mutable subagent must use `auto` even when the parent policy is `Never`,
/// because only that mode reaches the host's `can_use_tool` callback. The host
/// applies the parent policy after the ownership guard; root callers retain the
/// regular [`permission_mode_for`] mapping.
pub(crate) fn permission_mode_for_access(
    sandbox: &SandboxPolicy,
    approval_policy: AskForApproval,
    requires_tool_authorization: bool,
) -> &'static str {
    if requires_tool_authorization && !matches!(sandbox, SandboxPolicy::ReadOnly { .. }) {
        "auto"
    } else {
        permission_mode_for(sandbox, approval_policy)
    }
}

/// Whether the CLI should ask this host before running a tool.
///
/// Only in `auto`: `bypassPermissions` suppresses `can_use_tool` on the CLI
/// side, and `plan`/`acceptEdits` have already decided the answer.
pub(crate) fn uses_permission_prompt(permission_mode: &str) -> bool {
    permission_mode == "auto"
}

/// Tools a read-only child is allowed to load at all.
///
/// `plan` mode already refuses mutations, but advertising the write tools
/// invites the agent to try and then report a failure it could not avoid.
const READ_ONLY_TOOLS: &str = "Read,Glob,Grep,WebFetch,WebSearch,TodoWrite,Task";

/// The roots this sandbox lets the child write to.
fn writable_roots(sandbox: &SandboxPolicy) -> Vec<PathBuf> {
    match sandbox {
        SandboxPolicy::WorkspaceWrite { writable_roots, .. } => writable_roots
            .iter()
            .map(codex_utils_absolute_path::AbsolutePathBuf::to_path_buf)
            .collect(),
        SandboxPolicy::DangerFullAccess | SandboxPolicy::ExternalSandbox { .. } => Vec::new(),
        SandboxPolicy::ReadOnly { .. } => Vec::new(),
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
        None => {
            let cwd = codex_utils_absolute_path::AbsolutePathBuf::current_dir().map_err(|err| {
                CodexErr::UnsupportedOperation(format!(
                    "claude_code provider could not resolve a workspace: {err}"
                ))
            })?;
            ClaudeCodeWorkspace {
                cwd_uri: codex_utils_path_uri::PathUri::from_abs_path(&cwd),
                cwd: cwd.to_path_buf(),
                extra_roots: Vec::new(),
                sandbox: SandboxPolicy::new_read_only_policy(),
                writable_roots: Vec::new(),
                host: None,
                permission_mode: permission_mode_for(
                    &SandboxPolicy::new_read_only_policy(),
                    AskForApproval::OnRequest,
                ),
                account_dirs: Vec::new(),
                accounts_state_path: None,
                sessions_state_path: None,
                selection: ClaudeCodeAccountSelection::default(),
                sticky_min_headroom_pct: 0.0,
                pinned_account: None,
                idle_timeout: None,
                transient_retry_delays: TRANSIENT_RETRY_DELAYS,
                claude_command: None,
                developer_instructions: None,
                control_protocol: false,
                stream_partial_messages: false,
            }
        }
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
        /// FORK: the `result` frame that reported it, when there was one. Its
        /// `subtype` says what kind of failure this was far more reliably than
        /// the rendered text does.
        frame: Option<JsonValue>,
        /// FORK: the `error` field of the CLI's `isApiErrorMessage` frame
        /// (`overloaded`, `server_error`, `rate_limit`, …). The `result`
        /// subtype is `error_during_execution` for all of them; this is what
        /// separates "Anthropic is down" from "this account is spent".
        api_error: Option<String>,
        /// FORK: the Claude session the attempt ran in, so a retry can
        /// `--resume` it instead of replaying the whole transcript.
        session_id: Option<String>,
        /// FORK: fingerprints of the items the attempt already delivered to
        /// Codex. A retry must carry them forward, or the partial answer is
        /// read back to Claude as new input on the next turn.
        authored: Vec<u64>,
    },
}

/// FORK: why an attempt ended the whole turn rather than just itself.
enum AttemptAbort {
    /// Report this to the consumer and stop; another account would fail the
    /// same way.
    Fatal(CodexErr),
    /// The consumer is gone; stop without reporting anything.
    Silent,
}

/// FORK: how many extra attempts one account gets after an Anthropic-side
/// failure. The CLI has already exhausted its own retries by the time we see
/// one, so this layer is deliberately slow and short.
const TRANSIENT_MAX_RETRIES: usize = 2;

/// FORK: pauses before each of those attempts.
const TRANSIENT_RETRY_DELAYS: &[std::time::Duration] = &[
    std::time::Duration::from_secs(10),
    std::time::Duration::from_secs(30),
];

/// FORK: what a retry has to carry over from the attempt that failed.
struct RetryPlan {
    /// The Claude session to `--resume`, so the CLI keeps its context and cache.
    session_id: String,
    /// Replaces the planned turn text: the input was already delivered.
    turn_text: String,
    /// Fingerprints the failed attempt authored.
    authored: Vec<u64>,
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
    if tx_event.send(Ok(ResponseEvent::Created {
        guardian_ticket: None,
    })).await.is_err() {
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
        // Held across every attempt on this account so a concurrent spawn can
        // see it is already busy and pick a quieter one.
        let _in_flight = accounts::InFlightGuard::acquire(account_dir.as_deref());
        let mut retries = 0usize;
        let mut retry: Option<RetryPlan> = None;

        let (outcome, class) = loop {
            let outcome = match run_attempt(
                &input,
                &model_slug,
                effort.as_ref(),
                &workspace,
                &state,
                &tx_event,
                &consumer_dropped,
                account_dir.clone(),
                retry.take(),
            )
            .await
            {
                Ok(outcome) => outcome,
                Err(AttemptAbort::Fatal(err)) => {
                    let _ = tx_event.send(Err(err)).await;
                    return;
                }
                Err(AttemptAbort::Silent) => return,
            };

            let AttemptOutcome::Failed {
                detail,
                turn_reported,
                frame,
                api_error,
                session_id,
                authored,
                ..
            } = &outcome
            else {
                break (outcome, None);
            };
            let class = match frame.as_ref() {
                Some(frame) => {
                    accounts::classify_result_failure(frame, api_error.as_deref(), detail)
                }
                None => accounts::classify_failure(detail),
            };

            // FORK: Anthropic failing is worth waiting out on this account
            // rather than failing over — the next account talks to the same
            // API. The CLI has already spent its own retries by the time we
            // see one, so this second layer is deliberately slow and short.
            //
            // Only a clean `result` frame may be retried: a stream that died
            // mid-item left a half-delivered answer behind, and there is no
            // session id to resume it from.
            let session = session_id.clone().filter(|_| {
                class.is_retryable_in_place() && *turn_reported && retries < TRANSIENT_MAX_RETRIES
            });
            let Some(session) = session else {
                break (outcome, Some(class));
            };

            let delay = workspace
                .transient_retry_delays
                .get(retries)
                .copied()
                .unwrap_or_default();
            let label = accounts::account_label(account_dir.as_deref());
            let detail_line = detail.lines().next().unwrap_or(detail).trim().to_string();
            let attempt = retries + 1;
            warn!(
                "claude_code: Anthropic error on {label} ({detail_line}); retrying {attempt}/{TRANSIENT_MAX_RETRIES} in {}s",
                delay.as_secs()
            );
            if let Some(host) = workspace.host.as_ref() {
                host.notify_retry(
                    format!(
                        "Anthropic overloaded; retrying {attempt}/{TRANSIENT_MAX_RETRIES} in {}s",
                        delay.as_secs()
                    ),
                    detail_line.clone(),
                )
                .await;
            }
            let authored = authored.clone();
            tokio::select! {
                _ = consumer_dropped.cancelled() => return,
                () = tokio::time::sleep(delay) => {}
            }
            retry = Some(RetryPlan {
                session_id: session,
                turn_text: format!(
                    "[codex] The previous response was cut off by a transient Anthropic error ({detail_line}). Continue from where you left off; do not repeat completed work."
                ),
                authored,
            });
            retries += 1;
        };

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
                ..
            } => {
                // Only a session that cannot be resumed is worth forgetting.
                // `delivered_items` advances on success alone, so keeping the
                // resume point after a transient failure just re-sends the same
                // tail — while dropping it replays the entire transcript and
                // throws away the prompt cache with it.
                if session_lost(&detail) {
                    state.invalidate();
                }
                let class = class.unwrap_or(accounts::FailureClass::Other);
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

                let message = if retries > 0 {
                    format!(
                        "claude_code turn failed after {} attempts (Anthropic server error) [{label}]: {detail}",
                        retries + 1
                    )
                } else if failures.len() > 1 {
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

/// FORK: one attempt against one account.
///
/// Split out of [`run_turn`] so an Anthropic-side failure can be retried in
/// place: the retry keeps the account's in-flight guard, resumes the same
/// Claude session, and carries the failed attempt's items forward.
///
/// `Err` ends the whole turn — a missing binary or a wedged child fails the
/// same way on every account.
#[allow(clippy::too_many_arguments)]
async fn run_attempt(
    input: &[ResponseItem],
    model_slug: &str,
    effort: Option<&ReasoningEffortConfig>,
    workspace: &ClaudeCodeWorkspace,
    state: &ClaudeCodeThreadState,
    tx_event: &mpsc::Sender<Result<ResponseEvent>>,
    consumer_dropped: &CancellationToken,
    account_dir: Option<PathBuf>,
    retry: Option<RetryPlan>,
) -> std::result::Result<AttemptOutcome, AttemptAbort> {
    let continuity = state.snapshot();
    let continuity = if continuity_matches_account(&continuity, account_dir.as_deref()) {
        continuity
    } else {
        // The recorded Claude session lives in another account's history;
        // replay the conversation into a fresh session instead.
        ClaudeSessionContinuity::default()
    };
    let mut plan = history::plan_request(input, &continuity);
    let mut resume_session_id = if plan.restart_session {
        None
    } else {
        continuity.session_id.clone()
    };
    let mut authored_seed = Vec::new();
    // FORK: a retry continues the session the failed attempt was already in.
    // The input it carried has been delivered; what goes on stdin is a nudge to
    // finish, and the items that attempt already produced travel with it so the
    // next turn does not read them back to Claude as new input.
    if let Some(retry) = retry {
        plan.restart_session = false;
        plan.turn_text = retry.turn_text;
        resume_session_id = Some(retry.session_id);
        authored_seed = retry.authored;
    }

    let spawned = match spawn_claude(
        model_slug,
        effort,
        resume_session_id.as_deref(),
        workspace,
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
                model_slug,
                effort,
                resume_session_id.as_deref(),
                workspace,
                account_dir.as_deref(),
            )
        }
    };
    let mut child = match spawned {
        Ok(child) => child,
        Err(err) => {
            // Startup failures (missing binary) are account-independent:
            // retrying elsewhere would fail identically.
            return Err(AttemptAbort::Fatal(err));
        }
    };

    let Some(mut stdin) = child.stdin.take() else {
        return Err(AttemptAbort::Fatal(CodexErr::UnsupportedOperation(
            "claude_code provider could not open the CLI stdin".to_string(),
        )));
    };
    // FORK: stdin stays open for the length of the turn and is driven by a
    // writer task. The old code wrote one line and closed the pipe, which
    // made the CLI's control protocol unreachable: every "ask" decision it
    // took was terminal because there was nobody left to ask.
    //
    // Writing is concurrent with reading stdout either way: a replayed
    // transcript easily exceeds the pipe buffer, and the CLI starts emitting
    // events immediately, so writing to completion first would deadlock both
    // sides on a full pipe.
    let (tx_stdin, mut rx_stdin) = mpsc::channel::<String>(CONTROL_CHANNEL_SIZE);
    let writer = tokio::spawn(async move {
        while let Some(line) = rx_stdin.recv().await {
            if let Err(err) = stdin.write_all(format!("{line}\n").as_bytes()).await {
                warn!("claude_code: failed to write to the CLI: {err}");
                break;
            }
            if let Err(err) = stdin.flush().await {
                warn!("claude_code: failed to flush the CLI stdin: {err}");
                break;
            }
        }
        // Closing stdin is what tells the CLI the turn is over; without it a
        // CLI that has finished its work keeps the pipe (and the process)
        // alive waiting for more input.
        let _ = stdin.shutdown().await;
    });
    let control = control::ControlChannel::new(tx_stdin.clone());
    let control_protocol_enabled = workspace.control_protocol;

    // FORK: the bridge only exists when there is a session behind it.
    let bridge = workspace
        .host
        .clone()
        .map(|host| Arc::new(bridge::McpBridge::new(host)));
    let sdk_mcp_servers: &[&str] = if bridge.is_some() {
        &[bridge::BRIDGE_SERVER_NAME]
    } else {
        &[]
    };
    // The handshake carries the system prompt and the in-process MCP bridge.
    //
    // Sent, not awaited: its answer arrives on stdout, and the only task
    // that reads stdout is the one we have not started yet. Waiting here
    // would stall every turn until the request timed out. `translate_stream`
    // watches for the answer instead, and drops the bridge if the CLI says
    // it cannot do it.
    let (mut control, initialize_request_id) = if control_protocol_enabled {
        let request_id = control
            .send_request(
                "initialize",
                control::initialize_payload(
                    Some(&claude_system_prompt(workspace)),
                    sdk_mcp_servers,
                ),
            )
            .await;
        (Some(control), request_id)
    } else {
        // FORK: drop the channel's `tx_stdin` clone explicitly. Shadowing the
        // binding with `None` leaves the original `ControlChannel` — and its
        // sender — alive to the end of the attempt, so the writer never sees
        // EOF and teardown fails every turn with `WriterTimedOut`.
        drop(control);
        (None, None)
    };

    let turn_line = serde_json::json!({
        "type": "user",
        "message": { "role": "user", "content": plan.turn_text },
    })
    .to_string();
    if tx_stdin.send(turn_line).await.is_err() {
        warn!("claude_code: the CLI closed stdin before the turn was written");
    }

    let outcome = translate_stream(
        &mut child,
        tx_event,
        consumer_dropped,
        AttemptContext {
            plan: &plan,
            account_dir: account_dir.clone(),
            state,
            pinned_account: workspace.pinned_account.as_deref(),
            idle_timeout: workspace.idle_timeout,
            control: control.as_ref(),
            cwd: workspace.cwd_uri.clone(),
            host: workspace.host.clone(),
            bridge: bridge.clone(),
            accounts_state_path: workspace.accounts_state_path.clone(),
            initialize_request_id: initialize_request_id.clone(),
            authored_seed,
        },
    )
    .await;

    // Ending the turn: close every sender before joining the writer. In
    // particular, `ControlChannel` owns a clone of `tx_stdin`; dropping only
    // the local sender leaves the writer waiting for an EOF that can never
    // arrive. Cancellation is the one explicit path that requests process
    // termination; a writer timeout is reported, not silently converted into
    // a kill.
    let termination = if consumer_dropped.is_cancelled() {
        teardown::TerminationRequest::ExplicitCancellation
    } else {
        teardown::TerminationRequest::WaitForExit
    };
    let teardown_result =
        teardown::finish(&mut child, control.take(), tx_stdin, writer, termination).await;
    if let Err(error) = teardown_result {
        if !error.state.child_exited {
            // `finish` deliberately does not kill a child that ignored
            // EOF. Keep ownership in a bounded reaper so it is eventually
            // collected, while stream cancellation remains the explicit
            // process-tree termination path.
            teardown::spawn_reaper(child, consumer_dropped.clone());
        }
        if matches!(outcome, AttemptOutcome::ConsumerGone) {
            return Err(AttemptAbort::Silent);
        }
        return Err(AttemptAbort::Fatal(CodexErr::UnsupportedOperation(
            error.to_string(),
        )));
    }
    Ok(outcome)
}

/// FORK: the plain text of an `assistant` frame, joined across its blocks.
fn assistant_frame_text(event: &JsonValue) -> String {
    event
        .get("message")
        .and_then(|message| message.get("content"))
        .and_then(JsonValue::as_array)
        .map(|blocks| {
            blocks
                .iter()
                .filter(|block| block.get("type").and_then(JsonValue::as_str) == Some("text"))
                .filter_map(|block| block.get("text").and_then(JsonValue::as_str))
                .collect::<Vec<_>>()
                .join("")
        })
        .unwrap_or_default()
        .trim()
        .to_string()
}

/// FORK: the first entry of a `result` frame's `errors[]`.
///
/// An `error_during_execution` result leaves `result` empty and puts what went
/// wrong here, so a turn that died on an API error used to report only its
/// subtype.
fn result_errors_text(event: &JsonValue) -> Option<String> {
    let first = event
        .get("errors")
        .and_then(JsonValue::as_array)
        .and_then(|errors| errors.first())?;
    let text = first
        .as_str()
        .map(str::to_string)
        .or_else(|| {
            first
                .get("message")
                .and_then(JsonValue::as_str)
                .map(str::to_string)
        })
        .unwrap_or_else(|| first.to_string());
    let text = text.trim().to_string();
    (!text.is_empty()).then_some(text)
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

/// FORK: the Claude usage windows for the account that just served a turn.
///
/// Reads the snapshot cached when the account was chosen, which the selection
/// path refreshes on its own TTL. Asking the CLI directly (`get_usage`) would be
/// fresher, but the answer arrives on stdout inside the very loop that would be
/// waiting for it — and by `result` the CLI is already on its way out.
///
/// Never fabricates zeros: an unknown window is reported as unknown, because a
/// full green bar for an account we cannot see reads as good news rather than as
/// no news.
fn claude_rate_limits(
    accounts_state_path: Option<&Path>,
    account_dir: Option<&Path>,
) -> Option<codex_protocol::protocol::RateLimitSnapshot> {
    let label = account_dir.map(|dir| accounts::account_label(Some(dir)));
    let state_path = accounts_state_path?;
    accounts::cached_usage(state_path, account_dir)?.to_rate_limit_snapshot(label)
}

/// FORK: whether a failure means the recorded Claude session is unusable.
///
/// The CLI refuses an unknown id outright ("No conversation found with session
/// ID: …"), and a corrupt or partially written session file surfaces the same
/// way. Everything else — a crash, a killed process, a bad flag — leaves the
/// session resumable.
fn session_lost(detail: &str) -> bool {
    let lower = detail.to_lowercase();
    if lower.contains("no conversation found")
        || (lower.contains("session") && lower.contains("not found"))
    {
        return true;
    }
    // FORK: `--resume` alone used to be enough, which threw the session away
    // every time the CLI printed its help text or named the flag in an
    // unrelated complaint — and each false positive costs a full transcript
    // replay and the prompt cache with it.
    lower.contains("--resume")
        && (lower.contains("invalid") || lower.contains("unknown") || lower.contains("failed"))
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
    let mut command = match workspace.claude_command.as_deref() {
        Some([program, leading @ ..]) => {
            let mut command = Command::new(program);
            command.args(leading);
            command
        }
        _ => Command::new(claude_bin()),
    };
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

    // FORK: give the CLI somewhere to send its permission questions. Without
    // this every "ask" decision it reaches is terminal, which is what silently
    // blocked builds and tests. Never in `bypassPermissions`: the CLI suppresses
    // `can_use_tool` there, so the flag would only be misleading.
    if workspace.host.is_some() && uses_permission_prompt(workspace.permission_mode) {
        command.args(["--permission-prompt-tool", "stdio"]);
    }
    // A read-only turn should not even load the tools it is not allowed to use.
    if workspace.permission_mode == "plan" {
        command.args(["--tools", READ_ONLY_TOOLS]);
    }
    // The bridge's tools are already gated by Codex's own allow-list; asking the
    // user again for each call would make the child feel permission-bound for no
    // added safety.
    if workspace.host.is_some() {
        command.args(["--allowedTools", bridge::BRIDGE_ALLOWED_TOOLS]);
    }
    // FORK: without this the CLI emits nothing between one completed block and
    // the next, so a child thinking through a hard problem looks identical to a
    // wedged one for minutes at a time.
    if workspace.stream_partial_messages {
        command.arg("--include-partial-messages");
    }

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

    if let Some(effort) = effort.and_then(claude_effort) {
        command.args(["--effort", effort]);
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

/// FORK: the Codex effort scale is wider than the CLI's. Passing `ultra` (the
/// user's default) straight through made the CLI reject the whole run, so the
/// request is clamped down to the nearest level the CLI does accept, never up.
fn claude_effort(effort: &ReasoningEffortConfig) -> Option<&'static str> {
    match effort {
        ReasoningEffortConfig::None | ReasoningEffortConfig::Minimal => Some("low"),
        ReasoningEffortConfig::Low => Some("low"),
        ReasoningEffortConfig::Medium => Some("medium"),
        ReasoningEffortConfig::High => Some("high"),
        ReasoningEffortConfig::XHigh
        | ReasoningEffortConfig::Max
        | ReasoningEffortConfig::Ultra => Some("max"),
        // A value this build does not know: let the CLI pick its own default
        // rather than forwarding a string it may reject. `Persistent` is an
        // OpenAI proactivity mode (wire effort "disabled"), not a depth.
        ReasoningEffortConfig::Custom(_) | ReasoningEffortConfig::Persistent => None,
    }
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
    /// FORK: the control channel for this attempt, when the CLI speaks the
    /// protocol. `None` reproduces the pre-control behavior exactly.
    control: Option<&'a control::ControlChannel>,
    /// FORK: directory the CLI runs in, shown on the exec cells of the commands
    /// it runs — the CLI does not report one per call.
    cwd: codex_utils_path_uri::PathUri,
    /// FORK: the session that answers the CLI's permission requests.
    host: Option<Arc<dyn host::ClaudeHost>>,
    /// FORK: the in-process MCP server the CLI calls back into.
    bridge: Option<Arc<bridge::McpBridge>>,
    /// FORK: where the cached per-account usage lives, for the rate-limit
    /// snapshot reported at the end of the turn.
    accounts_state_path: Option<PathBuf>,
    /// FORK: the id of the `initialize` request, so its answer can be
    /// recognized among the CLI's other control responses.
    initialize_request_id: Option<String>,
    /// FORK: fingerprints an earlier attempt on this turn already authored.
    authored_seed: Vec<u64>,
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
        control,
        cwd,
        host,
        mut bridge,
        accounts_state_path,
        initialize_request_id,
        authored_seed,
    } = attempt;
    let mut emitted_output = false;
    let Some(stdout) = child.stdout.take() else {
        return AttemptOutcome::Failed {
            detail: "claude_code provider could not open the CLI stdout".to_string(),
            emitted_output,
            turn_reported: false,
            frame: None,
            api_error: None,
            session_id: None,
            authored: Vec::new(),
        };
    };
    // Drained continuously, not only when the turn fails: a chatty child that
    // fills the stderr pipe buffer would otherwise block on its own write and
    // hang the turn.
    let stderr = child.stderr.take().map(drain_stderr);
    let mut lines = BufReader::new(stdout).lines();

    let mut session_id: Option<String> = None;
    // FORK: the CLI announces an API failure as an assistant frame flagged
    // `isApiErrorMessage`, and only afterwards as an error `result` whose
    // subtype is the same `error_during_execution` it uses for everything else.
    // The flagged frame is the only place the failure is named.
    let mut api_error: Option<String> = None;
    let mut api_error_text: Option<String> = None;
    let account_label_dir = account_dir.clone();
    let mut assembler = StreamAssembler::new(tx_event).with_authored(authored_seed);
    // FORK: pairs each `tool_use` with the `tool_result` that closes it.
    let mut pending_tool_uses = tools::PendingToolUses::new(cwd);

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
                    frame: None,
                    api_error,
                    session_id,
                    authored: assembler.take_authored(),
                };
            }
            Err(()) => {
                let seconds = idle_timeout.map(|idle| idle.as_secs()).unwrap_or_default();
                teardown::cancel_process_tree(child).await;
                return AttemptOutcome::Failed {
                    detail: format!(
                        "claude_code turn produced no output for {seconds}s and was stopped"
                    ),
                    emitted_output,
                    turn_reported: false,
                    frame: None,
                    api_error,
                    session_id,
                    authored: assembler.take_authored(),
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
            // FORK: the CLI is asking us something mid-turn. Every recognized
            // frame must be answered — one left hanging stalls the CLI's own
            // turn until it times out, which reads to the parent as a wedged
            // agent.
            Some("control_request") => {
                let Some(control) = control.as_ref() else {
                    // We never opened the channel, so we cannot answer on it.
                    debug!("claude_code: ignoring a control_request with no control channel");
                    continue;
                };
                let Some((request_id, request)) = control::classify_request(&event) else {
                    continue;
                };
                handle_control_request(
                    control,
                    host.as_ref(),
                    bridge.as_ref(),
                    &request_id,
                    request,
                )
                .await;
            }
            Some("control_response") => {
                let Some(outcome) = control
                    .as_ref()
                    .and_then(|control| control.resolve_response(&event))
                else {
                    continue;
                };
                // FORK: this is the handshake's answer. A CLI that refuses it
                // does not host our MCP server either, so stop offering it —
                // the turn itself carries on regardless.
                if Some(&outcome.request_id) == initialize_request_id.as_ref()
                    && let Err(error) = &outcome.result
                {
                    warn!(
                        "claude_code: CLI refused `initialize` ({error}); running without the bridge"
                    );
                    bridge = None;
                }
            }
            // FORK: incremental text and thinking, so a working child looks
            // like one. The completed `assistant` frames still arrive and are
            // what actually build the items; these only paint.
            Some("stream_event") => {
                let Some(delta) = event.get("event").and_then(|inner| inner.get("delta")) else {
                    continue;
                };
                let alive = match delta.get("type").and_then(JsonValue::as_str) {
                    Some("text_delta") => {
                        let text = delta.get("text").and_then(JsonValue::as_str);
                        match text.filter(|text| !text.is_empty()) {
                            Some(text) => assembler.push_text_delta(text).await,
                            None => true,
                        }
                    }
                    Some("thinking_delta") => {
                        let text = delta.get("thinking").and_then(JsonValue::as_str);
                        match text.filter(|text| !text.is_empty()) {
                            Some(text) => assembler.push_reasoning_delta(text).await,
                            None => true,
                        }
                    }
                    _ => true,
                };
                if !alive {
                    return AttemptOutcome::ConsumerGone;
                }
            }
            Some("system") => {
                if let Some(id) = event.get("session_id").and_then(JsonValue::as_str) {
                    session_id = Some(id.to_string());
                }
            }
            Some("assistant") => {
                // FORK: an API failure, not the agent speaking. Pushing it
                // through the assembler wrote "API Error: 529 ..." into the
                // Codex transcript as the agent's own answer, and made every
                // such attempt look like it had produced output. Keep what it
                // says, emit nothing yet.
                if event.get("isApiErrorMessage").and_then(JsonValue::as_bool) == Some(true) {
                    if let Some(error) = event.get("error").and_then(JsonValue::as_str) {
                        api_error = Some(error.to_string());
                    }
                    let text = assistant_frame_text(&event);
                    if !text.is_empty() {
                        api_error_text = Some(text);
                    }
                    continue;
                }
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
                                // FORK: Claude executed this itself. Open a real
                                // exec / file-change / MCP cell for it instead of
                                // the one line of reasoning text this used to be.
                                match pending_tool_uses.start(&block) {
                                    Some(started) => {
                                        // Close any prose run first, so the cell
                                        // does not land inside a message.
                                        if !assembler.close(MessagePhase::Commentary).await {
                                            return AttemptOutcome::ConsumerGone;
                                        }
                                        (assembler.send_provider_tool(started).await, true)
                                    }
                                    None => (true, false),
                                }
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
            // FORK: the CLI reports each tool's outcome in a `user` frame whose
            // content holds `tool_result` blocks. These used to be discarded
            // outright, which is why a Claude child showed no command output and
            // no diff.
            Some("user") => {
                let blocks = event
                    .get("message")
                    .and_then(|message| message.get("content"))
                    .and_then(JsonValue::as_array)
                    .cloned()
                    .unwrap_or_default();
                for block in blocks {
                    if block.get("type").and_then(JsonValue::as_str) != Some("tool_result") {
                        continue;
                    }
                    let Some(completed) = pending_tool_uses.complete(&block) else {
                        continue;
                    };
                    if !assembler.send_provider_tool(completed).await {
                        return AttemptOutcome::ConsumerGone;
                    }
                    emitted_output = true;
                }
            }
            Some("result") => {
                pending_tool_uses.clear();
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
                    // FORK: an `error_during_execution` result carries its
                    // text in `errors[]`, not in `result`, and the flagged
                    // assistant frame said it first.
                    let detail = if !result_text.is_empty() {
                        result_text.to_string()
                    } else if let Some(text) = result_errors_text(&event) {
                        text
                    } else if let Some(text) = api_error_text.clone() {
                        text
                    } else {
                        event
                            .get("subtype")
                            .and_then(JsonValue::as_str)
                            .unwrap_or("unknown error")
                            .to_string()
                    };
                    return AttemptOutcome::Failed {
                        detail,
                        emitted_output,
                        turn_reported: true,
                        frame: Some(event.clone()),
                        api_error,
                        session_id,
                        authored: assembler.take_authored(),
                    };
                }

                // FORK: an API failure the CLI recovered from on its own.
                // It was held back in case it ended the turn; it did not, so
                // say it once rather than losing it.
                if let Some(text) = api_error_text.take()
                    && !assembler.emit_message(text, MessagePhase::Commentary).await
                {
                    return AttemptOutcome::ConsumerGone;
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
                // FORK: the Claude window is what actually limits this thread,
                // and the status line had no way to show it. Ask the CLI first
                // — it knows exactly — and fall back to the usage we cached
                // when choosing the account.
                if let Some(snapshot) =
                    claude_rate_limits(accounts_state_path.as_deref(), account_label_dir.as_deref())
                    && tx_event
                        .send(Ok(ResponseEvent::RateLimits(snapshot)))
                        .await
                        .is_err()
                {
                    return AttemptOutcome::ConsumerGone;
                }

                if tx_event
                    .send(Ok(ResponseEvent::Completed {
                        response_id,
                        token_usage,
                        usage_metadata: None,
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
        frame: None,
        api_error,
        session_id,
        authored: assembler.take_authored(),
    }
}

/// FORK: answers one inbound control request.
///
/// Phase 3 establishes the transport only: approvals (Phase 4) and the MCP
/// bridge (Phase 6) replace the refusals below with real decisions. Refusing
/// explicitly is still strictly better than not answering — the CLI learns
/// immediately that the host cannot help and continues, instead of stalling.
async fn handle_control_request(
    control: &control::ControlChannel,
    host: Option<&Arc<dyn host::ClaudeHost>>,
    bridge: Option<&Arc<bridge::McpBridge>>,
    request_id: &str,
    request: control::InboundControl,
) {
    match request {
        control::InboundControl::CanUseTool(can_use_tool) => {
            let decision = match host {
                Some(host) => host.approve_tool(&can_use_tool).await,
                // Without a host the safe answer is "no, but keep going":
                // denying is information the agent can work around, whereas
                // interrupting loses the whole turn.
                None => control::ToolPermissionDecision::Deny {
                    message: format!(
                        "`{}` was not approved: this Codex session cannot review tool permissions right now.",
                        can_use_tool.tool_name
                    ),
                    interrupt: false,
                },
            };
            control.respond_tool_permission(request_id, &decision).await;
        }
        control::InboundControl::McpMessage { server, message } => {
            match bridge.filter(|_| server == bridge::BRIDGE_SERVER_NAME) {
                Some(bridge) => match bridge.handle(&message).await {
                    Some(response) => {
                        control
                            .respond_success(
                                request_id,
                                serde_json::json!({ "mcp_response": response }),
                            )
                            .await;
                    }
                    // A JSON-RPC notification has no reply, but the control
                    // request still needs one or the CLI stalls.
                    None => {
                        control
                            .respond_success(request_id, serde_json::json!({}))
                            .await
                    }
                },
                None => {
                    control
                        .respond_error(
                            request_id,
                            &format!(
                                "no in-process MCP server named `{server}` is hosted by this session"
                            ),
                        )
                        .await;
                }
            }
        }
        control::InboundControl::HookCallback { .. } => {
            control
                .respond_success(request_id, serde_json::json!({}))
                .await;
        }
        control::InboundControl::Unknown { subtype } => {
            debug!("claude_code: unsupported control request `{subtype}`");
            control
                .respond_error(
                    request_id,
                    &format!("unsupported control request `{subtype}`"),
                )
                .await;
        }
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
#[path = "run_turn_tests.rs"]
mod run_turn_tests;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn headless_claude_uses_auto_for_interactive_codex_policies() {
        // FORK: the mode now depends on the sandbox too. Eight combinations,
        // and the two rules that matter: `acceptEdits` refuses every non-edit
        // request in headless mode, so it is only reachable once Codex itself
        // stopped asking; and `bypassPermissions` suppresses `can_use_tool`, so
        // it must never be paired with the prompt tool.
        let workspace = SandboxPolicy::new_workspace_write_policy();
        let full = SandboxPolicy::DangerFullAccess;
        let external = SandboxPolicy::ExternalSandbox {
            network_access: Default::default(),
        };
        let read_only = SandboxPolicy::new_read_only_policy();

        assert_eq!(
            permission_mode_for(&full, AskForApproval::Never),
            "bypassPermissions"
        );
        assert_eq!(
            permission_mode_for(&external, AskForApproval::Never),
            "bypassPermissions"
        );
        // FORK: not `acceptEdits`. Against the real CLI that mode auto-approves
        // edits and refuses everything else, so the agent keeps `Write` and
        // loses `Bash` — observed as "This command requires approval" on a plain
        // `cat`. And it confines nothing: `--add-dir` decides that.
        assert_eq!(
            permission_mode_for(&workspace, AskForApproval::Never),
            "bypassPermissions"
        );
        assert_eq!(
            permission_mode_for(&workspace, AskForApproval::OnRequest),
            "auto"
        );
        assert_eq!(
            permission_mode_for(&workspace, AskForApproval::UnlessTrusted),
            "auto"
        );
        assert_eq!(
            permission_mode_for(&read_only, AskForApproval::Never),
            "plan"
        );
        assert_eq!(
            permission_mode_for(&read_only, AskForApproval::OnRequest),
            "plan"
        );

        // A leased writable subagent uses the control callback even when the
        // parent itself is configured not to prompt. Root keeps the ordinary
        // `Never` mapping above.
        assert_eq!(
            permission_mode_for_access(&workspace, AskForApproval::Never, true),
            "auto"
        );
        assert_eq!(
            permission_mode_for_access(&workspace, AskForApproval::Never, false),
            "bypassPermissions"
        );
        assert_eq!(
            permission_mode_for_access(&read_only, AskForApproval::Never, true),
            "plan"
        );

        // Only `auto` has a question left to ask.
        assert!(uses_permission_prompt("auto"));
        assert!(!uses_permission_prompt("bypassPermissions"));
        assert!(!uses_permission_prompt("acceptEdits"));
        assert!(!uses_permission_prompt("plan"));
    }

    /// FORK: a read-only child should not even load the tools it is forbidden
    /// to use, and an approving child must be given a prompt surface.
    #[test]
    fn the_command_line_matches_the_permission_mode() {
        let temp = tempfile::tempdir().expect("tempdir");
        let mut workspace = test_workspace(&temp);

        workspace.permission_mode = "plan";
        let args = command_args(&workspace);
        assert!(
            args.windows(2)
                .any(|pair| pair == ["--tools", READ_ONLY_TOOLS])
        );
        assert!(!args.iter().any(|arg| arg == "--permission-prompt-tool"));

        workspace.permission_mode = "auto";
        workspace.host = Some(Arc::new(NoHost));
        let args = command_args(&workspace);
        assert!(
            args.windows(2)
                .any(|pair| pair == ["--permission-prompt-tool", "stdio"])
        );

        // Never in bypass: the CLI suppresses `can_use_tool` there anyway.
        workspace.permission_mode = "bypassPermissions";
        let args = command_args(&workspace);
        assert!(!args.iter().any(|arg| arg == "--permission-prompt-tool"));
    }

    fn command_args(workspace: &ClaudeCodeWorkspace) -> Vec<String> {
        build_claude_command("claude-opus-5", None, None, workspace, None)
            .as_std()
            .get_args()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect()
    }

    /// FORK: discarding the session costs a full transcript replay and the
    /// prompt cache with it, so only a genuine "that session is gone" counts.
    #[test]
    fn help_text_mentioning_resume_does_not_discard_the_session() {
        assert!(session_lost("No conversation found with session ID: abc"));
        assert!(session_lost("Session not found"));
        assert!(session_lost("invalid --resume argument"));
        assert!(session_lost("--resume failed"));

        // The CLI printing its usage, or naming the flag in passing, is not a
        // lost session.
        assert!(!session_lost(
            "Usage: claude [options]\n  --resume <id>  Resume a conversation"
        ));
        assert!(!session_lost("try --resume to continue where you left off"));
        assert!(!session_lost("connection reset by peer"));
    }

    /// A host that is present but answers nothing; enough to prove the flag is
    /// driven by having one, not by what it decides.
    #[derive(Debug)]
    struct NoHost;

    impl host::ClaudeHost for NoHost {
        fn approve_tool<'a>(
            &'a self,
            _request: &'a control::CanUseTool,
        ) -> futures::future::BoxFuture<'a, control::ToolPermissionDecision> {
            Box::pin(async {
                control::ToolPermissionDecision::Deny {
                    message: String::new(),
                    interrupt: false,
                }
            })
        }

        fn call_bridge_tool<'a>(
            &'a self,
            _name: &'a str,
            _arguments: JsonValue,
        ) -> futures::future::BoxFuture<'a, std::result::Result<JsonValue, String>> {
            Box::pin(async { Err(String::new()) })
        }

        fn bridge_tool_specs(&self) -> futures::future::BoxFuture<'_, Vec<JsonValue>> {
            Box::pin(async { Vec::new() })
        }
    }

    fn test_workspace(temp: &tempfile::TempDir) -> ClaudeCodeWorkspace {
        let cwd = codex_utils_absolute_path::AbsolutePathBuf::from_absolute_path(
            temp.path().join("repo"),
        )
        .expect("an absolute temp path");
        ClaudeCodeWorkspace {
            cwd_uri: codex_utils_path_uri::PathUri::from_abs_path(&cwd),
            cwd: cwd.to_path_buf(),
            extra_roots: vec![temp.path().join("sibling"), temp.path().join("repo")],
            permission_mode: "bypassPermissions",
            account_dirs: Vec::new(),
            accounts_state_path: None,
            sessions_state_path: None,
            selection: ClaudeCodeAccountSelection::default(),
            sticky_min_headroom_pct: 20.0,
            pinned_account: None,
            idle_timeout: None,
            transient_retry_delays: TRANSIENT_RETRY_DELAYS,
            claude_command: None,
            developer_instructions: None,
            control_protocol: false,
            stream_partial_messages: false,
            sandbox: SandboxPolicy::new_workspace_write_policy(),
            writable_roots: Vec::new(),
            host: None,
        }
    }

    /// FORK: `plan` is the CLI's read-only mode and it exposes no `Bash` tool.
    /// A child that is not told spends its first calls finding out by failure.
    #[test]
    fn claude_system_prompt_mentions_missing_bash_in_plan_mode() {
        let temp = tempfile::tempdir().expect("tempdir");
        let mut workspace = test_workspace(&temp);
        workspace.permission_mode = "plan";
        workspace.writable_roots = Vec::new();

        let prompt = claude_system_prompt(&workspace);
        assert!(
            prompt.contains("This turn is read-only"),
            "read-only notice missing: {prompt}"
        );
        assert!(
            prompt
                .contains("Bash is unavailable in this read-only session; use Read, Glob and Grep"),
            "missing-Bash notice missing: {prompt}"
        );

        // A writable turn keeps its `Bash`, and must not be told otherwise.
        let writable = test_workspace(&temp);
        assert!(
            !claude_system_prompt(&writable).contains("Bash is unavailable"),
            "a bypassPermissions turn has Bash"
        );
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
