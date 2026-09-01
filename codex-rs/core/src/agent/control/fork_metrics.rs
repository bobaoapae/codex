//! Runtime fork timing and cache accounting.
//!
//! The tracker owns only opaque IDs and bounded counters. The workflow store
//! receives the same projection for restart/inspect use; raw prompts and
//! response contents never cross this module's persistence boundary.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::Mutex;
use std::time::SystemTime;
use std::time::UNIX_EPOCH;

use codex_history::RolloutItem;
use codex_protocol::ThreadId;
use codex_protocol::models::ResponseItem;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::TokenUsage;
use codex_state::WorkflowForkContextEntry;
use codex_state::WorkflowForkContextOrigin;
use codex_state::WorkflowForkMetricsCreate;
use codex_state::WorkflowForkTurns;
use codex_state::WorkflowStore;
use serde::Serialize;
use tracing::debug;
use tracing::warn;
use uuid::Uuid;

/// A full-history fork is considered close to the compaction limit at this
/// fraction. This only controls a warning; it never requests compaction.
pub(crate) const FULL_HISTORY_FORK_WARNING_NUMERATOR: i64 = 9;
pub(crate) const FULL_HISTORY_FORK_WARNING_DENOMINATOR: i64 = 10;
const MAX_PROJECTED_ITEM_BYTES: usize = 16 * 1024 * 1024;
const MAX_RUNTIME_FORKS: usize = 256;

pub(crate) trait ForkMetricsClock: Send + Sync {
    fn now_ms(&self) -> i64;
}

#[derive(Debug, Default)]
pub(crate) struct SystemForkMetricsClock;

impl ForkMetricsClock for SystemForkMetricsClock {
    fn now_ms(&self) -> i64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|duration| i64::try_from(duration.as_millis()).unwrap_or(i64::MAX))
            .unwrap_or_default()
    }
}

#[derive(Debug)]
struct RuntimeFork {
    requested_at_ms: i64,
    child_thread_id: Option<ThreadId>,
    next_sequence: i64,
    first_event_recorded: bool,
    first_response_recorded: bool,
    completion_recorded: bool,
    warning_recorded: bool,
}

#[derive(Debug, Default)]
struct RuntimeState {
    forks: HashMap<String, RuntimeFork>,
    child_to_fork: HashMap<ThreadId, String>,
}

/// In-process causal fork metrics state shared by all cloned agent controls.
pub(crate) struct ForkMetricsTracker {
    state: Mutex<RuntimeState>,
    clock: Arc<dyn ForkMetricsClock>,
}

impl std::fmt::Debug for ForkMetricsTracker {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ForkMetricsTracker")
            .finish_non_exhaustive()
    }
}

impl Default for ForkMetricsTracker {
    fn default() -> Self {
        Self {
            state: Mutex::default(),
            clock: Arc::new(SystemForkMetricsClock),
        }
    }
}

impl ForkMetricsTracker {
    #[cfg(test)]
    pub(crate) fn with_clock(clock: Arc<dyn ForkMetricsClock>) -> Arc<Self> {
        Arc::new(Self {
            state: Mutex::default(),
            clock,
        })
    }

    pub(crate) fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    pub(crate) async fn spawn_requested(
        &self,
        workflow: Option<WorkflowStore>,
        parent_thread_id: ThreadId,
        spawn_call_id: &str,
        fork_turns: WorkflowForkTurns,
    ) -> String {
        let fork_id = Uuid::new_v4().to_string();
        let now_ms = self.now_ms();
        {
            let mut state = self
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            state.forks.insert(
                fork_id.clone(),
                RuntimeFork {
                    requested_at_ms: now_ms,
                    child_thread_id: None,
                    next_sequence: 0,
                    first_event_recorded: false,
                    first_response_recorded: false,
                    completion_recorded: false,
                    warning_recorded: false,
                },
            );
            if state.forks.len() > MAX_RUNTIME_FORKS {
                let mut evictable = state
                    .forks
                    .iter()
                    .filter_map(|(fork_id, runtime)| {
                        runtime
                            .completion_recorded
                            .then_some((fork_id.clone(), runtime.requested_at_ms))
                    })
                    .collect::<Vec<_>>();
                evictable.sort_by_key(|(_, requested_at_ms)| *requested_at_ms);
                let mut completed_ids = evictable
                    .into_iter()
                    .map(|(fork_id, _)| fork_id)
                    .take(state.forks.len().saturating_sub(MAX_RUNTIME_FORKS))
                    .collect::<Vec<_>>();
                if completed_ids.len() < state.forks.len().saturating_sub(MAX_RUNTIME_FORKS) {
                    let missing = state
                        .forks
                        .keys()
                        .filter(|fork_id| !completed_ids.iter().any(|id| id == *fork_id))
                        .take(
                            state
                                .forks
                                .len()
                                .saturating_sub(MAX_RUNTIME_FORKS + completed_ids.len()),
                        )
                        .cloned()
                        .collect::<Vec<_>>();
                    completed_ids.extend(missing);
                }
                for completed_id in completed_ids {
                    state.forks.remove(&completed_id);
                    state
                        .child_to_fork
                        .retain(|_, mapped_fork_id| mapped_fork_id != &completed_id);
                }
            }
        }
        if let Some(workflow) = workflow {
            let request = WorkflowForkMetricsCreate {
                fork_id: fork_id.clone(),
                spawn_call_id: spawn_call_id.to_string(),
                parent_thread_id: parent_thread_id.to_string(),
                fork_turns,
                spawn_requested_at_ms: now_ms,
                projected_fork_bytes: 0,
                projected_fork_tokens: 0,
                context_entries: Vec::new(),
            };
            if let Err(error) = workflow.create_fork_metrics(&request).await {
                debug!(%error, fork_id = %fork_id, "failed to persist fork spawn request");
            }
        }
        fork_id
    }

