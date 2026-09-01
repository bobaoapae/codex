//! Runtime-only observability for interactive unified-exec processes.
//!
//! This state is deliberately separate from the process transport and from
//! rollout history.  A process may be quiet for minutes without producing a
//! model-visible event; its bounded snapshot still needs to be useful to a
//! coordinator while remaining safe to expose.  The store therefore keeps
//! only redacted command/output metadata and publishes a small watch revision
//! for local waiters.  It never appends heartbeat events to a rollout.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::Weak;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;
use std::time::SystemTime;
use std::time::UNIX_EPOCH;

use serde::Deserialize;
use serde::Serialize;
use tokio::sync::Semaphore;
use tokio::sync::watch;

use super::terminal_redaction::redact_and_truncate;
use crate::session::session::Session;

/// Maximum bytes retained for a redacted command summary.
pub(crate) const MAX_COMMAND_SUMMARY_BYTES: usize = 1_024;
/// Maximum bytes retained for a redacted output preview.
pub(crate) const MAX_OUTPUT_PREVIEW_BYTES: usize = 512;
/// Maximum completed/cancelled snapshots retained for one session.
pub(crate) const MAX_RETAINED_TERMINAL_OBSERVATIONS: usize = 128;
/// Default quiet period after which a live process is marked as needing attention.
pub(crate) const DEFAULT_INACTIVITY_THRESHOLD_MS: u64 = 60_000;

/// Clock used by terminal observability.  The production clock is wall-clock
/// Unix milliseconds; tests can inject a deterministic implementation without
/// sleeping or changing the process environment.
pub(crate) trait TerminalClock: Send + Sync {
    fn now_ms(&self) -> u64;
}

#[derive(Debug, Default)]
pub(crate) struct SystemTerminalClock;

impl TerminalClock for SystemTerminalClock {
    fn now_ms(&self) -> u64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|duration| duration.as_millis().try_into().unwrap_or(u64::MAX))
            .unwrap_or_default()
    }
}

/// Safe state of a process from the coordinator's point of view.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) enum TerminalProcessState {
    Running,
    Waiting,
    NeedsAttention,
    Exited,
    Failed,
    Cancelled,
}

impl TerminalProcessState {
    pub(crate) fn is_terminal(self) -> bool {
        matches!(self, Self::Exited | Self::Failed | Self::Cancelled)
    }

    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Running => "running",
            Self::Waiting => "waiting",
            Self::NeedsAttention => "needsAttention",
            Self::Exited => "exited",
            Self::Failed => "failed",
            Self::Cancelled => "cancelled",
        }
    }
}

/// Bounded, redacted runtime view of one unified-exec process.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct TerminalProcessSnapshot {
    pub(crate) session_id: String,
    pub(crate) pid: i32,
    pub(crate) command: String,
    pub(crate) started_at: u64,
    pub(crate) elapsed_ms: u64,
    pub(crate) last_activity_at: u64,
    pub(crate) last_output_at: Option<u64>,
    pub(crate) last_output_preview: Option<String>,
    pub(crate) last_output_bytes: u64,
    pub(crate) output_bytes: u64,
    pub(crate) state: TerminalProcessState,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) struct ObservationChange {
    pub(crate) state_changed: bool,
    pub(crate) entered_needs_attention: bool,
    pub(crate) cleared_needs_attention: bool,
}

#[derive(Debug)]
struct TerminalObservation {
    session: Weak<Session>,
    session_id: String,
    pid: i32,
    call_id: String,
    command: String,
    started_at: u64,
    last_activity_at: u64,
    last_output_at: Option<u64>,
    last_output_preview: Option<String>,
    last_output_bytes: u64,
    output_bytes: u64,
    state: TerminalProcessState,
    final_receipt_emitted: bool,
}

impl TerminalObservation {
    fn snapshot(&self, now_ms: u64) -> TerminalProcessSnapshot {
        TerminalProcessSnapshot {
            session_id: self.session_id.clone(),
            pid: self.pid,
            command: self.command.clone(),
            started_at: self.started_at,
            elapsed_ms: now_ms.saturating_sub(self.started_at),
            last_activity_at: self.last_activity_at,
            last_output_at: self.last_output_at,
            last_output_preview: self.last_output_preview.clone(),
            last_output_bytes: self.last_output_bytes,
            output_bytes: self.output_bytes,
            state: self.state,
        }
    }
}

/// In-memory terminal observation state and its local watch revision.
pub(crate) struct TerminalObservabilityStore {
    entries: Mutex<HashMap<i32, TerminalObservation>>,
    clock: Arc<dyn TerminalClock>,
    inactivity_threshold_ms: u64,
    revision: AtomicU64,
    revision_tx: watch::Sender<u64>,
    persistence_lock: Semaphore,
}

impl std::fmt::Debug for TerminalObservabilityStore {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("TerminalObservabilityStore")
            .field("inactivity_threshold_ms", &self.inactivity_threshold_ms)
            .field("revision", &self.revision.load(Ordering::Acquire))
            .finish_non_exhaustive()
    }
}

