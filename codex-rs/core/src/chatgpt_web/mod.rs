//! FORK: the `chatgpt_web` provider — ChatGPT Pro web (chatgpt.com), driven
//! through the chrome-mcp daemon and a real Chrome tab, as a model backend for
//! Codex threads (`wire_api = "chatgpt_web"`).
//!
//! Layout (see `docs/plans/2026-08-26-chatgpt-web/PLANO.md`):
//! - `driver/`  — talks to the chrome-mcp daemon: tabs, page scripts, the
//!   chatgpt.com backend API, and the send/stop/upload operations.
//! - `history`/`sessions` — which ChatGPT conversation serves a thread and how
//!   much of the Codex history it has seen.
//! - `prompt`/`attachments` — the message typed into the composer and the
//!   images uploaded with it.
//! - `stream` — poll → `ResponseEvent` translation.
//! - `connector/` — the `tools = "connector"` mode (M6+).
//! - this file — `stream()` / `run_turn()` and the per-thread state.

pub(crate) mod attachments;
pub(crate) mod connector;
pub(crate) mod driver;
pub(crate) mod history;
pub(crate) mod prompt;
pub(crate) mod sessions;
pub(crate) mod stream;

pub(crate) use connector::ConnectorBroker;

use crate::chatgpt_web::connector::BeginTurn;
use crate::chatgpt_web::connector::ToolRequest;
use crate::chatgpt_web::connector::client::DaemonSessionBroker;
use crate::chatgpt_web::connector::connector_attach::ConnectorAttach;
use crate::chatgpt_web::connector::contract::CallTarget;
use crate::chatgpt_web::connector::contract::ExecTool;
use crate::chatgpt_web::connector::contract::ToolSummary;
use crate::chatgpt_web::connector::tool_summaries;
use crate::claude_code::assembler::StreamAssembler;
use crate::claude_code::history::item_fingerprint;
use crate::client_common::Prompt;
use crate::client_common::ResponseEvent;
use crate::client_common::ResponseStream;
use crate::config::ChatGptWebSettings;
use codex_config::config_toml::ChatGptWebTools;
use codex_protocol::error::CodexErr;
use codex_protocol::error::Result;
use codex_protocol::models::MessagePhase;
use codex_protocol::models::ResponseItem;
use codex_protocol::openai_models::ModelInfo;
use codex_protocol::openai_models::ReasoningEffort as ReasoningEffortConfig;
use codex_protocol::protocol::SandboxPolicy;
use codex_protocol::protocol::TokenUsage;
use driver::DriverError;
use driver::DriverErrorKind;
use driver::api::Conversation;
use driver::daemon::DaemonClient;
use driver::daemon::DaemonConfig;
use driver::ops::ChatGptOps;
use driver::ops::MentionStrategy;
use driver::ops::ModelSpec;
use driver::ops::SendRequest;
use driver::tabs::TabPool;
use driver::tabs::TabPoolOptions;
use futures::future::BoxFuture;
use history::ConversationContinuity;
use std::collections::HashMap;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Mutex as StdMutex;
use std::sync::OnceLock;
use std::time::Duration;
use tokio::sync::Semaphore;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use tracing::info;
use tracing::warn;

/// File name under `CODEX_HOME` of the durable conversation record.
pub(crate) const SESSIONS_STATE_FILE_NAME: &str = "chatgpt_web_sessions.json";

/// Bound on the event channel between the poll task and the turn loop.
const EVENT_CHANNEL_SIZE: usize = 256;

/// Tokens reserved on top of the character estimate: ChatGPT's own system
/// prompt and the transport header, which the transcript does not show.
const HIDDEN_TOKEN_RESERVE: i64 = 8192;

/// Wait once after a rate limit before giving the turn back to Codex's retry.
const RATE_LIMIT_PAUSE: Duration = Duration::from_secs(30);

/// Budget for best-effort browser actions at the end of a turn (stop, archive).
const CLEANUP_BUDGET: Duration = Duration::from_secs(10);