    pub(crate) async fn update_projection(
        &self,
        workflow: Option<WorkflowStore>,
        fork_id: &str,
        projected_fork_bytes: i64,
        projected_fork_tokens: i64,
        context_entries: Vec<WorkflowForkContextEntry>,
    ) {
        let next_sequence = context_entries
            .iter()
            .map(|entry| entry.sequence)
            .max()
            .map_or(0, |sequence| sequence.saturating_add(1));
        {
            let mut state = self
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if let Some(runtime) = state.forks.get_mut(fork_id) {
                runtime.next_sequence = next_sequence;
            }
        }
        let Some(workflow) = workflow else {
            return;
        };
        if let Err(error) = workflow
            .update_fork_projection(
                fork_id,
                projected_fork_bytes,
                projected_fork_tokens,
                &context_entries,
                self.now_ms(),
            )
            .await
        {
            debug!(%error, fork_id, "failed to persist fork projection");
        }
    }

    pub(crate) async fn child_created(
        &self,
        workflow: Option<WorkflowStore>,
        fork_id: &str,
        child_thread_id: ThreadId,
    ) {
        let at_ms = self.now_ms();
        {
            let mut state = self
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if let Some(runtime) = state.forks.get_mut(fork_id) {
                runtime.child_thread_id = Some(child_thread_id);
                state
                    .child_to_fork
                    .insert(child_thread_id, fork_id.to_string());
            } else {
                return;
            }
        }
        if let Some(workflow) = workflow
            && let Err(error) = workflow
                .mark_fork_child_created(fork_id, &child_thread_id.to_string(), at_ms)
                .await
        {
            debug!(%error, fork_id, "failed to persist fork child creation");
        }
    }

    /// Observe one child event. Since the child mapping is installed after
    /// initial history is persisted, inherited rollout items cannot be counted
    /// as `firstNewResponse` or provider usage.
    pub(crate) async fn observe_event(
        &self,
        workflow: Option<WorkflowStore>,
        child_thread_id: ThreadId,
        event: &EventMsg,
    ) {
        let at_ms = self.now_ms();
        let response = is_new_response(event);
        let completed = matches!(event, EventMsg::TurnComplete(_) | EventMsg::TurnAborted(_));
        let (fork_id, first_event, first_response, completed_now, sequence) = {
            let mut state = self
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let Some(fork_id) = state.child_to_fork.get(&child_thread_id).cloned() else {
                return;
            };
            let Some(runtime) = state.forks.get_mut(&fork_id) else {
                return;
            };
            let first_event = !runtime.first_event_recorded;
            let first_response = response && !runtime.first_response_recorded;
            let completed_now = completed && !runtime.completion_recorded;
            runtime.first_event_recorded |= first_event;
            runtime.first_response_recorded |= first_response;
            runtime.completion_recorded |= completed_now;
            let sequence = runtime.next_sequence;
            if response {
                runtime.next_sequence = runtime.next_sequence.saturating_add(1);
            }
            (
                fork_id,
                first_event,
                first_response,
                completed_now,
                sequence,
            )
        };
        let Some(workflow) = workflow else {
            return;
        };
        if first_event {
            let _ = workflow.mark_fork_first_event(&fork_id, at_ms).await;
        }
        if response {
            if first_response {
                let _ = workflow.mark_fork_first_new_response(&fork_id, at_ms).await;
            }
            let (byte_count, token_count) = serialized_size(event);
            let entry = WorkflowForkContextEntry {
                fork_id: fork_id.clone(),
                sequence,
                origin: WorkflowForkContextOrigin::NewOutput,
                byte_count,
                token_count,
            };
            if let Err(error) = workflow.append_fork_context_entry(&entry).await {
                debug!(%error, fork_id = %fork_id, "failed to persist fork response projection");
            }
        }
        if completed_now {
            let _ = workflow.mark_fork_completed(&fork_id, at_ms).await;
        }
    }