impl TerminalObservabilityStore {
    pub(crate) fn new(clock: Arc<dyn TerminalClock>, inactivity_threshold_ms: u64) -> Arc<Self> {
        let (revision_tx, _revision_rx) = watch::channel(0_u64);
        Arc::new(Self {
            entries: Mutex::default(),
            clock,
            inactivity_threshold_ms,
            revision: AtomicU64::default(),
            revision_tx,
            persistence_lock: Semaphore::new(1),
        })
    }

    pub(crate) fn system(inactivity_threshold_ms: u64) -> Arc<Self> {
        Self::new(Arc::new(SystemTerminalClock), inactivity_threshold_ms)
    }

    pub(crate) fn now_ms(&self) -> u64 {
        self.clock.now_ms()
    }

    pub(crate) async fn wait_for_persistence(&self) {
        let _permit = self.persistence_lock.acquire().await;
    }

    pub(crate) async fn acquire_persistence(
        &self,
    ) -> Result<tokio::sync::SemaphorePermit<'_>, tokio::sync::AcquireError> {
        self.persistence_lock.acquire().await
    }

    pub(crate) fn inactivity_threshold_ms(&self) -> u64 {
        self.inactivity_threshold_ms
    }

    pub(crate) fn current_revision(&self) -> u64 {
        self.revision.load(Ordering::Acquire)
    }

    pub(crate) fn subscribe_revision(&self) -> watch::Receiver<u64> {
        self.revision_tx.subscribe()
    }

    pub(crate) fn register(
        &self,
        session: Weak<Session>,
        session_id: String,
        pid: i32,
        call_id: String,
        command: &str,
        started_at: Option<u64>,
    ) -> TerminalProcessSnapshot {
        let started_at = started_at.unwrap_or_else(|| self.now_ms());
        let observation = TerminalObservation {
            session,
            session_id,
            pid,
            call_id,
            command: redact_and_truncate(command, MAX_COMMAND_SUMMARY_BYTES),
            started_at,
            last_activity_at: started_at,
            last_output_at: None,
            last_output_preview: None,
            last_output_bytes: 0,
            output_bytes: 0,
            state: TerminalProcessState::Running,
            final_receipt_emitted: false,
        };
        let snapshot = observation.snapshot(started_at);
        let mut entries = self
            .entries
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        entries.insert(pid, observation);
        if entries.len() > MAX_RETAINED_TERMINAL_OBSERVATIONS
            && let Some(oldest_pid) = entries
                .iter()
                .filter(|(_, entry)| entry.state.is_terminal())
                .min_by_key(|(_, entry)| entry.started_at)
                .map(|(pid, _)| *pid)
        {
            entries.remove(&oldest_pid);
        }
        drop(entries);
        self.publish_revision();
        snapshot
    }

    pub(crate) fn pid_for_call_id(&self, call_id: &str) -> Option<i32> {
        self.entries
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .values()
            .find(|entry| entry.call_id == call_id)
            .map(|entry| entry.pid)
    }

    pub(crate) fn session_for_pid(&self, pid: i32) -> Option<Arc<Session>> {
        self.entries
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(&pid)
            .and_then(|entry| entry.session.upgrade())
    }

    pub(crate) fn mark_output(
        &self,
        pid: i32,
        bytes: &[u8],
        at_ms: Option<u64>,
    ) -> Option<ObservationChange> {
        let now_ms = at_ms.unwrap_or_else(|| self.now_ms());
        let mut entries = self
            .entries
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let entry = entries.get_mut(&pid)?;
        if entry.state.is_terminal() {
            return None;
        }
        let old_state = entry.state;
        entry.last_activity_at = now_ms;
        entry.last_output_at = Some(now_ms);
        entry.last_output_bytes = bytes.len().try_into().unwrap_or(u64::MAX);
        entry.output_bytes = entry.output_bytes.saturating_add(entry.last_output_bytes);
        entry.last_output_preview = Some(redact_and_truncate(
            &String::from_utf8_lossy(bytes),
            MAX_OUTPUT_PREVIEW_BYTES,
        ));
        if matches!(
            entry.state,
            TerminalProcessState::Waiting | TerminalProcessState::NeedsAttention
        ) {
            entry.state = TerminalProcessState::Running;
        }
        let change = ObservationChange {
            state_changed: old_state != entry.state,
            entered_needs_attention: false,
            cleared_needs_attention: old_state == TerminalProcessState::NeedsAttention,
        };
        drop(entries);
        self.publish_revision();
        Some(change)
    }

    pub(crate) fn mark_write(
        &self,
        pid: i32,
        input: &str,
        at_ms: Option<u64>,
    ) -> Option<ObservationChange> {
        let now_ms = at_ms.unwrap_or_else(|| self.now_ms());
        let mut entries = self
            .entries
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let entry = entries.get_mut(&pid)?;
        if entry.state.is_terminal() {
            return None;
        }
        let old_state = entry.state;
        entry.last_activity_at = now_ms;
        entry.state = if input.is_empty() {
            TerminalProcessState::Waiting
        } else {
            TerminalProcessState::Running
        };
        let change = ObservationChange {
            state_changed: old_state != entry.state,
            entered_needs_attention: false,
            cleared_needs_attention: old_state == TerminalProcessState::NeedsAttention,
        };
        drop(entries);
        self.publish_revision();
        Some(change)
    }

    pub(crate) fn mark_state(
        &self,
        pid: i32,
        state: TerminalProcessState,
        at_ms: Option<u64>,
    ) -> Option<ObservationChange> {
        let now_ms = at_ms.unwrap_or_else(|| self.now_ms());
        let mut entries = self
            .entries
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let entry = entries.get_mut(&pid)?;
        if entry.state == state {
            return Some(ObservationChange::default());
        }
        let old_state = entry.state;
        entry.state = state;
        if state != TerminalProcessState::NeedsAttention {
            entry.last_activity_at = now_ms;
        }
        let change = ObservationChange {
            state_changed: true,
            entered_needs_attention: state == TerminalProcessState::NeedsAttention,
            cleared_needs_attention: old_state == TerminalProcessState::NeedsAttention,
        };
        drop(entries);
        self.publish_revision();
        Some(change)
    }

    /// Run one deterministic heartbeat.  No timer or rollout write is needed
    /// by callers; production uses the manager's periodic task and tests can
    /// pass an injected timestamp directly.
    pub(crate) fn heartbeat(
        &self,
        pid: i32,
        process_alive: bool,
        at_ms: Option<u64>,
    ) -> Option<ObservationChange> {
        let now_ms = at_ms.unwrap_or_else(|| self.now_ms());
        let mut entries = self
            .entries
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let entry = entries.get_mut(&pid)?;
        if !process_alive {
            let state = if entry.state == TerminalProcessState::Cancelled {
                TerminalProcessState::Cancelled
            } else {
                TerminalProcessState::Exited
            };
            if entry.state == state {
                return Some(ObservationChange::default());
            }
            entry.state = state;
            entry.last_activity_at = now_ms;
            drop(entries);
            self.publish_revision();
            return Some(ObservationChange {
                state_changed: true,
                entered_needs_attention: false,
                cleared_needs_attention: false,
            });
        }
        if entry.state.is_terminal()
            || entry.state == TerminalProcessState::NeedsAttention
            || now_ms.saturating_sub(entry.last_activity_at) < self.inactivity_threshold_ms
        {
            return Some(ObservationChange::default());
        }
        entry.state = TerminalProcessState::NeedsAttention;
        drop(entries);
        self.publish_revision();
        Some(ObservationChange {
            state_changed: true,
            entered_needs_attention: true,
            cleared_needs_attention: false,
        })
    }

    pub(crate) fn mark_final_receipt(&self, pid: i32) -> bool {
        let mut entries = self
            .entries
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let Some(entry) = entries.get_mut(&pid) else {
            return false;
        };
        if entry.final_receipt_emitted {
            return false;
        }
        entry.final_receipt_emitted = true;
        drop(entries);
        self.publish_revision();
        true
    }

    pub(crate) fn final_receipt_emitted(&self, pid: i32) -> bool {
        self.entries
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(&pid)
            .is_some_and(|entry| entry.final_receipt_emitted)
    }

    pub(crate) fn snapshot(&self, pid: i32, at_ms: Option<u64>) -> Option<TerminalProcessSnapshot> {
        let now_ms = at_ms.unwrap_or_else(|| self.now_ms());
        self.entries
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(&pid)
            .map(|entry| entry.snapshot(now_ms))
    }

    pub(crate) fn snapshots(&self, at_ms: Option<u64>) -> Vec<TerminalProcessSnapshot> {
        let now_ms = at_ms.unwrap_or_else(|| self.now_ms());
        let mut snapshots = self
            .entries
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .values()
            .map(|entry| entry.snapshot(now_ms))
            .collect::<Vec<_>>();
        snapshots.sort_by_key(|snapshot| snapshot.pid);
        snapshots
    }

    pub(crate) fn remove(&self, pid: i32) -> Option<TerminalProcessSnapshot> {
        let now_ms = self.now_ms();
        let removed = self
            .entries
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(&pid)
            .map(|entry| entry.snapshot(now_ms));
        if removed.is_some() {
            self.publish_revision();
        }
        removed
    }

    pub(crate) fn clear(&self) {
        let removed = {
            let mut entries = self
                .entries
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let removed = !entries.is_empty();
            entries.clear();
            removed
        };
        if removed {
            self.publish_revision();
        }
    }

    fn publish_revision(&self) {
        let revision = self.revision.fetch_add(1, Ordering::AcqRel) + 1;
        self.revision_tx.send_replace(revision);
    }
}

#[cfg(test)]
#[path = "terminal_observability_tests.rs"]
mod tests;