/// Where and under which rules a `chatgpt_web` turn runs.
///
/// Resolved per turn like `ClaudeCodeWorkspace`: roots and approval settings are
/// materialized by `Session::build_per_turn_config`, so a construction-time
/// config has none of them.
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
    /// The connector broker, when a test injects one; production self-attaches
    /// a process-global broker instead. `None` in `tools = "none"`.
    pub(crate) connector: Option<Arc<dyn ConnectorBroker>>,
    /// The summarization prompt Codex appends on a compaction turn, so the
    /// provider can recognise that turn and answer it from a disposable
    /// conversation.
    pub(crate) compact_prompt: String,
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
            compact_prompt: config
                .compact_prompt
                .clone()
                .unwrap_or_else(|| crate::compact::SUMMARIZATION_PROMPT.to_string()),
        }
    }

    fn fallback() -> Result<Self> {
        let cwd = codex_utils_absolute_path::AbsolutePathBuf::current_dir().map_err(|err| {
            CodexErr::UnsupportedOperation(format!(
                "chatgpt_web provider could not resolve a workspace: {err}"
            ))
        })?;
        Ok(Self {
            cwd: cwd.to_path_buf(),
            extra_roots: Vec::new(),
            writable_roots: Vec::new(),
            sandbox: SandboxPolicy::new_read_only_policy(),
            developer_instructions: None,
            settings: ChatGptWebSettings::default(),
            codex_home: codex_utils_home_dir::find_codex_home()
                .map(|home| home.to_path_buf())
                .unwrap_or_else(|_| cwd.to_path_buf()),
            sessions_state_path: None,
            connector: None,
            compact_prompt: crate::compact::SUMMARIZATION_PROMPT.to_string(),
        })
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

/// Identity of a thread's on-disk continuity record.
#[derive(Debug, Clone)]
struct SessionStore {
    path: PathBuf,
    thread_key: String,
}

/// Cross-turn state of one Codex thread served by ChatGPT Web.
///
/// Lives in the client state (not the per-turn session) so consecutive turns
/// extend the same ChatGPT conversation.
#[derive(Debug, Default)]
pub(crate) struct ChatGptWebThreadState {
    continuity: StdMutex<ConversationContinuity>,
    store: StdMutex<Option<SessionStore>>,
    /// FORK: a connector turn suspended between `stream()` calls, waiting for the
    /// tool outputs Codex is producing. `None` outside connector mode and
    /// between turns.
    live_turn: StdMutex<Option<LiveTurn>>,
}

impl ChatGptWebThreadState {
    fn snapshot(&self) -> ConversationContinuity {
        self.continuity
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    /// Binds this state to its durable record, loading it the first time.
    fn hydrate(&self, path: &Path, thread_key: String) {
        let mut store = self
            .store
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if store.is_some() {
            return;
        }
        *store = Some(SessionStore {
            path: path.to_path_buf(),
            thread_key: thread_key.clone(),
        });
        drop(store);

        if let Some(recorded) = sessions::load(path, &thread_key) {
            let mut continuity = self
                .continuity
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if continuity.conversation_id.is_none() {
                *continuity = recorded;
            }
        }
    }

    fn persist(&self, continuity: &ConversationContinuity) {
        let store = self
            .store
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        if let Some(store) = store {
            sessions::store(&store.path, &store.thread_key, continuity);
        }
    }

    fn record(&self, continuity: ConversationContinuity) {
        {
            let mut current = self
                .continuity
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            *current = continuity.clone();
        }
        self.persist(&continuity);
    }

    /// Forgets the conversation, so the next request replays into a new one.
    fn invalidate(&self) {
        self.record(ConversationContinuity::default());
    }

    fn set_live_turn(&self, live: LiveTurn) {
        *self
            .live_turn
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(live);
    }

    fn take_live_turn(&self) -> Option<LiveTurn> {
        self.live_turn
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take()
    }

    /// The message landed but its reply was not recorded.
    fn mark_unanswered(&self, echoed: Vec<u64>) {
        let snapshot = {
            let mut current = self
                .continuity
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            current.message_landed_unanswered = true;
            if !echoed.is_empty() {
                current.echoed = echoed;
            }
            current.clone()
        };
        self.persist(&snapshot);
    }
}

/// Streams one Codex request through the ChatGPT web app.
pub(crate) async fn stream(
    prompt: &Prompt,
    model_info: &ModelInfo,
    _effort: Option<ReasoningEffortConfig>,
    workspace: Option<&ChatGptWebWorkspace>,
    state: Arc<ChatGptWebThreadState>,
    thread_id: codex_protocol::ThreadId,
) -> Result<ResponseStream> {
    let workspace = match workspace {
        Some(workspace) => workspace.clone(),
        None => ChatGptWebWorkspace::fallback()?,
    };
    if let Some(path) = workspace.sessions_state_path.clone() {
        state.hydrate(&path, thread_id.to_string());
    }

    let input = prompt.input.clone();
    let model_slug = model_info.slug.clone();
    // Reduced ahead of the spawn while `prompt` is borrowable; only the
    // connector turn reads it.
    let connector_tools = tool_summaries(&prompt.tools);
    let (tx_event, rx_event) = mpsc::channel(EVENT_CHANNEL_SIZE);
    let consumer_dropped = CancellationToken::new();
    let consumer_dropped_for_task = consumer_dropped.clone();

    tokio::spawn(async move {
        run_turn(
            input,
            model_slug,
            workspace,
            state,
            connector_tools,
            thread_id,
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

/// The Codex model line → what to ask ChatGPT for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ModelLine {
    pub(crate) spec: ModelSpec,
    pub(crate) label: &'static str,
    pub(crate) is_pro: bool,
}

/// Maps a `chatgpt-web/*` slug onto the ChatGPT model spec and its label.
pub(crate) fn model_line(slug: &str) -> Option<ModelLine> {
    let (spec, label, is_pro) = match slug {
        "chatgpt-web/instant" => (ModelSpec::Instant, "Instant", false),
        "chatgpt-web/thinking" => (ModelSpec::Thinking, "Thinking", false),
        "chatgpt-web/high" => (ModelSpec::High, "Thinking (High)", false),
        "chatgpt-web/extra-high" => (ModelSpec::ExtraHigh, "Thinking (Extra High)", false),
        "chatgpt-web/pro" => (ModelSpec::Pro, "Pro", true),
        _ => return None,
    };
    Some(ModelLine {
        spec,
        label,
        is_pro,
    })
}

/// Turn-slot semaphore: at most `max_parallel_turns` ChatGPT turns in flight
/// per process (three workers already trip "too many requests").
fn turn_slots(max_parallel_turns: usize) -> Arc<Semaphore> {
    static SLOTS: OnceLock<Arc<Semaphore>> = OnceLock::new();
    Arc::clone(SLOTS.get_or_init(|| Arc::new(Semaphore::new(max_parallel_turns.max(1)))))
}

/// One daemon connection + tab pool per (daemon url, chatgpt origin), shared
/// by every thread in the process so tabs are pooled across agents.
struct SharedDriver {
    daemon: Arc<DaemonClient>,
    ops: Arc<ChatGptOps>,
    /// Kept for the connector mode's browser-side attach (mention + approval),
    /// which drives the same pooled tabs as `ops`.
    tabs: Arc<TabPool>,
}

/// How long to wait between `/healthz` probes in [`wait_extension_connected`].
const HEALTH_RETRY_INTERVAL: Duration = Duration::from_millis(500);

/// FORK (verified live): waits for the chrome-mcp bridge to have its Chrome
/// extension connected, up to `timeout`.
///
/// The extension's service worker sleeps and the bridge reconnects on its own —
/// 67s, once observed — so failing a turn on the first sighting of a
/// disconnected extension killed a consultant agent 19s in while the budget
/// was barely touched. `Ok(false)` = the daemon answered but the extension
/// never came back; `Err` = the daemon itself stayed unreachable.
async fn wait_extension_connected(
    driver: &SharedDriver,
    timeout: Duration,
) -> driver::DriverResult<bool> {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        let last_error = match driver.daemon.health().await {
            Ok(health) if health.ok && health.extension_connected => return Ok(true),
            Ok(_) => None,
            Err(err) => {
                warn!("chatgpt_web: chrome-mcp health check failed: {err}");
                Some(err)
            }
        };
        if tokio::time::Instant::now() >= deadline {
            return match last_error {
                Some(err) => Err(err),
                None => Ok(false),
            };
        }
        tokio::time::sleep(HEALTH_RETRY_INTERVAL).await;
    }
}

/// FORK: the verdict after [`wait_extension_connected`] gave up.
///
/// It already spent the whole `connector_ready_timeout` on this, so the answer
/// is final and the error must not be retryable: a `Stream` error sends the
/// turn loop through five more reconnects, each waiting the budget again — nine
/// minutes of nothing for a browser that is simply closed. One wait, one
/// verdict; `connector_ready_timeout` is the knob for how long that wait is.
fn chrome_mcp_unready(waited: Duration, error: Option<&driver::DriverError>) -> CodexErr {
    let seconds = waited.as_secs();
    CodexErr::UnsupportedOperation(match error {
        Some(error) => format!("the chrome-mcp daemon stayed unreachable for {seconds}s: {error}"),
        None => format!(
            "chrome-mcp is up but its Chrome extension did not connect within {seconds}s; open Chrome with the 'Chrome MCP Bridge' extension loaded"
        ),
    })
}

fn shared_driver(settings: &ChatGptWebSettings, codex_home: &Path) -> Arc<SharedDriver> {
    static DRIVERS: OnceLock<StdMutex<HashMap<String, Arc<SharedDriver>>>> = OnceLock::new();
    let key = format!("{}|{}", settings.daemon_url, settings.base_url);
    let mut drivers = DRIVERS
        .get_or_init(|| StdMutex::new(HashMap::new()))
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if let Some(driver) = drivers.get(&key) {
        return Arc::clone(driver);
    }
    // The daemon is loopback: system proxies are wrong for it, hence the
    // plain factory rather than the session's proxy-aware one.
    let http_client: Arc<dyn codex_exec_server::HttpClient> = Arc::new(
        codex_exec_server::RouteAwareHttpClient::new(codex_http_client::HttpClientFactory::new(
            codex_http_client::OutboundProxyPolicy::ReqwestDefault,
        ))
        .with_tls_backend_fallback(),
    );
    let config = DaemonConfig::resolve(&settings.daemon_url, settings.token_file.as_deref());
    let daemon = Arc::new(DaemonClient::new(config, http_client));
    let tabs = Arc::new(TabPool::new(
        Arc::clone(&daemon),
        TabPoolOptions {
            max_tabs: settings.max_tabs,
            idle_ms: settings.tab_idle.as_millis() as u64,
            registry_path: driver::tabs::default_registry_path()
                .unwrap_or_else(|| codex_home.join("chatgpt_web").join("tabs.json")),
            base_url: settings.base_url.trim_end_matches('/').to_string(),
        },
    ));
    let ops = Arc::new(ChatGptOps::new(
        Arc::clone(&daemon),
        Arc::clone(&tabs),
        settings.base_url.clone(),
    ));
    let driver = Arc::new(SharedDriver { daemon, ops, tabs });
    drivers.insert(key, Arc::clone(&driver));
    driver
}

/// The poll loop reads conversations through the shared driver.
struct OpsSource<'a>(&'a ChatGptOps);

impl stream::ConversationSource for OpsSource<'_> {
    fn read<'a>(
        &'a self,
        conversation_id: &'a str,
    ) -> BoxFuture<'a, driver::DriverResult<Conversation>> {
        Box::pin(async move { self.0.read_conversation(conversation_id, true).await })
    }
}

