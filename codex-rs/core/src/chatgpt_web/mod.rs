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

use crate::claude_code::assembler::StreamAssembler;
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
    /// The connector broker, attached per sampling request when
    /// `tools = "connector"`; `None` in `tools = "none"`.
    // TODO(M6): read by the connector turn.
    #[allow(dead_code)]
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
    let (tx_event, rx_event) = mpsc::channel(EVENT_CHANNEL_SIZE);
    let consumer_dropped = CancellationToken::new();
    let consumer_dropped_for_task = consumer_dropped.clone();

    tokio::spawn(async move {
        run_turn(
            input,
            model_slug,
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
        tabs,
        settings.base_url.clone(),
    ));
    let driver = Arc::new(SharedDriver { daemon, ops });
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

async fn run_turn(
    input: Vec<ResponseItem>,
    model_slug: String,
    workspace: ChatGptWebWorkspace,
    state: Arc<ChatGptWebThreadState>,
    tx_event: mpsc::Sender<Result<ResponseEvent>>,
    consumer_dropped: CancellationToken,
) {
    if tx_event.send(Ok(ResponseEvent::Created)).await.is_err() {
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
        // FORK: M6/C3 attach the broker and drive the connector turn.
        fail(CodexErr::UnsupportedOperation(
            "[chatgpt_web] tools = \"connector\" needs the connector daemon, which this build does not wire yet; use tools = \"none\"".to_string(),
        ))
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
    match driver.daemon.health().await {
        Ok(health) if health.ok && health.extension_connected => {}
        Ok(_) => {
            fail(CodexErr::Stream(
                "chrome-mcp daemon is up but its Chrome extension is not connected".to_string(),
            ))
            .await;
            return;
        }
        Err(err) => {
            fail(map_driver_error(&err)).await;
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
        connector_rx: None,
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
