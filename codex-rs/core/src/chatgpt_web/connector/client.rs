//! FORK: the session side of the connector protocol.
//!
//! `DaemonSessionBroker` is the `ConnectorBroker` the provider drives. It owns
//! one registration with the shared `codex chatgpt-web daemon` per Codex
//! process (sessions are cheap on the daemon but the heartbeat is not, and one
//! `session_id` per process is what the daemon expects), and turns the daemon's
//! loopback control API into the `ConnectorTurn` the provider consumes:
//!
//! - `begin_turn` waits for the connector to be `Verified`, mints a
//!   `turn_token`, registers the turn, and — the first time — starts one shared
//!   long-poll that fans the daemon's tool calls out to the turn that owns each
//!   `turn_token`;
//! - each `ToolRequest` carries a `oneshot` the provider fulfils once Codex has
//!   run the tool; the reply is posted back as the call's result;
//! - `end_turn` retires the turn.

use super::BeginTurn;
use super::ConnectorBroker;
use super::ConnectorTurn;
use super::ToolRequest;
use super::daemon::state::FailureKind;
use super::daemon::state::RegistryStatus;
use super::daemon::state::now_ms;
use super::daemon::wire;
use codex_protocol::models::FunctionCallOutputBody;
use codex_protocol::models::FunctionCallOutputContentItem;
use codex_protocol::models::FunctionCallOutputPayload;
use futures::future::BoxFuture;
use std::collections::HashMap;
use std::collections::HashSet;
use std::sync::Arc;
use std::sync::Mutex as StdMutex;
use std::time::Duration;
use tokio::sync::mpsc;
use tokio::sync::oneshot;
use tokio_util::sync::CancellationToken;
use tracing::debug;
use tracing::warn;

/// Bound on the tool-call channel handed to one connector turn.
const REQUEST_CHANNEL_SIZE: usize = 32;

/// How often the session heartbeats the daemon (dead after 30s of silence).
const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(10);

/// How long a `Verified` poll waits between checks.
const READY_POLL_INTERVAL: Duration = Duration::from_millis(500);

/// The daemon endpoint the broker talks to.
#[derive(Clone)]
pub(crate) struct DaemonHandle {
    http: reqwest::Client,
    control_url: String,
    token: String,
}

impl std::fmt::Debug for DaemonHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DaemonHandle")
            .field("control_url", &self.control_url)
            .finish_non_exhaustive()
    }
}

impl DaemonHandle {
    pub(crate) fn new(control_url: String, token: String) -> Self {
        Self {
            http: reqwest::Client::builder()
                .no_proxy()
                .build()
                .unwrap_or_default(),
            control_url,
            token,
        }
    }

    /// FORK: asks the daemon to reconcile now, on behalf of a starting turn.
    ///
    /// The `Turn` trigger is the one allowed to override a parked registry, so
    /// a user who has just fixed their tunnel gets a fresh attempt on the very
    /// next turn instead of waiting out the backoff ladder.
    async fn refresh_registry(
        &self,
        timeout: Duration,
    ) -> Result<wire::ReconcileResponse, String> {
        let response = self
            .http
            .post(format!("{}/v1/registry/refresh", self.control_url))
            .bearer_auth(&self.token)
            .timeout(timeout)
            .send()
            .await
            .map_err(|err| err.to_string())?;
        if !response.status().is_success() {
            return Err(format!("HTTP {}", response.status()));
        }
        response
            .json::<wire::ReconcileResponse>()
            .await
            .map_err(|err| err.to_string())
    }

    async fn health(&self) -> Result<wire::HealthResponse, String> {
        self.http
            .get(format!("{}/healthz", self.control_url))
            .timeout(Duration::from_secs(5))
            .send()
            .await
            .map_err(|err| err.to_string())?
            .json::<wire::HealthResponse>()
            .await
            .map_err(|err| err.to_string())
    }
}

/// One registered Codex session, shared by every connector turn in the process.
struct SharedSession {
    daemon: DaemonHandle,
    session_id: String,
    /// Per-turn sinks, keyed by `turn_token`; the shared poll task routes each
    /// batch to the turn that owns it.
    turns: Arc<StdMutex<HashMap<String, mpsc::Sender<ToolRequest>>>>,
    /// Stops the heartbeat and poll tasks when the session is dropped.
    cancel: CancellationToken,
}