impl stream::DomSource for OpsSource<'_> {
    fn read_dom<'a>(
        &'a self,
        conversation_id: &'a str,
    ) -> BoxFuture<'a, driver::DriverResult<Option<driver::page_scripts::DomProgress>>> {
        Box::pin(async move { self.0.dom_progress(conversation_id).await })
    }
}

/// Maps a driver failure onto the Codex error that decides retry vs. stop.
fn map_driver_error(err: &DriverError) -> CodexErr {
    let phase = err
        .phase
        .map(|phase| format!(" (phase: {phase})"))
        .unwrap_or_default();
    match err.kind {
        DriverErrorKind::DaemonDown => CodexErr::Stream(format!(
            "chrome-mcp daemon unreachable: {}. Start it (chrome-mcp) and make sure the extension is connected.",
            err.message
        )),
        DriverErrorKind::LoginRequired => CodexErr::UnsupportedOperation(format!(
            "chatgpt.com is not logged in in the driven Chrome: {}",
            err.message
        )),
        DriverErrorKind::RateLimited => {
            CodexErr::Stream(format!("ChatGPT rate-limited the request: {}", err.message))
        }
        DriverErrorKind::MessageTooLong => CodexErr::ContextWindowExceeded,
        DriverErrorKind::UiChanged => CodexErr::UnsupportedOperation(format!(
            "chatgpt.com UI changed under the driver{phase}: {}",
            err.message
        )),
        DriverErrorKind::SubmitAmbiguous => CodexErr::UnsupportedOperation(format!(
            "could not confirm whether the message reached ChatGPT{phase}; not resending it blindly: {}",
            err.message
        )),
        DriverErrorKind::Upstream
        | DriverErrorKind::ConversationNotFound
        | DriverErrorKind::Busy
        | DriverErrorKind::Tool
        | DriverErrorKind::Timeout
        | DriverErrorKind::Other => {
            CodexErr::Stream(format!("chatgpt_web{phase}: {}", err.message))
        }
    }
}

/// `chars/4` plus the hidden reserve, so Codex's context meter tracks the
/// size of the ChatGPT conversation and triggers compaction in time.
fn estimate_usage(transcript_chars: usize, reply_chars: usize) -> TokenUsage {
    let input_tokens = transcript_chars.div_ceil(4) as i64 + HIDDEN_TOKEN_RESERVE;
    let output_tokens = reply_chars.div_ceil(4) as i64;
    TokenUsage {
        input_tokens,
        cached_input_tokens: 0,
        cache_write_input_tokens: 0,
        output_tokens,
        reasoning_output_tokens: 0,
        total_tokens: input_tokens + output_tokens,
        codex_rollout_budget_units: None,
    }
}

#[allow(clippy::type_complexity, clippy::too_many_arguments)]
async fn run_turn(
    input: Vec<ResponseItem>,
    model_slug: String,
    workspace: ChatGptWebWorkspace,
    state: Arc<ChatGptWebThreadState>,
    connector_tools: (Vec<ToolSummary>, ExecTool, bool),
    thread_id: codex_protocol::ThreadId,
    tx_event: mpsc::Sender<Result<ResponseEvent>>,
    consumer_dropped: CancellationToken,
) {
    if tx_event.send(Ok(ResponseEvent::Created {
        guardian_ticket: None,
    })).await.is_err() {
        return;
    }
    let fail = |err: CodexErr| {
        let tx = tx_event.clone();
        async move {
            let _ = tx.send(Err(err)).await;
        }
    };

    let Some(line) = model_line(&model_slug) else {
        fail(CodexErr::UnsupportedOperation(format!(
            "model `{model_slug}` is not a ChatGPT Web model line (expected chatgpt-web/instant|thinking|high|extra-high|pro)"
        )))
        .await;
        return;
    };
    let settings = workspace.settings.clone();
    if settings.tools == ChatGptWebTools::Connector {
        run_connector_turn(
            input,
            model_slug,
            workspace,
            state,
            connector_tools,
            thread_id,
            line,
            tx_event,
            consumer_dropped,
        )
        .await;
        return;
    }

    // One slot per turn: more parallel ChatGPT turns than this trip its
    // rate limiting.
    let slots = turn_slots(settings.max_parallel_turns);
    let _permit = tokio::select! {
        _ = consumer_dropped.cancelled() => return,
        permit = slots.acquire_owned() => match permit {
            Ok(permit) => permit,
            Err(_) => return,
        },
    };

    let continuity = state.snapshot();
    let plan = history::plan_request(
        &input,
        &continuity,
        &model_slug,
        Some(workspace.compact_prompt.as_str()),
    );
    let image_store = attachments::ImageStore::new(&workspace.codex_home);
    image_store.cleanup_stale();
    let rendered = prompt::render(prompt::RenderRequest {
        plan: &plan,
        workspace: &workspace,
        mode: if plan.is_compaction {
            prompt::PromptMode::Compaction
        } else {
            prompt::PromptMode::None
        },
        is_pro: line.is_pro,
        resume_after_interrupt: continuity.message_landed_unanswered && !plan.restart,
        images: Some(&image_store),
    });
    let transcript_chars = prompt::transcript_chars(&input);

    let mut assembler = StreamAssembler::new(&tx_event);
    if !plan.is_compaction
        && !assembler
            .emit_message(prompt::warning_text(line.label), MessagePhase::Commentary)
            .await
    {
        return;
    }

    let driver = shared_driver(&settings, &workspace.codex_home);
    match wait_extension_connected(&driver, settings.connector_ready_timeout).await {
        Ok(true) => {}
        Ok(false) => {
            fail(chrome_mcp_unready(settings.connector_ready_timeout, None)).await;
            return;
        }
        Err(err) => {
            fail(chrome_mcp_unready(
                settings.connector_ready_timeout,
                Some(&err),
            ))
            .await;
            return;
        }
    }

    let request = SendRequest {
        conversation_id: if plan.restart {
            None
        } else {
            continuity.conversation_id.clone()
        },
        text: rendered.text.clone(),
        model: if plan.restart {
            Some(line.spec.clone())
        } else {
            None
        },
        files: rendered.attachments.clone(),
        mention: None,
        mention_strategy: MentionStrategy::default(),
    };
    info!(
        "chatgpt_web: sending {} chars ({}; {} attachments) to {}",
        rendered.text.chars().count(),
        if plan.restart {
            "new conversation"
        } else {
            "extension"
        },
        rendered.attachments.len(),
        request.conversation_id.as_deref().unwrap_or("<new>")
    );
    let sent_at = tokio::time::Instant::now();
    let mut rate_limited_once = false;
    let sent = loop {
        match driver.ops.send(request.clone()).await {
            Ok(sent) => break sent,
            Err(err) if err.kind == DriverErrorKind::RateLimited && !rate_limited_once => {
                rate_limited_once = true;
                warn!("chatgpt_web: rate limited; pausing {RATE_LIMIT_PAUSE:?} before one retry");
                tokio::select! {
                    _ = consumer_dropped.cancelled() => return,
                    _ = tokio::time::sleep(RATE_LIMIT_PAUSE) => {}
                }
            }
            Err(err) => {
                match err.kind {
                    DriverErrorKind::MessageTooLong | DriverErrorKind::ConversationNotFound => {
                        state.invalidate();
                    }
                    _ if err.message_landed != Some(false) && !plan.restart => {
                        // Landed or unknown: the next extension must say so.
                        state.mark_unanswered(assembler.take_authored());
                    }
                    _ => {}
                }
                fail(map_driver_error(&err)).await;
                return;
            }
        }
    };
    let conversation_id = sent.conversation_id.clone();
    for note in &sent.notes {
        info!("chatgpt_web: {note}");
    }

    // Recorded before polling: the message is in the conversation whatever
    // happens next, so a failed poll must extend, not replay.
    if !plan.is_compaction {
        state.record(ConversationContinuity {
            conversation_id: Some(conversation_id.clone()),
            model_slug: Some(model_slug.clone()),
            delivered_items: plan.delivered_items,
            delivered_fingerprint: plan.delivered_fingerprint,
            echoed: continuity.echoed.clone(),
            message_landed_unanswered: true,
        });
    }

    let source = OpsSource(&driver.ops);
    let outcome = stream::PollLoop {
        source: &source,
        conversation_id: conversation_id.clone(),
        tracker: stream::ReplyTracker::new(&rendered.text),
        mode: stream::TrackMode::None,
        poll_interval: settings.poll_interval,
        idle_timeout: settings.idle_timeout,
        sent_at,
        dom: Some(&source),
        anchor: rendered.text.clone(),
    }
    .run(&mut assembler, &consumer_dropped)
    .await;

    let authored = assembler.take_authored();
    match outcome {
        stream::PollOutcome::Done { reason, text_chars } => {
            info!("chatgpt_web: reply complete ({reason:?}, {text_chars} chars)");
            if plan.is_compaction {
                archive_conversation(&driver.ops, &conversation_id).await;
            } else {
                state.record(ConversationContinuity {
                    conversation_id: Some(conversation_id.clone()),
                    model_slug: Some(model_slug),
                    delivered_items: plan.delivered_items,
                    delivered_fingerprint: plan.delivered_fingerprint,
                    echoed: authored,
                    message_landed_unanswered: false,
                });
            }
            let _ = tx_event
                .send(Ok(ResponseEvent::Completed {
                    response_id: conversation_id,
                    token_usage: Some(estimate_usage(transcript_chars, text_chars)),
                    usage_metadata: None,
                    end_turn: Some(true),
                }))
                .await;
        }
        stream::PollOutcome::Interrupted => {
            stop_generation(&driver.ops, &conversation_id).await;
            if !plan.is_compaction {
                state.mark_unanswered(authored);
            }
        }
        stream::PollOutcome::Stalled { seconds } => {
            stop_generation(&driver.ops, &conversation_id).await;
            if !plan.is_compaction {
                state.mark_unanswered(authored);
            }
            fail(CodexErr::UnsupportedOperation(format!(
                "chatgpt_web: no progress for {seconds}s; generation stopped"
            )))
            .await;
        }
        stream::PollOutcome::PartialCompletion => {
            if !plan.is_compaction {
                state.mark_unanswered(authored);
            }
            fail(CodexErr::Stream(
                "ChatGPT stopped before finishing the answer (partial completion)".to_string(),
            ))
            .await;
        }
        stream::PollOutcome::Failed(err) => {
            if err.kind == DriverErrorKind::ConversationNotFound {
                state.invalidate();
            } else if !plan.is_compaction {
                state.mark_unanswered(authored);
            }
            fail(map_driver_error(&err)).await;
        }
    }
}