    pub(crate) async fn observe_usage(
        &self,
        workflow: Option<WorkflowStore>,
        child_thread_id: ThreadId,
        usage: &TokenUsage,
    ) {
        let Some(workflow) = workflow else {
            return;
        };
        let fork_id = {
            let state = self
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            state.child_to_fork.get(&child_thread_id).cloned()
        };
        let Some(fork_id) = fork_id else {
            return;
        };
        let input_tokens = usage.input_tokens.max(0);
        let cached_input_tokens = usage.cached_input_tokens.max(0);
        let cache_write_input_tokens = usage.cache_write_input_tokens.max(0);
        if let Err(error) = workflow
            .add_fork_provider_usage(
                &fork_id,
                input_tokens,
                cached_input_tokens,
                cache_write_input_tokens,
                self.now_ms(),
            )
            .await
        {
            debug!(%error, fork_id = %fork_id, "failed to persist fork provider usage");
        }
    }

    pub(crate) async fn claim_compaction_warning(
        &self,
        workflow: Option<WorkflowStore>,
        fork_id: &str,
        projected_tokens: i64,
        limit_tokens: i64,
    ) -> bool {
        if limit_tokens <= 0
            || projected_tokens.saturating_mul(FULL_HISTORY_FORK_WARNING_DENOMINATOR)
                < limit_tokens.saturating_mul(FULL_HISTORY_FORK_WARNING_NUMERATOR)
        {
            return false;
        }
        let claimed_in_memory = {
            let mut state = self
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let Some(runtime) = state.forks.get_mut(fork_id) else {
                return false;
            };
            if runtime.warning_recorded {
                false
            } else {
                runtime.warning_recorded = true;
                true
            }
        };
        if !claimed_in_memory {
            return false;
        }
        let Some(workflow) = workflow else {
            return true;
        };
        match workflow
            .claim_fork_compaction_warning(fork_id, projected_tokens, limit_tokens, self.now_ms())
            .await
        {
            Ok(claimed) => claimed,
            Err(error) => {
                warn!(%error, fork_id, "failed to persist fork compaction warning claim");
                true
            }
        }
    }

    fn now_ms(&self) -> i64 {
        self.clock.now_ms().max(0)
    }
}

/// Build the bounded context projection for the normalized fork history.
pub(crate) fn project_rollout_context(
    fork_id: &str,
    items: &[RolloutItem],
) -> (i64, i64, Vec<WorkflowForkContextEntry>) {
    let mut projected_bytes = 0_i64;
    let mut entries = Vec::with_capacity(items.len().min(codex_state::MAX_FORK_CONTEXT_ENTRIES));
    for (sequence, item) in items.iter().enumerate() {
        let (byte_count, token_count) = serialized_size(item);
        projected_bytes = projected_bytes.saturating_add(byte_count);
        if sequence < codex_state::MAX_FORK_CONTEXT_ENTRIES {
            entries.push(WorkflowForkContextEntry {
                fork_id: fork_id.to_string(),
                sequence: i64::try_from(sequence).unwrap_or(i64::MAX),
                origin: WorkflowForkContextOrigin::InheritedHistory,
                byte_count,
                token_count,
            });
        }
    }
    let projected_tokens = projected_bytes.saturating_add(3) / 4;
    (projected_bytes, projected_tokens, entries)
}

fn serialized_size<T: Serialize>(value: &T) -> (i64, i64) {
    let byte_count = serde_json::to_vec(value)
        .map(|bytes| bytes.len().min(MAX_PROJECTED_ITEM_BYTES))
        .unwrap_or_default();
    let byte_count = i64::try_from(byte_count).unwrap_or(i64::MAX);
    (byte_count, byte_count.saturating_add(3) / 4)
}

fn is_new_response(event: &EventMsg) -> bool {
    match event {
        EventMsg::AgentMessage(_) => true,
        EventMsg::RawResponseItem(item) => {
            matches!(item.item, ResponseItem::Message { ref role, .. } if role == "assistant")
        }
        EventMsg::ItemCompleted(item) => {
            matches!(item.item, codex_protocol::items::TurnItem::AgentMessage(_))
        }
        _ => false,
    }
}

#[cfg(test)]
#[path = "fork_metrics_tests.rs"]
mod tests;