impl Drop for SharedSession {
    fn drop(&mut self) {
        self.cancel.cancel();
        // Best-effort DELETE so the daemon retires the session's turns at once.
        let daemon = self.daemon.clone();
        let session_id = self.session_id.clone();
        tokio::spawn(async move {
            let _ = daemon
                .http
                .delete(format!("{}/v1/sessions/{session_id}", daemon.control_url))
                .bearer_auth(&daemon.token)
                .timeout(Duration::from_secs(3))
                .send()
                .await;
        });
    }
}

impl SharedSession {
    async fn register(daemon: DaemonHandle) -> Result<Arc<Self>, String> {
        let session_id = format!(
            "codex-{}-{}",
            std::process::id(),
            super::daemon::state::new_token()
        );
        let request = wire::RegisterSessionRequest {
            codex_pid: std::process::id(),
            session_id: session_id.clone(),
            codex_version: super::daemon::DAEMON_VERSION.to_string(),
        };
        let response = daemon
            .http
            .post(format!("{}/v1/sessions", daemon.control_url))
            .bearer_auth(&daemon.token)
            .json(&request)
            .timeout(Duration::from_secs(10))
            .send()
            .await
            .map_err(|err| format!("registering the Codex session with the daemon: {err}"))?;
        if !response.status().is_success() {
            return Err(format!(
                "the daemon refused the session registration (HTTP {})",
                response.status()
            ));
        }

        let cancel = CancellationToken::new();
        let session = Arc::new(Self {
            daemon: daemon.clone(),
            session_id: session_id.clone(),
            turns: Arc::new(StdMutex::new(HashMap::new())),
            cancel: cancel.clone(),
        });
        spawn_heartbeat(daemon.clone(), session_id.clone(), cancel.clone());
        spawn_poll_loop(daemon, session_id, Arc::clone(&session.turns), cancel);
        Ok(session)
    }

    fn register_turn_sink(&self, turn_token: &str, sink: mpsc::Sender<ToolRequest>) {
        self.turns
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(turn_token.to_string(), sink);
    }

    fn drop_turn_sink(&self, turn_token: &str) {
        self.turns
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(turn_token);
    }
}

fn spawn_heartbeat(daemon: DaemonHandle, session_id: String, cancel: CancellationToken) {
    tokio::spawn(async move {
        loop {
            tokio::select! {
                _ = cancel.cancelled() => return,
                _ = tokio::time::sleep(HEARTBEAT_INTERVAL) => {}
            }
            let sent = daemon
                .http
                .post(format!(
                    "{}/v1/sessions/{session_id}/heartbeat",
                    daemon.control_url
                ))
                .bearer_auth(&daemon.token)
                .timeout(Duration::from_secs(5))
                .send()
                .await;
            match sent {
                Ok(response) if response.status().as_u16() == 404 => {
                    // The daemon forgot us (restart / eviction); stop pretending.
                    warn!("chatgpt_web connector: the daemon no longer knows this session");
                    return;
                }
                Ok(_) => {}
                Err(err) => warn!("chatgpt_web connector: heartbeat failed: {err}"),
            }
        }
    });
}