// ---------------------------------------------------------------------------
// FORK: connector mode (`tools = "connector"`) — the session-side turn driver.
// ---------------------------------------------------------------------------

/// One tool call ChatGPT made that Codex is running; its `respond` is fulfilled
/// on the next `stream()` once the `FunctionCallOutput` appears.
#[derive(Debug)]
struct PendingCall {
    call_id: String,
    respond: tokio::sync::oneshot::Sender<codex_protocol::models::FunctionCallOutputPayload>,
}

/// A connector turn suspended between `stream()` calls, waiting for tool
/// outputs. Everything needed to resume the poll after the outputs land.
#[derive(Debug)]
struct LiveTurn {
    conversation_id: String,
    connector_name: String,
    turn_token: String,
    requests: mpsc::Receiver<ToolRequest>,
    pending: Vec<PendingCall>,
    tracker: stream::ReplyTracker,
    echoed: Vec<u64>,
    model_slug: String,
    delivered_items: usize,
    delivered_fingerprint: u64,
    transcript_chars: usize,
    sent_at: tokio::time::Instant,
    broker: Arc<dyn ConnectorBroker>,
}

/// Process-global connector broker, one per `CODEX_HOME`, so every thread in the
/// process shares a single daemon session (`session_id`).
async fn connector_broker(
    codex_home: &Path,
    settings: &ChatGptWebSettings,
) -> std::result::Result<Arc<dyn ConnectorBroker>, String> {
    #[allow(clippy::type_complexity)]
    static CELLS: OnceLock<
        StdMutex<HashMap<String, Arc<tokio::sync::OnceCell<Arc<dyn ConnectorBroker>>>>>,
    > = OnceLock::new();
    let key = codex_home.to_string_lossy().to_string();
    let cell = {
        let mut map = CELLS
            .get_or_init(|| StdMutex::new(HashMap::new()))
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        Arc::clone(
            map.entry(key)
                .or_insert_with(|| Arc::new(tokio::sync::OnceCell::new())),
        )
    };
    let connector_name = settings.connector_name.clone();
    let overrides = connector::daemon::daemon_overrides(settings);
    let codex_home = codex_home.to_path_buf();
    let broker = cell
        .get_or_try_init(|| async move {
            let endpoint = connector::daemon::ensure_daemon(&codex_home, &overrides)
                .await
                .map_err(|err| err.to_string())?;
            let broker =
                DaemonSessionBroker::connect(endpoint.control_url, endpoint.token, connector_name)
                    .await?;
            Ok::<Arc<dyn ConnectorBroker>, String>(Arc::new(broker))
        })
        .await?;
    Ok(Arc::clone(broker))
}

/// Maps a connector call target onto the Codex item the turn loop dispatches.
fn target_to_item(target: &CallTarget, call_id: &str) -> ResponseItem {
    match target {
        CallTarget::Function {
            namespace,
            name,
            arguments,
        } => ResponseItem::FunctionCall {
            id: None,
            name: name.clone(),
            namespace: namespace.clone(),
            arguments: arguments.to_string(),
            // Calls delivered by the local connector are already plaintext.
            // Preserve the explicit empty marker so the normal tool router
            // does not mistake a connector's `message` argument for backend
            // ciphertext.
            encrypted_function_args: Some(Vec::new()),
            call_id: call_id.to_string(),
            internal_chat_message_metadata_passthrough: None,
        },
        CallTarget::Custom { name, input } => ResponseItem::CustomToolCall {
            id: None,
            status: None,
            call_id: call_id.to_string(),
            name: name.clone(),
            namespace: None,
            input: input.clone(),
            internal_chat_message_metadata_passthrough: None,
        },
    }
}

