//! FORK: the turn broker — who owns a `turn_token`, and how a tool call that
//! ChatGPT makes against the public MCP server reaches the Codex session that
//! must execute it.
//!
//! Every Codex turn in connector mode registers a fresh token here together
//! with the tools it announced. ChatGPT's first call with that token mints a
//! *binding* (idempotent, so retries land on the same turn); each call is
//! queued, batched for a few milliseconds so a burst becomes one delivery, and
//! handed to the owning session through its long-poll. The session posts the
//! result, which completes the oneshot the MCP handler is waiting on.
//!
//! Tokens are retired when the turn ends — with the reason kept in a bounded
//! LRU — so a token that ChatGPT copies out of an old conversation gets a
//! precise "already finished" answer instead of silently doing nothing.

use super::wire::CallBatchWire;
use super::wire::CallsResponse;
use super::wire::PendingCallWire;
use super::wire::ResultContent;
use crate::chatgpt_web::connector::contract::CallTarget;
use crate::chatgpt_web::connector::contract::ExecTool;
use crate::chatgpt_web::connector::contract::ToolSummary;
use crate::chatgpt_web::connector::contract::TurnTools;
use base64::Engine;
use serde_json::Value;
use std::collections::HashMap;
use std::collections::HashSet;
use std::collections::VecDeque;
use std::fmt;
use std::sync::Arc;
use std::sync::Mutex;
use std::time::Duration;
use std::time::Instant;
use std::time::SystemTime;
use std::time::UNIX_EPOCH;
use tokio::sync::Notify;
use tokio::sync::oneshot;

/// Tunables; the defaults are the plan's.
#[derive(Debug, Clone)]
pub struct BrokerConfig {
    /// Longest the MCP handler waits for a session to answer one call.
    pub call_timeout: Duration,
    /// How long calls accumulate before one delivery is made.
    pub batch_window: Duration,
    /// A session that has not polled or heartbeated for this long is dead.
    pub heartbeat_timeout: Duration,
    /// Retired tokens remembered for the "already finished" message.
    pub retire_cap: usize,
    /// Default `yield_time_ms` for `codex_exec` when ChatGPT passes none.
    pub exec_default_yield_ms: u64,
}

impl Default for BrokerConfig {
    fn default() -> Self {
        Self {
            call_timeout: Duration::from_secs(120),
            batch_window: Duration::from_millis(15),
            heartbeat_timeout: Duration::from_secs(30),
            retire_cap: 256,
            exec_default_yield_ms: 10_000,
        }
    }
}

/// What a session reports back for one call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BrokerResult {
    pub content: Vec<ResultContent>,
    pub is_error: bool,
    pub structured: Option<Value>,
}

impl BrokerResult {
    pub fn error(text: impl Into<String>) -> Self {
        Self {
            content: vec![ResultContent::Text { text: text.into() }],
            is_error: true,
            structured: None,
        }
    }
}

/// Why a `turn_token` could not be claimed. `Display` is the text ChatGPT sees.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ClaimError {
    Unknown,
    Retired { trace: String, reason: String },
    Expired,
    SessionGone,
}

