use super::AgentControl;
use crate::agent::AgentLifecycle;
use crate::agent::AgentLifecycleStatus;
use crate::agent::AgentStatus;
use crate::agent::status::agent_status_from_event;
use crate::config::Config;
use codex_history::RolloutItem;
use codex_protocol::ThreadId;
use codex_protocol::error::Result as CodexResult;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::MultiAgentVersion;

impl AgentControl {
    /// Project the authoritative registry lifecycle.  The live thread status
    /// wins when a runtime is loaded; after eviction, the registry's retained
    /// terminal status supplies the same projection without reopening a graph
    /// edge.
    pub(crate) async fn agent_lifecycle(&self, agent_id: ThreadId) -> AgentLifecycle {
        let status = self.get_status(agent_id).await;
        let activity = self.agent_activity(agent_id);
        self.state.lifecycle(
            agent_id,
            status,
            activity.as_ref().map(|activity| activity.label.as_str()),
        )
    }

    pub(crate) fn is_agent_closed(&self, agent_id: ThreadId) -> bool {
        self.state.is_agent_closed(agent_id)
    }

    /// Atomically reacquire a logical slot and advance the generation when a
    /// follow-up targets a terminal identity.  Active generations retain their
    /// generation number and existing turn semantics.
    pub(crate) async fn begin_followup_generation(
        &self,
        agent_id: ThreadId,
        config: &Config,
    ) -> CodexResult<u64> {
        let was_active = self.state.is_agent_active(agent_id);
        let status = self.get_status(agent_id).await;
        let terminal = AgentLifecycleStatus::from_agent_status(&status, None).is_terminal();
        let generation = self.state.begin_followup_generation(
            agent_id,
            status,
            config.effective_agent_max_threads(MultiAgentVersion::V2),
        )?;
        // Publish a causal status transition after the registry's generation
        // critical section has completed.
        if !was_active || terminal {
            let _ = self
                .state
                .record_status_change(agent_id, AgentStatus::PendingInit);
        }
        Ok(generation)
    }
}

/// Reconstruct the latest persisted lifecycle and generation after a process
/// restart.  A generation is the number of completed turns after the initial
/// turn, so one terminal turn maps to generation zero.
pub(crate) fn reconstruct_agent_lifecycle(items: &[RolloutItem]) -> (AgentStatus, u64, bool) {
    let mut status = AgentStatus::PendingInit;
    let mut terminal_turns = 0_u64;
    for item in items {
        let RolloutItem::EventMsg(event) = item else {
            continue;
        };
        if let Some(event_status) = agent_status_from_event(event) {
            status = event_status;
        }
        if matches!(event, EventMsg::TurnComplete(_) | EventMsg::TurnAborted(_)) {
            terminal_turns = terminal_turns.saturating_add(1);
        }
    }
    let generation = terminal_turns.saturating_sub(1);
    let active = !AgentLifecycleStatus::from_agent_status(&status, None).is_terminal();
    (status, generation, active)
}

#[cfg(test)]
#[path = "agent_lifecycle_tests.rs"]
mod tests;