/// Finds the output Codex produced for one connector call, from the tail of the
/// next request's input.
fn extract_output(
    input: &[ResponseItem],
    call_id: &str,
) -> Option<codex_protocol::models::FunctionCallOutputPayload> {
    input.iter().rev().find_map(|item| match item {
        ResponseItem::FunctionCallOutput {
            call_id: Some(id),
            output,
            ..
        } if id == call_id => Some(output.clone()),
        ResponseItem::CustomToolCallOutput {
            call_id: id,
            output,
            ..
        } if id == call_id => Some(output.clone()),
        _ => None,
    })
}

/// Collects the tool calls arriving within a ~15ms window into one batch, so the
/// items are emitted together (the daemon already batches, but a burst can span
/// two channel sends).
async fn collect_batch(
    first: ToolRequest,
    rx: &mut mpsc::Receiver<ToolRequest>,
) -> Vec<ToolRequest> {
    let mut batch = vec![first];
    while batch.len() < 16 {
        match tokio::time::timeout(Duration::from_millis(15), rx.recv()).await {
            Ok(Some(request)) => batch.push(request),
            Ok(None) | Err(_) => break,
        }
    }
    batch
}

/// Drives one `tools = "connector"` turn: reattach a suspended turn if Codex
/// just produced its tool outputs, otherwise open a fresh one.
#[allow(clippy::too_many_arguments)]
async fn run_connector_turn(
    input: Vec<ResponseItem>,
    model_slug: String,
    workspace: ChatGptWebWorkspace,
    state: Arc<ChatGptWebThreadState>,
    connector_tools: (Vec<ToolSummary>, ExecTool, bool),
    thread_id: codex_protocol::ThreadId,
    line: ModelLine,
    tx_event: mpsc::Sender<Result<ResponseEvent>>,
    consumer_dropped: CancellationToken,
) {
    let settings = workspace.settings.clone();
    let driver = shared_driver(&settings, &workspace.codex_home);
    let fail = |err: CodexErr| {
        let tx = tx_event.clone();
        async move {
            let _ = tx.send(Err(err)).await;
        }
    };

    // Reattach: does a suspended turn's every pending call now have its output?
    if let Some(mut live) = state.take_live_turn() {
        let ready = live
            .pending
            .iter()
            .all(|pending| extract_output(&input, &pending.call_id).is_some());
        if ready {
            for pending in std::mem::take(&mut live.pending) {
                if let Some(payload) = extract_output(&input, &pending.call_id) {
                    let _ = pending.respond.send(payload);
                }
            }
            connector_loop(live, driver, state, tx_event, consumer_dropped, &settings).await;
            return;
        }
        // The user redirected the turn (the outputs never came): abort it and
        // fall through to a fresh turn below.
        live.broker
            .end_turn(&live.turn_token, "the Codex turn was replaced")
            .await;
        stop_generation(&driver.ops, &live.conversation_id).await;
    }

    // Fresh connector turn.
    let slots = turn_slots(settings.max_parallel_turns);
    let _permit = tokio::select! {
        _ = consumer_dropped.cancelled() => return,
        permit = slots.acquire_owned() => match permit {
            Ok(permit) => permit,
            Err(_) => return,
        },
    };

    let continuity = state.snapshot();
    let plan = history::plan_request(
        &input,
        &continuity,
        &model_slug,
        Some(workspace.compact_prompt.as_str()),
    );
    let image_store = attachments::ImageStore::new(&workspace.codex_home);
    image_store.cleanup_stale();
    let transcript_chars = prompt::transcript_chars(&input);

    // A compaction turn never runs tools: answer it from a disposable
    // conversation like the `tools = "none"` path, with no connector.
    if plan.is_compaction {
        run_compaction_turn(
            &plan,
            &workspace,
            &driver,
            &line,
            &image_store,
            transcript_chars,
            &tx_event,
            &consumer_dropped,
        )
        .await;
        return;
    }

    let broker = match workspace.connector.clone() {
        Some(broker) => broker,
        None => match connector_broker(&workspace.codex_home, &settings).await {
            Ok(broker) => broker,
            Err(reason) => {
                fail(CodexErr::UnsupportedOperation(format!(
                    "[chatgpt_web] tools = \"connector\" needs the connector daemon: {reason}"
                )))
                .await;
                return;
            }
        },
    };

    let (tools, exec_tool, apply_patch) = connector_tools;
    let turn_id = connector::daemon::state::new_token();
    let begin = BeginTurn {
        thread_id,
        turn_id: &turn_id,
        tools,
        exec_tool,
        apply_patch,
        ttl_ms: settings.turn_ttl.as_millis() as u64,
        ready_timeout: settings.connector_ready_timeout,
    };
    let connector_turn = match broker.begin_turn(begin).await {
        Ok(turn) => turn,
        Err(reason) => {
            fail(CodexErr::UnsupportedOperation(format!(
                "the ChatGPT connector could not start the turn: {reason}"
            )))
            .await;
            return;
        }
    };
    let contract = broker.prompt_contract(&connector_turn);
    let connector_name = connector_turn.connector_name.clone();
    let turn_token = connector_turn.turn_token.clone();
    let requests = connector_turn.requests;

    let rendered = prompt::render(prompt::RenderRequest {
        plan: &plan,
        workspace: &workspace,
        mode: prompt::PromptMode::Connector(contract),
        is_pro: line.is_pro,
        resume_after_interrupt: continuity.message_landed_unanswered && !plan.restart,
        images: Some(&image_store),
    });

    match wait_extension_connected(&driver, settings.connector_ready_timeout).await {
        Ok(true) => {}
        Ok(false) => {
            broker
                .end_turn(&turn_token, "chrome-mcp extension not connected")
                .await;
            fail(chrome_mcp_unready(settings.connector_ready_timeout, None)).await;
            return;
        }
        Err(err) => {
            broker.end_turn(&turn_token, "chrome-mcp unreachable").await;
            fail(chrome_mcp_unready(
                settings.connector_ready_timeout,
                Some(&err),
            ))
            .await;
            return;
        }
    }

    // Mention the connector only when the conversation is new; selection is
    // sticky per conversation (spike S4), so a follow-up needs no pill.
    let mention = plan.restart.then(|| connector_name.clone());
    let request = SendRequest {
        conversation_id: if plan.restart {
            None
        } else {
            continuity.conversation_id.clone()
        },
        text: rendered.text.clone(),
        model: if plan.restart {
            Some(line.spec.clone())
        } else {
            None
        },
        files: rendered.attachments.clone(),
        mention,
        mention_strategy: match settings.connector_mention_strategy {
            codex_config::config_toml::ChatGptWebMentionStrategy::Auto => MentionStrategy::Auto,
            codex_config::config_toml::ChatGptWebMentionStrategy::BackgroundOnly => {
                MentionStrategy::BackgroundOnly
            }
            codex_config::config_toml::ChatGptWebMentionStrategy::Activate => {
                MentionStrategy::Activate
            }
        },
    };
    info!(
        "chatgpt_web connector: sending {} chars ({}) with connector \"{connector_name}\"",
        rendered.text.chars().count(),
        if plan.restart {
            "new conversation"
        } else {
            "extension"
        }
    );
    let sent_at = tokio::time::Instant::now();
    let sent = match driver.ops.send(request).await {
        Ok(sent) => sent,
        Err(err) => {
            broker.end_turn(&turn_token, "the send failed").await;
            if err.kind == DriverErrorKind::MessageTooLong
                || err.kind == DriverErrorKind::ConversationNotFound
            {
                state.invalidate();
            } else if err.message_landed != Some(false) {
                state.mark_unanswered(Vec::new());
            }
            fail(map_driver_error(&err)).await;
            return;
        }
    };
    let conversation_id = sent.conversation_id.clone();
    for note in &sent.notes {
        info!("chatgpt_web connector: {note}");
    }
    // The message is in the conversation; a failed poll must extend it.
    state.record(ConversationContinuity {
        conversation_id: Some(conversation_id.clone()),
        model_slug: Some(model_slug.clone()),
        delivered_items: plan.delivered_items,
        delivered_fingerprint: plan.delivered_fingerprint,
        echoed: continuity.echoed.clone(),
        message_landed_unanswered: true,
    });

    let live = LiveTurn {
        conversation_id,
        connector_name,
        turn_token,
        requests,
        pending: Vec::new(),
        tracker: stream::ReplyTracker::new(&rendered.text),
        echoed: Vec::new(),
        model_slug,
        delivered_items: plan.delivered_items,
        delivered_fingerprint: plan.delivered_fingerprint,
        transcript_chars,
        sent_at,
        broker,
    };
    connector_loop(live, driver, state, tx_event, consumer_dropped, &settings).await;
}