impl fmt::Display for ClaimError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unknown => f.write_str(
                "turn_token is invalid, expired, or revoked. Use the turn_token from the current Codex prompt, unchanged.",
            ),
            Self::Retired { trace, reason } => write!(
                f,
                "This turn_token was issued for Codex turn {trace}, which has already finished ({reason}). Do not reuse tokens from earlier messages; wait for a new Codex request."
            ),
            Self::Expired => f.write_str(
                "This turn_token has expired: the Codex turn it belonged to is over. Wait for a new Codex request.",
            ),
            Self::SessionGone => f.write_str(
                "The Codex session that issued this turn_token has disconnected; nothing can execute the call.",
            ),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RegisterTurnError {
    UnknownSession,
    Duplicate,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CompleteError {
    UnknownCall,
    WrongSession,
    NotInFlight,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnknownSession;

/// A successful claim: the binding to invoke through, and the turn's tools.
#[derive(Debug, Clone)]
pub struct Claim {
    pub binding: String,
    pub turn_token: String,
    pub tools: TurnTools,
}

/// What `register_turn` needs to know.
#[derive(Debug, Clone)]
pub struct TurnRegistration {
    pub session_id: String,
    pub turn_token: String,
    /// Shown in "already finished" messages: `<thread_id>/<turn_id>`.
    pub trace: String,
    pub ttl: Duration,
    pub tools: Arc<[ToolSummary]>,
    pub exec_tool: ExecTool,
    pub apply_patch: bool,
}

struct SessionChannel {
    codex_pid: u32,
    last_seen: Instant,
    next_seq: u64,
    /// Delivered-but-unacked batches, redelivered on the next poll.
    pending: VecDeque<CallBatchWire>,
    notify: Arc<Notify>,
    turns: HashSet<String>,
}

struct TurnChannel {
    session_id: String,
    trace: String,
    tools: Arc<[ToolSummary]>,
    exec_tool: ExecTool,
    apply_patch: bool,
    binding: Option<String>,
    expires_at: Instant,
    queued: Vec<PendingCallWire>,
    batch_armed: bool,
    in_flight: HashSet<String>,
}

struct CallSlot {
    turn_token: String,
    session_id: String,
    tx: oneshot::Sender<BrokerResult>,
}

#[derive(Debug, Clone)]
struct RetiredTurn {
    trace: String,
    reason: String,
}

#[derive(Default)]
struct BrokerState {
    sessions: HashMap<String, SessionChannel>,
    turns: HashMap<String, TurnChannel>,
    bindings: HashMap<String, String>,
    retired: HashMap<String, RetiredTurn>,
    retired_order: VecDeque<String>,
    calls: HashMap<String, CallSlot>,
}

pub struct TurnBroker {
    inner: Mutex<BrokerState>,
    config: BrokerConfig,
}

impl fmt::Debug for TurnBroker {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TurnBroker")
            .field("config", &self.config)
            .finish_non_exhaustive()
    }
}

fn random_id(prefix: &str, bytes: usize) -> String {
    use rand::RngCore;
    let mut buf = vec![0u8; bytes];
    rand::rng().fill_bytes(&mut buf);
    format!(
        "{prefix}{}",
        base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(buf)
    )
}

fn unix_ms(at: SystemTime) -> u64 {
    at.duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

impl TurnBroker {
    pub fn new(config: BrokerConfig) -> Arc<Self> {
        Arc::new(Self {
            inner: Mutex::new(BrokerState::default()),
            config,
        })
    }

    pub fn config(&self) -> &BrokerConfig {
        &self.config
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, BrokerState> {
        self.inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// `(sessions, active turns)` for `/healthz`.
    pub fn stats(&self) -> (usize, usize) {
        let state = self.lock();
        (state.sessions.len(), state.turns.len())
    }

    /// Registers (or re-registers) a session. Returns its poll token.
    pub fn register_session(&self, session_id: &str, codex_pid: u32) -> String {
        let mut state = self.lock();
        let notify = state
            .sessions
            .get(session_id)
            .map(|session| Arc::clone(&session.notify))
            .unwrap_or_default();
        let existing_turns = state
            .sessions
            .get(session_id)
            .map(|session| session.turns.clone())
            .unwrap_or_default();
        state.sessions.insert(
            session_id.to_string(),
            SessionChannel {
                codex_pid,
                last_seen: Instant::now(),
                next_seq: 0,
                pending: VecDeque::new(),
                notify,
                turns: existing_turns,
            },
        );
        random_id("sess_", 18)
    }

    pub fn heartbeat(&self, session_id: &str) -> Result<(), UnknownSession> {
        let mut state = self.lock();
        let session = state.sessions.get_mut(session_id).ok_or(UnknownSession)?;
        session.last_seen = Instant::now();
        Ok(())
    }

    pub fn session_pid(&self, session_id: &str) -> Option<u32> {
        self.lock()
            .sessions
            .get(session_id)
            .map(|session| session.codex_pid)
    }

    /// Drops a session and revokes every turn it owned.
    pub fn remove_session(&self, session_id: &str, reason: &str) -> bool {
        let mut state = self.lock();
        let Some(session) = state.sessions.remove(session_id) else {
            return false;
        };
        session.notify.notify_one();
        for token in session.turns {
            Self::revoke_locked(&mut state, &token, reason, self.config.retire_cap);
        }
        true
    }

    pub fn register_turn(&self, registration: TurnRegistration) -> Result<(), RegisterTurnError> {
        let mut state = self.lock();
        if state.turns.contains_key(&registration.turn_token)
            || state.retired.contains_key(&registration.turn_token)
        {
            return Err(RegisterTurnError::Duplicate);
        }
        let session = state
            .sessions
            .get_mut(&registration.session_id)
            .ok_or(RegisterTurnError::UnknownSession)?;
        session.turns.insert(registration.turn_token.clone());
        session.last_seen = Instant::now();
        state.turns.insert(
            registration.turn_token.clone(),
            TurnChannel {
                session_id: registration.session_id,
                trace: registration.trace,
                tools: registration.tools,
                exec_tool: registration.exec_tool,
                apply_patch: registration.apply_patch,
                binding: None,
                expires_at: Instant::now() + registration.ttl,
                queued: Vec::new(),
                batch_armed: false,
                in_flight: HashSet::new(),
            },
        );
        Ok(())
    }

    /// Resolves a `turn_token`; the first claim mints the binding.
    pub fn claim(&self, turn_token: &str) -> Result<Claim, ClaimError> {
        let mut state = self.lock();
        if let Some(retired) = state.retired.get(turn_token) {
            return Err(ClaimError::Retired {
                trace: retired.trace.clone(),
                reason: retired.reason.clone(),
            });
        }
        let sessions_alive: HashSet<String> = state.sessions.keys().cloned().collect();
        let turn = state.turns.get_mut(turn_token).ok_or(ClaimError::Unknown)?;
        if Instant::now() >= turn.expires_at {
            return Err(ClaimError::Expired);
        }
        if !sessions_alive.contains(&turn.session_id) {
            return Err(ClaimError::SessionGone);
        }
        let binding = match &turn.binding {
            Some(binding) => binding.clone(),
            None => {
                let binding = random_id("bind_", 12);
                turn.binding = Some(binding.clone());
                binding
            }
        };
        let tools = TurnTools {
            tools: Arc::clone(&turn.tools),
            exec_tool: turn.exec_tool,
            apply_patch: turn.apply_patch,
            exec_default_yield_ms: self.config.exec_default_yield_ms,
        };
        state
            .bindings
            .insert(binding.clone(), turn_token.to_string());
        Ok(Claim {
            binding,
            turn_token: turn_token.to_string(),
            tools,
        })
    }

    /// Queues a call for the owning session and waits for its result.
    pub async fn invoke(self: &Arc<Self>, binding: &str, target: CallTarget) -> BrokerResult {
        let display = target.display_name();
        let (rx, wait, call_id) = {
            let mut state = self.lock();
            let Some(token) = state.bindings.get(binding).cloned() else {
                return BrokerResult::error(ClaimError::Unknown.to_string());
            };
            let Some(turn) = state.turns.get_mut(&token) else {
                let message = match state.retired.get(&token) {
                    Some(retired) => ClaimError::Retired {
                        trace: retired.trace.clone(),
                        reason: retired.reason.clone(),
                    }
                    .to_string(),
                    None => ClaimError::Unknown.to_string(),
                };
                return BrokerResult::error(message);
            };
            let now = Instant::now();
            if now >= turn.expires_at {
                return BrokerResult::error(ClaimError::Expired.to_string());
            }
            let wait = self
                .config
                .call_timeout
                .min(turn.expires_at.saturating_duration_since(now));
            let call_id = random_id("call_", 24);
            let (tx, rx) = oneshot::channel();
            turn.queued.push(PendingCallWire {
                call_id: call_id.clone(),
                target,
                deadline_ms: unix_ms(SystemTime::now() + wait),
            });
            let session_id = turn.session_id.clone();
            let arm = !turn.batch_armed;
            turn.batch_armed = true;
            state.calls.insert(
                call_id.clone(),
                CallSlot {
                    turn_token: token.clone(),
                    session_id,
                    tx,
                },
            );
            if arm {
                let broker = Arc::clone(self);
                let window = self.config.batch_window;
                tokio::spawn(async move {
                    tokio::time::sleep(window).await;
                    broker.flush(&token);
                });
            }
            (rx, wait, call_id)
        };

        match tokio::time::timeout(wait, rx).await {
            Ok(Ok(result)) => result,
            // The turn was revoked without answering: the sender was dropped.
            Ok(Err(_)) => BrokerResult::error(format!(
                "Codex did not answer {display}: the turn ended before the tool finished."
            )),
            Err(_) => {
                // Forget the call so a late result is rejected rather than
                // completing nothing.
                let mut state = self.lock();
                if let Some(slot) = state.calls.remove(&call_id) {
                    if let Some(turn) = state.turns.get_mut(&slot.turn_token) {
                        turn.in_flight.remove(&call_id);
                        turn.queued.retain(|call| call.call_id != call_id);
                    }
                }
                BrokerResult::error(format!(
                    "Codex did not finish {display} within {}s. For long commands use yield_time_ms ≤ {} and poll the session with codex_write_stdin.",
                    wait.as_secs(),
                    crate::chatgpt_web::connector::contract::MAX_YIELD_TIME_MS
                ))
            }
        }
    }

    /// Moves a turn's queued calls into one batch on its session's poll queue.
    fn flush(&self, turn_token: &str) {
        let mut state = self.lock();
        let Some(turn) = state.turns.get_mut(turn_token) else {
            return;
        };
        turn.batch_armed = false;
        if turn.queued.is_empty() {
            return;
        }
        let calls = std::mem::take(&mut turn.queued);
        for call in &calls {
            turn.in_flight.insert(call.call_id.clone());
        }
        let session_id = turn.session_id.clone();
        let Some(session) = state.sessions.get_mut(&session_id) else {
            // The session died between enqueue and flush; the sweep revokes.
            return;
        };
        session.next_seq += 1;
        session.pending.push_back(CallBatchWire {
            seq: session.next_seq,
            turn_token: turn_token.to_string(),
            calls,
        });
        session.notify.notify_one();
    }

    /// Long-poll: acknowledges everything up to `after`, then returns the
    /// outstanding batches, waiting up to `wait` for one to appear.
    pub async fn next_batches(
        &self,
        session_id: &str,
        after: u64,
        wait: Duration,
    ) -> Result<CallsResponse, UnknownSession> {
        let notify = {
            let mut state = self.lock();
            let session = state.sessions.get_mut(session_id).ok_or(UnknownSession)?;
            session.last_seen = Instant::now();
            session.pending.retain(|batch| batch.seq > after);
            if !session.pending.is_empty() {
                return Ok(CallsResponse {
                    seq: session.next_seq,
                    batches: session.pending.iter().cloned().collect(),
                });
            }
            Arc::clone(&session.notify)
        };
        let _ = tokio::time::timeout(wait, notify.notified()).await;
        let mut state = self.lock();
        let session = state.sessions.get_mut(session_id).ok_or(UnknownSession)?;
        session.last_seen = Instant::now();
        Ok(CallsResponse {
            seq: session.next_seq,
            batches: session.pending.iter().cloned().collect(),
        })
    }

    /// Delivers a session's result to the waiting MCP call.
    pub fn complete(
        &self,
        session_id: &str,
        call_id: &str,
        result: BrokerResult,
    ) -> Result<(), CompleteError> {
        let mut state = self.lock();
        let Some(slot) = state.calls.get(call_id) else {
            return Err(CompleteError::UnknownCall);
        };
        if slot.session_id != session_id {
            return Err(CompleteError::WrongSession);
        }
        let token = slot.turn_token.clone();
        let in_flight = state
            .turns
            .get(&token)
            .is_some_and(|turn| turn.in_flight.contains(call_id));
        if !in_flight {
            return Err(CompleteError::NotInFlight);
        }
        if let Some(slot) = state.calls.remove(call_id) {
            let _ = slot.tx.send(result);
        }
        if let Some(turn) = state.turns.get_mut(&token) {
            turn.in_flight.remove(call_id);
        }
        // Drop the call from any unacked batch so a redelivery does not run it
        // twice.
        if let Some(session) = state.sessions.get_mut(session_id) {
            for batch in session.pending.iter_mut() {
                batch.calls.retain(|call| call.call_id != call_id);
            }
            session.pending.retain(|batch| !batch.calls.is_empty());
        }
        Ok(())
    }

    /// Ends a turn: pending calls fail with `reason`, the token is retired.
    pub fn revoke(&self, turn_token: &str, reason: &str) -> bool {
        let mut state = self.lock();
        Self::revoke_locked(&mut state, turn_token, reason, self.config.retire_cap)
    }

    fn revoke_locked(
        state: &mut BrokerState,
        turn_token: &str,
        reason: &str,
        retire_cap: usize,
    ) -> bool {
        let Some(turn) = state.turns.remove(turn_token) else {
            return false;
        };
        if let Some(binding) = &turn.binding {
            state.bindings.remove(binding);
        }
        if let Some(session) = state.sessions.get_mut(&turn.session_id) {
            session.turns.remove(turn_token);
            session
                .pending
                .retain(|batch| batch.turn_token != turn_token);
        }
        let mut affected: Vec<String> = turn.in_flight.into_iter().collect();
        affected.extend(turn.queued.into_iter().map(|call| call.call_id));
        for call_id in affected {
            if let Some(slot) = state.calls.remove(&call_id) {
                let _ = slot.tx.send(BrokerResult::error(reason.to_string()));
            }
        }
        state.retired.insert(
            turn_token.to_string(),
            RetiredTurn {
                trace: turn.trace,
                reason: reason.to_string(),
            },
        );
        state.retired_order.push_back(turn_token.to_string());
        while state.retired_order.len() > retire_cap {
            if let Some(old) = state.retired_order.pop_front() {
                state.retired.remove(&old);
            }
        }
        true
    }

    /// Whether the turn still has calls the session has not answered.
    pub fn has_in_flight(&self, turn_token: &str) -> bool {
        self.lock()
            .turns
            .get(turn_token)
            .is_some_and(|turn| !turn.in_flight.is_empty() || !turn.queued.is_empty())
    }

    /// Expires overdue turns and drops sessions that stopped heartbeating.
    pub fn sweep(&self) {
        let now = Instant::now();
        let mut state = self.lock();
        let expired: Vec<String> = state
            .turns
            .iter()
            .filter(|(_, turn)| now >= turn.expires_at)
            .map(|(token, _)| token.clone())
            .collect();
        for token in expired {
            Self::revoke_locked(
                &mut state,
                &token,
                "the Codex turn expired",
                self.config.retire_cap,
            );
        }
        let dead: Vec<String> = state
            .sessions
            .iter()
            .filter(|(_, session)| {
                now.duration_since(session.last_seen) > self.config.heartbeat_timeout
            })
            .map(|(id, _)| id.clone())
            .collect();
        for session_id in dead {
            if let Some(session) = state.sessions.remove(&session_id) {
                session.notify.notify_one();
                for token in session.turns {
                    Self::revoke_locked(
                        &mut state,
                        &token,
                        "Codex session disconnected",
                        self.config.retire_cap,
                    );
                }
            }
        }
    }

    /// Runs `sweep` every `interval` until `cancel` fires.
    pub fn spawn_sweeper(
        self: &Arc<Self>,
        interval: Duration,
        cancel: tokio_util::sync::CancellationToken,
    ) -> tokio::task::JoinHandle<()> {
        let broker = Arc::clone(self);
        tokio::spawn(async move {
            loop {
                tokio::select! {
                    _ = cancel.cancelled() => break,
                    _ = tokio::time::sleep(interval) => broker.sweep(),
                }
            }
        })
    }
}

#[cfg(test)]
#[path = "broker_tests.rs"]
mod tests;