/// One long-poll per session; routes each batch to the owning turn.
fn spawn_poll_loop(
    daemon: DaemonHandle,
    session_id: String,
    turns: Arc<StdMutex<HashMap<String, mpsc::Sender<ToolRequest>>>>,
    cancel: CancellationToken,
) {
    tokio::spawn(async move {
        let mut after = 0u64;
        let mut seen: HashSet<String> = HashSet::new();
        loop {
            let poll = daemon
                .http
                .get(format!(
                    "{}/v1/sessions/{session_id}/calls?after={after}&wait_ms=30000",
                    daemon.control_url
                ))
                .bearer_auth(&daemon.token)
                .timeout(Duration::from_secs(45))
                .send();
            let response = tokio::select! {
                _ = cancel.cancelled() => return,
                response = poll => response,
            };
            let batches = match response {
                Ok(response) if response.status().is_success() => {
                    match response.json::<wire::CallsResponse>().await {
                        Ok(body) => {
                            after = body.seq.max(after);
                            body.batches
                        }
                        Err(err) => {
                            warn!("chatgpt_web connector: decoding a call batch failed: {err}");
                            continue;
                        }
                    }
                }
                Ok(response) if response.status().as_u16() == 404 => {
                    // Session gone: nothing more will arrive.
                    return;
                }
                Ok(response) => {
                    warn!(
                        "chatgpt_web connector: poll returned HTTP {}",
                        response.status()
                    );
                    tokio::time::sleep(Duration::from_secs(1)).await;
                    continue;
                }
                Err(err) => {
                    warn!("chatgpt_web connector: poll failed: {err}");
                    tokio::time::sleep(Duration::from_secs(1)).await;
                    continue;
                }
            };

            for batch in batches {
                let sink = turns
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .get(&batch.turn_token)
                    .cloned();
                let Some(sink) = sink else {
                    // A turn we do not own (already ended locally): let the
                    // daemon's deadline turn it into an error.
                    continue;
                };
                for call in batch.calls {
                    if !seen.insert(call.call_id.clone()) {
                        continue;
                    }
                    let (respond, reply) = oneshot::channel::<FunctionCallOutputPayload>();
                    let request = ToolRequest {
                        call_id: call.call_id.clone(),
                        target: call.target.clone(),
                        respond,
                    };
                    if sink.send(request).await.is_err() {
                        // The turn stopped consuming; the call will time out at
                        // the daemon.
                        continue;
                    }
                    spawn_result_poster(
                        daemon.clone(),
                        session_id.clone(),
                        call.call_id,
                        reply,
                        cancel.clone(),
                    );
                }
            }
        }
    });
}

/// Waits for the provider to run one tool, then posts its result.
fn spawn_result_poster(
    daemon: DaemonHandle,
    session_id: String,
    call_id: String,
    reply: oneshot::Receiver<FunctionCallOutputPayload>,
    cancel: CancellationToken,
) {
    tokio::spawn(async move {
        let payload = tokio::select! {
            _ = cancel.cancelled() => return,
            payload = reply => payload,
        };
        let Ok(payload) = payload else {
            // The provider dropped the responder (turn aborted); the daemon's
            // deadline handles it.
            return;
        };
        let request = payload_to_result(&session_id, &payload);
        let posted = daemon
            .http
            .post(format!("{}/v1/calls/{call_id}/result", daemon.control_url))
            .bearer_auth(&daemon.token)
            .json(&request)
            .timeout(Duration::from_secs(10))
            .send()
            .await;
        if let Err(err) = posted {
            warn!("chatgpt_web connector: posting the call result failed: {err}");
        }
    });
}

/// Maps a Codex tool output onto the daemon's result wire shape.
fn payload_to_result(
    session_id: &str,
    payload: &FunctionCallOutputPayload,
) -> wire::CallResultRequest {
    let is_error = payload.success == Some(false);
    let content = match &payload.body {
        FunctionCallOutputBody::Text(text) => {
            vec![wire::ResultContent::Text { text: text.clone() }]
        }
        FunctionCallOutputBody::ContentItems(items) => items
            .iter()
            .filter_map(|item| match item {
                FunctionCallOutputContentItem::InputText { text } => {
                    Some(wire::ResultContent::Text { text: text.clone() })
                }
                FunctionCallOutputContentItem::InputImage { image_url, .. } => {
                    image_to_result(image_url)
                }
                _ => None,
            })
            .collect(),
    };
    wire::CallResultRequest {
        session_id: session_id.to_string(),
        content,
        is_error,
        structured: None,
    }
}

/// A `data:<mime>;base64,<data>` image URL → an image result; anything else is
/// dropped (the connector cannot forward a remote URL to ChatGPT as an image).
fn image_to_result(image_url: &str) -> Option<wire::ResultContent> {
    let rest = image_url.strip_prefix("data:")?;
    let (mime, data) = rest.split_once(";base64,")?;
    Some(wire::ResultContent::Image {
        data: data.to_string(),
        mime_type: mime.to_string(),
    })
}

/// The provider-facing broker: one shared session, many turns.
pub(crate) struct DaemonSessionBroker {
    session: Arc<SharedSession>,
    connector_name: String,
}