/// The connector poll: streams the reply, parks when ChatGPT calls tools, and
/// records the outcome. Owns `live`; on a tool batch it moves `live` into the
/// thread state and returns so the next `stream()` resumes it.
async fn connector_loop(
    mut live: LiveTurn,
    driver: Arc<SharedDriver>,
    state: Arc<ChatGptWebThreadState>,
    tx_event: mpsc::Sender<Result<ResponseEvent>>,
    consumer_dropped: CancellationToken,
    settings: &ChatGptWebSettings,
) {
    let mut assembler = StreamAssembler::new(&tx_event);
    live.tracker.reset_open();
    let poll_interval = settings.poll_interval;
    let idle_timeout = settings.idle_timeout;
    let auto_always = settings.connector_auto_approve_ui;
    let mut last_progress = tokio::time::Instant::now();
    let mut requests_open = true;
    let mut polls: u32 = 0;
    let mut read_failures: u32 = 0;
    let mut rate_limit_cooldown = stream::RATE_LIMIT_COOLDOWN_MIN;
    let mut last_rate_limit: Option<tokio::time::Instant> = None;
    // FORK: the page is the cheap source of progress; the API is read only
    // when the scheduler says so (see `stream::PollScheduler`).
    let mut scheduler = stream::PollScheduler::new(live.sent_at);
    let anchor = live.tracker.anchor().to_string();

    loop {
        let stall_deadline = idle_timeout
            .map(|timeout| last_progress + timeout)
            .unwrap_or_else(|| tokio::time::Instant::now() + Duration::from_secs(365 * 24 * 3600));
        tokio::select! {
            biased;
            _ = consumer_dropped.cancelled() => {
                live.broker.end_turn(&live.turn_token, "the Codex turn was interrupted").await;
                stop_generation(&driver.ops, &live.conversation_id).await;
                state.mark_unanswered(live.echoed);
                return;
            }
            maybe = live.requests.recv(), if requests_open => {
                let Some(first) = maybe else {
                    // The daemon retired the turn out from under us.
                    requests_open = false;
                    continue;
                };
                let batch = collect_batch(first, &mut live.requests).await;
                if !assembler.close(MessagePhase::Commentary).await {
                    return;
                }
                let mut pending = Vec::new();
                for request in batch {
                    let item = target_to_item(&request.target, &request.call_id);
                    live.echoed.push(item_fingerprint(&item));
                    if !assembler.send(ResponseEvent::OutputItemAdded(item.clone())).await
                        || !assembler.send(ResponseEvent::OutputItemDone(item)).await
                    {
                        return;
                    }
                    pending.push(PendingCall {
                        call_id: request.call_id,
                        respond: request.respond,
                    });
                }
                let usage = estimate_usage(live.transcript_chars, live.tracker.text_chars());
                let _ = assembler
                    .send(ResponseEvent::Completed {
                        response_id: live.conversation_id.clone(),
                        token_usage: Some(usage),
                        usage_metadata: None,
                        end_turn: Some(false),
                    })
                    .await;
                live.pending = pending;
                state.set_live_turn(live);
                return;
            }
            _ = tokio::time::sleep(stream::effective_poll_interval(poll_interval, last_rate_limit)) => {
                polls = polls.wrapping_add(1);
                if polls.is_multiple_of(2) {
                    let attach = ConnectorAttach {
                        daemon: &driver.daemon,
                        tabs: &driver.tabs,
                        connector_name: live.connector_name.clone(),
                        auto_always,
                    };
                    if attach.approve_on_conversation(&live.conversation_id).await {
                        info!("chatgpt_web connector: approved a tool card");
                        last_progress = tokio::time::Instant::now();
                    }
                }
                let progress = match driver.ops.dom_progress(&live.conversation_id).await {
                    Ok(progress) => progress,
                    Err(err) => {
                        tracing::debug!("chatgpt_web connector: DOM progress read failed: {err}");
                        None
                    }
                };
                let step = scheduler.on_dom(progress, &anchor, tokio::time::Instant::now());
                if step.changed {
                    last_progress = tokio::time::Instant::now();
                }
                if !step.read_api {
                    continue;
                }
                scheduler.on_api_read(tokio::time::Instant::now());
                let conv = match driver.ops.read_conversation(&live.conversation_id, true).await {
                    Ok(conv) => {
                        read_failures = 0;
                        rate_limit_cooldown = stream::RATE_LIMIT_COOLDOWN_MIN;
                        conv
                    }
                    // FORK: see `stream::RATE_LIMIT_COOLDOWN_MIN` — polling
                    // through a 429 only keeps the account rate limited.
                    Err(err) if err.kind == DriverErrorKind::RateLimited => {
                        last_rate_limit = Some(tokio::time::Instant::now());
                        warn!(
                            "chatgpt_web connector: conversation reads are rate limited; backing off {}s: {err}",
                            rate_limit_cooldown.as_secs()
                        );
                        tokio::select! {
                            biased;
                            _ = consumer_dropped.cancelled() => {
                                live.broker.end_turn(&live.turn_token, "the Codex turn was interrupted").await;
                                stop_generation(&driver.ops, &live.conversation_id).await;
                                state.mark_unanswered(live.echoed);
                                return;
                            }
                            _ = tokio::time::sleep(rate_limit_cooldown) => {}
                        }
                        rate_limit_cooldown = stream::next_rate_limit_cooldown(rate_limit_cooldown);
                        continue;
                    }
                    Err(err)
                        if err.kind == DriverErrorKind::ConversationNotFound
                            && live.sent_at.elapsed() <= Duration::from_secs(30) =>
                    {
                        continue;
                    }
                    // FORK: a transient read failure (an eval timeout in a
                    // throttled hidden tab, a 5xx) must not end a connector
                    // turn — ending it retires the turn_token ChatGPT is still
                    // using, and the retry then starts over. Tolerate a run of
                    // failures like the `tools = "none"` poll loop does.
                    Err(err)
                        if !matches!(
                            err.kind,
                            DriverErrorKind::ConversationNotFound | DriverErrorKind::LoginRequired
                        ) && read_failures + 1 < stream::MAX_CONSECUTIVE_READ_FAILURES =>
                    {
                        read_failures += 1;
                        warn!(
                            "chatgpt_web connector: reading conversation failed ({read_failures}): {err}"
                        );
                        continue;
                    }
                    Err(err) => {
                        live.broker.end_turn(&live.turn_token, "the conversation could not be read").await;
                        if err.kind == DriverErrorKind::ConversationNotFound {
                            state.invalidate();
                        } else {
                            state.mark_unanswered(live.echoed.clone());
                        }
                        let _ = tx_event.send(Err(map_driver_error(&err))).await;
                        return;
                    }
                };
                let deltas = live.tracker.observe_at(
                    &conv,
                    stream::TrackMode::Connector,
                    step.hint,
                    tokio::time::Instant::now(),
                );
                for delta in deltas {
                    match stream::apply_delta(&mut assembler, delta, &mut last_progress).await {
                        stream::DeltaStep::Continue => {}
                        stream::DeltaStep::Interrupted => {
                            live.broker.end_turn(&live.turn_token, "interrupted").await;
                            state.mark_unanswered(live.echoed);
                            return;
                        }
                        stream::DeltaStep::Partial => {
                            live.broker.end_turn(&live.turn_token, "partial completion").await;
                            state.mark_unanswered(live.echoed.clone());
                            let _ = tx_event
                                .send(Err(CodexErr::Stream(
                                    "ChatGPT stopped before finishing the answer (partial completion)".to_string(),
                                )))
                                .await;
                            return;
                        }
                        stream::DeltaStep::Done(reason) => {
                            let text_chars = live.tracker.text_chars();
                            live.broker.end_turn(&live.turn_token, "the Codex turn finished").await;
                            state.record(ConversationContinuity {
                                conversation_id: Some(live.conversation_id.clone()),
                                model_slug: Some(live.model_slug.clone()),
                                delivered_items: live.delivered_items,
                                delivered_fingerprint: live.delivered_fingerprint,
                                echoed: live.echoed.clone(),
                                message_landed_unanswered: false,
                            });
                            let _ = tx_event
                                .send(Ok(ResponseEvent::Completed {
                                    response_id: live.conversation_id.clone(),
                                    token_usage: Some(estimate_usage(live.transcript_chars, text_chars)),
                                    usage_metadata: None,
                                    end_turn: Some(true),
                                }))
                                .await;
                            info!("chatgpt_web connector: reply complete ({reason:?}, {text_chars} chars)");
                            return;
                        }
                    }
                }
            }
            _ = tokio::time::sleep_until(stall_deadline) => {
                let seconds = idle_timeout.map(|timeout| timeout.as_secs()).unwrap_or_default();
                live.broker.end_turn(&live.turn_token, "no progress").await;
                stop_generation(&driver.ops, &live.conversation_id).await;
                state.mark_unanswered(live.echoed);
                let _ = tx_event
                    .send(Err(CodexErr::UnsupportedOperation(format!(
                        "chatgpt_web connector: no progress for {seconds}s; generation stopped"
                    ))))
                    .await;
                return;
            }
        }
    }
}

/// A compaction turn in connector mode: rendered with the checkpoint contract,
/// answered from a disposable conversation with tools off, then archived.
#[allow(clippy::too_many_arguments)]
async fn run_compaction_turn(
    plan: &history::RequestPlan<'_>,
    workspace: &ChatGptWebWorkspace,
    driver: &Arc<SharedDriver>,
    line: &ModelLine,
    image_store: &attachments::ImageStore,
    transcript_chars: usize,
    tx_event: &mpsc::Sender<Result<ResponseEvent>>,
    consumer_dropped: &CancellationToken,
) {
    let fail = |err: CodexErr| {
        let tx = tx_event.clone();
        async move {
            let _ = tx.send(Err(err)).await;
        }
    };
    let rendered = prompt::render(prompt::RenderRequest {
        plan,
        workspace,
        mode: prompt::PromptMode::Compaction,
        is_pro: line.is_pro,
        resume_after_interrupt: false,
        images: Some(image_store),
    });
    let mut assembler = StreamAssembler::new(tx_event);
    if let Err(err) = driver.daemon.health().await {
        fail(map_driver_error(&err)).await;
        return;
    }
    let sent_at = tokio::time::Instant::now();
    let sent = match driver
        .ops
        .send(SendRequest {
            conversation_id: None,
            text: rendered.text.clone(),
            model: Some(line.spec.clone()),
            files: rendered.attachments.clone(),
            mention: None,
            mention_strategy: MentionStrategy::default(),
        })
        .await
    {
        Ok(sent) => sent,
        Err(err) => {
            fail(map_driver_error(&err)).await;
            return;
        }
    };
    let conversation_id = sent.conversation_id.clone();
    let source = OpsSource(&driver.ops);
    let outcome = stream::PollLoop {
        source: &source,
        conversation_id: conversation_id.clone(),
        tracker: stream::ReplyTracker::new(&rendered.text),
        mode: stream::TrackMode::None,
        poll_interval: workspace.settings.poll_interval,
        idle_timeout: workspace.settings.idle_timeout,
        sent_at,
        dom: Some(&source),
        anchor: rendered.text.clone(),
    }
    .run(&mut assembler, consumer_dropped)
    .await;
    match outcome {
        stream::PollOutcome::Done { text_chars, .. } => {
            archive_conversation(&driver.ops, &conversation_id).await;
            let _ = tx_event
                .send(Ok(ResponseEvent::Completed {
                    response_id: conversation_id,
                    token_usage: Some(estimate_usage(transcript_chars, text_chars)),
                    usage_metadata: None,
                    end_turn: Some(true),
                }))
                .await;
        }
        stream::PollOutcome::Interrupted => {
            stop_generation(&driver.ops, &conversation_id).await;
        }
        stream::PollOutcome::Stalled { seconds } => {
            stop_generation(&driver.ops, &conversation_id).await;
            fail(CodexErr::UnsupportedOperation(format!(
                "chatgpt_web: no progress for {seconds}s; generation stopped"
            )))
            .await;
        }
        stream::PollOutcome::PartialCompletion => {
            fail(CodexErr::Stream(
                "ChatGPT stopped before finishing the compaction summary".to_string(),
            ))
            .await;
        }
        stream::PollOutcome::Failed(err) => {
            fail(map_driver_error(&err)).await;
        }
    }
}

/// Best-effort click on the stop button, bounded so a wedged tab cannot
/// hold the turn open.
async fn stop_generation(ops: &ChatGptOps, conversation_id: &str) {
    match tokio::time::timeout(CLEANUP_BUDGET, ops.stop(Some(conversation_id))).await {
        Ok(Ok(outcome)) => info!("chatgpt_web: stop → {}", outcome.detail),
        Ok(Err(err)) => warn!("chatgpt_web: stop failed: {err}"),
        Err(_) => warn!("chatgpt_web: stop timed out"),
    }
}

/// Best-effort `PATCH {is_archived: true}`, bounded.
async fn archive_conversation(ops: &ChatGptOps, conversation_id: &str) {
    let archive = async {
        let api = ops.api_for(Some(conversation_id)).await?;
        api.patch_conversation(conversation_id, serde_json::json!({ "is_archived": true }))
            .await
    };
    match tokio::time::timeout(CLEANUP_BUDGET, archive).await {
        Ok(Ok(())) => info!("chatgpt_web: archived conversation {conversation_id}"),
        Ok(Err(err)) => warn!("chatgpt_web: archiving {conversation_id} failed: {err}"),
        Err(_) => warn!("chatgpt_web: archiving {conversation_id} timed out"),
    }
}