impl std::fmt::Debug for DaemonSessionBroker {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DaemonSessionBroker")
            .field("connector_name", &self.connector_name)
            .finish_non_exhaustive()
    }
}

impl DaemonSessionBroker {
    /// Builds a broker over an already-registered session (used by tests and by
    /// `ensure`).
    fn with_session(session: Arc<SharedSession>, connector_name: String) -> Self {
        Self {
            session,
            connector_name,
        }
    }

    /// Registers a session with the daemon at `control_url`.
    pub(crate) async fn connect(
        control_url: String,
        token: String,
        connector_name: String,
    ) -> Result<Self, String> {
        let session = SharedSession::register(DaemonHandle::new(control_url, token)).await?;
        Ok(Self::with_session(session, connector_name))
    }

    async fn wait_verified(&self, ready_timeout: Duration) -> Result<(), String> {

        let deadline = tokio::time::Instant::now() + ready_timeout;
        // FORK: ask for a reconcile up front rather than polling a status the
        // daemon's own backoff may not revisit for half an hour. This is also
        // the only trigger that overrides a parked registry, so the turn right
        // after the user fixes something gets a real attempt.
        match self
            .session
            .daemon
            .refresh_registry(ready_timeout.min(Duration::from_secs(60)))
            .await
        {
            Ok(response) => {
                if let Some(error) = terminal_registry_error(&response.detail) {
                    return Err(error);
                }
                if matches!(response.detail, RegistryStatus::Verified { .. }) {
                    return Ok(());
                }
            }
            // 501 on a build without the registry, or the daemon is busy: the
            // poll below is still the right fallback.
            Err(err) => debug!("chatgpt_web connector: registry refresh unavailable: {err}"),
        }
        let mut last_seen: Option<String> = None;
        // FORK (verified live): `browser_unavailable` is transient — the
        // chrome-mcp extension's service worker sleeps and the daemon's next
        // reconcile brings the connector back (67s, once). Failing the turn on
        // the first sighting killed a consultant agent 19s in while the
        // 90s budget was barely touched, so it is retried like any other
        // not-yet-verified state and only reported if the deadline runs out.
        let mut browser_unavailable = false;
        loop {
            match self.session.daemon.health().await {
                Ok(health) if health.registry_status == "verified" => return Ok(()),
                Ok(health) if health.registry_status == "developer_mode_off" => {
                    return Err(
                        "the ChatGPT connector needs Developer Mode, which is off. Turn it on (Settings → Apps → Advanced) or run `codex chatgpt-web doctor`."
                            .to_string(),
                    );
                }
                Ok(health) => {
                    browser_unavailable = health.registry_status == "browser_unavailable";
                    // FORK: a failure the user has to fix is not going to
                    // resolve inside this budget; say what it is now.
                    if health.registry_status == "failed"
                        && let Some(error) = terminal_health_error(&health)
                    {
                        return Err(error);
                    }
                    last_seen = Some(match health.registry_reason.as_deref() {
                        Some(reason) => format!("{} ({reason})", health.registry_status),
                        None => health.registry_status.clone(),
                    });
                }
                Err(err) => warn!("chatgpt_web connector: health check failed: {err}"),
            }
            if tokio::time::Instant::now() >= deadline {
                if browser_unavailable {
                    return Err(
                        "the daemon could not reach chatgpt.com through chrome-mcp to register the connector; make sure Chrome and the chrome-mcp extension are running."
                            .to_string(),
                    );
                }
                return Err(format!(
                    "the ChatGPT connector was not ready within {}s (last status: {}); run `codex chatgpt-web registry show` to see why",
                    ready_timeout.as_secs(),
                    last_seen.as_deref().unwrap_or("unknown")
                ));
            }
            tokio::time::sleep(READY_POLL_INTERVAL).await;
        }
    }
}

/// FORK: the message for a registry failure no amount of waiting will fix.
fn terminal_registry_error(status: &RegistryStatus) -> Option<String> {
    let RegistryStatus::Failed {
        reason,
        retry_at_ms,
        kind,
        parked,
    } = status
    else {
        return None;
    };
    if !kind.is_terminal() {
        return None;
    }
    Some(render_terminal_registry_error(
        reason,
        kind.label(),
        *parked,
        Some(*retry_at_ms),
    ))
}