/// FORK: archives the ChatGPT conversation recorded for `thread_id` (root
/// shutdown, explicit agent close) and forgets its record.
///
/// A no-op for threads without a record, so callers need not know which
/// provider served the thread. Gated by `[chatgpt_web] archive_on_shutdown`.
pub(crate) async fn archive_thread_conversation(
    config: &crate::config::Config,
    thread_id: codex_protocol::ThreadId,
) {
    if !config.chatgpt_web.archive_on_shutdown {
        return;
    }
    let path = config
        .codex_home
        .to_path_buf()
        .join(SESSIONS_STATE_FILE_NAME);
    let thread_key = thread_id.to_string();
    let Some(conversation_id) =
        sessions::load(&path, &thread_key).and_then(|record| record.conversation_id)
    else {
        return;
    };
    let driver = shared_driver(&config.chatgpt_web, config.codex_home.as_path());
    archive_conversation(&driver.ops, &conversation_id).await;
    sessions::forget(&path, &thread_key);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_model_line_maps_to_a_spec() {
        assert_eq!(
            model_line("chatgpt-web/instant").unwrap().spec,
            ModelSpec::Instant
        );
        assert!(model_line("chatgpt-web/pro").unwrap().is_pro);
        assert_eq!(
            model_line("chatgpt-web/high").unwrap().spec,
            ModelSpec::High
        );
        assert!(model_line("gpt-5").is_none());
    }

    #[test]
    fn usage_adds_the_hidden_reserve_and_rounds_up() {
        let usage = estimate_usage(10, 5);
        assert_eq!(usage.input_tokens, 3 + HIDDEN_TOKEN_RESERVE);
        assert_eq!(usage.output_tokens, 2);
        assert_eq!(usage.total_tokens, usage.input_tokens + usage.output_tokens);
    }

    /// FORK: `wait_extension_connected` already burned the whole budget, so
    /// this verdict must be terminal — a retryable one made the turn loop
    /// reconnect five more times, waiting 90s each, for a closed browser.
    #[test]
    fn a_browser_that_never_showed_up_fails_the_turn_once() {
        let waited = Duration::from_secs(90);
        let closed = chrome_mcp_unready(waited, None);
        assert!(!closed.is_retryable(), "{closed}");
        assert!(closed.to_string().contains("90s"), "{closed}");
        let unreachable = chrome_mcp_unready(
            waited,
            Some(&DriverError::new(DriverErrorKind::DaemonDown, "refused")),
        );
        assert!(!unreachable.is_retryable(), "{unreachable}");
        assert!(unreachable.to_string().contains("refused"), "{unreachable}");
    }

    #[test]
    fn driver_errors_map_to_the_retry_classes_of_the_plan() {
        use codex_protocol::error::CodexErrorDetails;
        let stream = |kind| {
            matches!(
                map_driver_error(&DriverError::new(kind, "x")).details(),
                CodexErrorDetails::Stream(_)
            )
        };
        let unsupported = |kind| {
            matches!(
                map_driver_error(&DriverError::new(kind, "x")).details(),
                CodexErrorDetails::UnsupportedOperation(_)
            )
        };
        assert!(stream(DriverErrorKind::DaemonDown));
        assert!(stream(DriverErrorKind::RateLimited));
        assert!(stream(DriverErrorKind::Upstream));
        assert!(stream(DriverErrorKind::Timeout));
        assert!(stream(DriverErrorKind::ConversationNotFound));
        assert!(unsupported(DriverErrorKind::LoginRequired));
        assert!(unsupported(DriverErrorKind::UiChanged));
        assert!(unsupported(DriverErrorKind::SubmitAmbiguous));
        assert!(matches!(
            map_driver_error(&DriverError::new(DriverErrorKind::MessageTooLong, "x")).details(),
            CodexErrorDetails::ContextWindowExceeded
        ));
        let with_phase = DriverError::new(DriverErrorKind::UiChanged, "no composer")
            .with_phase(driver::FailurePhase::Compose);
        assert!(
            map_driver_error(&with_phase)
                .to_string()
                .contains("compose")
        );
    }

    #[test]
    fn a_function_target_becomes_a_function_call_item() {
        let target = CallTarget::Function {
            namespace: Some("figma".to_string()),
            name: "get_file".to_string(),
            arguments: serde_json::json!({"id": "1"}),
        };
        match target_to_item(&target, "call_1") {
            ResponseItem::FunctionCall {
                name,
                namespace,
                arguments,
                call_id,
                encrypted_function_args,
                ..
            } => {
                assert_eq!(name, "get_file");
                assert_eq!(namespace.as_deref(), Some("figma"));
                assert_eq!(call_id, "call_1");
                assert_eq!(encrypted_function_args, Some(Vec::new()));
                // Arguments are re-serialized to the JSON string the loop wants.
                assert_eq!(
                    serde_json::from_str::<serde_json::Value>(&arguments).unwrap(),
                    serde_json::json!({"id": "1"})
                );
            }
            other => panic!("unexpected item {other:?}"),
        }
    }

    #[test]
    fn a_custom_target_becomes_a_custom_tool_call_item() {
        let target = CallTarget::Custom {
            name: "apply_patch".to_string(),
            input: "*** Begin Patch".to_string(),
        };
        match target_to_item(&target, "call_2") {
            ResponseItem::CustomToolCall {
                name,
                input,
                call_id,
                ..
            } => {
                assert_eq!(name, "apply_patch");
                assert_eq!(input, "*** Begin Patch");
                assert_eq!(call_id, "call_2");
            }
            other => panic!("unexpected item {other:?}"),
        }
    }

    #[test]
    fn extract_output_finds_function_and_custom_outputs_by_call_id() {
        use codex_protocol::models::FunctionCallOutputBody;
        use codex_protocol::models::FunctionCallOutputPayload;
        let input = vec![
            ResponseItem::FunctionCallOutput {
                id: None,
                call_id: Some("call_1".to_string()),
                name: None,
                namespace: None,
                output: FunctionCallOutputPayload {
                    body: FunctionCallOutputBody::Text("out-1".to_string()),
                    success: Some(true),
                },
                internal_chat_message_metadata_passthrough: None,
            },
            ResponseItem::CustomToolCallOutput {
                id: None,
                call_id: "call_2".to_string(),
                name: None,
                output: FunctionCallOutputPayload {
                    body: FunctionCallOutputBody::Text("out-2".to_string()),
                    success: Some(false),
                },
                internal_chat_message_metadata_passthrough: None,
            },
        ];
        assert_eq!(
            extract_output(&input, "call_1").unwrap().body,
            FunctionCallOutputBody::Text("out-1".to_string())
        );
        assert!(!extract_output(&input, "call_2").unwrap().success.unwrap());
        assert!(extract_output(&input, "call_missing").is_none());
    }

    #[test]
    fn thread_state_round_trips_through_the_sessions_file() {
        let temp = tempfile::tempdir().expect("tempdir");
        let path = temp.path().join(SESSIONS_STATE_FILE_NAME);
        let state = ChatGptWebThreadState::default();
        state.hydrate(&path, "thread-1".to_string());
        state.record(ConversationContinuity {
            conversation_id: Some("conv".to_string()),
            model_slug: Some("chatgpt-web/thinking".to_string()),
            delivered_items: 2,
            delivered_fingerprint: 9,
            echoed: vec![1],
            message_landed_unanswered: false,
        });
        state.mark_unanswered(vec![2, 3]);

        let rebuilt = ChatGptWebThreadState::default();
        rebuilt.hydrate(&path, "thread-1".to_string());
        let snapshot = rebuilt.snapshot();
        assert_eq!(snapshot.conversation_id.as_deref(), Some("conv"));
        assert!(snapshot.message_landed_unanswered);
        assert_eq!(snapshot.echoed, vec![2, 3]);

        rebuilt.invalidate();
        assert!(
            sessions::load(&path, "thread-1")
                .unwrap()
                .conversation_id
                .is_none()
        );
    }
}