/// The same, from the `/healthz` shape the poll loop sees.
fn terminal_health_error(health: &wire::HealthResponse) -> Option<String> {
    let kind = FailureKind::parse(health.registry_failure_kind.as_deref()?)?;
    if !kind.is_terminal() {
        return None;
    }
    Some(render_terminal_registry_error(
        health.registry_reason.as_deref().unwrap_or("no reason given"),
        kind.label(),
        health.registry_parked,
        health.registry_retry_at_ms,
    ))
}

fn render_terminal_registry_error(
    reason: &str,
    kind: &str,
    parked: bool,
    retry_at_ms: Option<u64>,
) -> String {
    let retry = if parked {
        "automatic retries are parked".to_string()
    } else {
        let seconds = retry_at_ms
            .map(|at| at.saturating_sub(now_ms()) / 1000)
            .unwrap_or(0);
        format!("the daemon retries in {seconds}s")
    };
    format!(
        "the ChatGPT connector cannot be used right now: {reason} (registry status failed/{kind}; {retry}).          Run `codex chatgpt-web registry show` to see the current state, then `codex chatgpt-web registry reconcile` after fixing it"
    )
}

impl ConnectorBroker for DaemonSessionBroker {
    fn begin_turn<'a>(
        &'a self,
        params: BeginTurn<'a>,
    ) -> BoxFuture<'a, Result<ConnectorTurn, String>> {
        Box::pin(async move {
            self.wait_verified(params.ready_timeout).await?;
            let turn_token = super::daemon::state::new_token();
            let request = wire::RegisterTurnRequest {
                session_id: self.session.session_id.clone(),
                turn_token: turn_token.clone(),
                thread_id: params.thread_id.to_string(),
                turn_id: params.turn_id.to_string(),
                ttl_ms: params.ttl_ms,
                tools: params.tools,
                exec_tool: params.exec_tool,
                apply_patch: params.apply_patch,
            };
            let response = self
                .session
                .daemon
                .http
                .post(format!("{}/v1/turns", self.session.daemon.control_url))
                .bearer_auth(&self.session.daemon.token)
                .json(&request)
                .timeout(Duration::from_secs(10))
                .send()
                .await
                .map_err(|err| format!("registering the connector turn: {err}"))?;
            if !response.status().is_success() {
                return Err(format!(
                    "the daemon refused the turn registration (HTTP {})",
                    response.status()
                ));
            }
            let (sink, requests) = mpsc::channel::<ToolRequest>(REQUEST_CHANNEL_SIZE);
            self.session.register_turn_sink(&turn_token, sink);
            Ok(ConnectorTurn {
                turn_token,
                connector_name: self.connector_name.clone(),
                requests,
            })
        })
    }

    fn prompt_contract(&self, turn: &ConnectorTurn) -> Vec<String> {
        let name = &turn.connector_name;
        let token = &turn.turn_token;
        vec![
            format!(
                "This chat is connected to the Codex session on the user's computer through the \"{name}\" connector. Call its tools to run commands, edit files, view images and reach every tool Codex has."
            ),
            format!(
                "Pass turn_token {token} unchanged to every {name} call in this response, including continuations after tool results; never expose it in the answer."
            ),
            format!(
                "{name} tools: codex_exec runs a shell command; codex_write_stdin feeds a running command; codex_apply_patch edits files; codex_view_image reads an image; codex_tool_inventory lists every other Codex tool; codex_tool_call invokes one of them. Prefer codex_tool_inventory then codex_tool_call for anything the first four do not cover."
            ),
        ]
    }

    fn end_turn<'a>(&'a self, turn_token: &'a str, reason: &'a str) -> BoxFuture<'a, ()> {
        Box::pin(async move {
            self.session.drop_turn_sink(turn_token);
            let _ = self
                .session
                .daemon
                .http
                .delete(format!(
                    "{}/v1/turns/{turn_token}",
                    self.session.daemon.control_url
                ))
                .bearer_auth(&self.session.daemon.token)
                .json(&wire::EndTurnRequest {
                    reason: Some(reason.to_string()),
                })
                .timeout(Duration::from_secs(5))
                .send()
                .await;
        })
    }
}

#[cfg(test)]
#[path = "client_tests.rs"]
mod tests;
