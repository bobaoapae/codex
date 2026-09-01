//! Agent-control facade for runtime fork metrics.

use codex_protocol::ThreadId;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::TokenUsage;
use codex_state::WorkflowForkContextEntry;
use codex_state::WorkflowForkContextOrigin;
use codex_state::WorkflowForkTurns;

use super::AgentControl;
use super::SpawnAgentForkMode;

impl AgentControl {
    /// Return the bounded inherited-history count recorded for one child fork.
    ///
    /// Fork context entries contain size/provenance only; callers must keep the
    /// origin unknown when the workflow store has no entry or its bounded
    /// projection has already been evicted.
    pub(crate) async fn fork_context_inherited_count(
        &self,
        parent_thread_id: ThreadId,
        child_thread_id: ThreadId,
    ) -> Option<usize> {
        let workflow = self.workflow_store()?;
        let child_thread_id = child_thread_id.to_string();
        let metrics = workflow
            .list_fork_metrics(&parent_thread_id.to_string())
            .await
            .ok()?
            .into_iter()
            .find(|metrics| metrics.child_thread_id.as_deref() == Some(child_thread_id.as_str()))?;
        let entries = workflow.list_fork_context(&metrics.fork_id).await.ok()?;
        let inherited = entries
            .iter()
            .filter(|entry| entry.origin == WorkflowForkContextOrigin::InheritedHistory)
            .count();
        (inherited > 0).then_some(inherited)
    }

    pub(crate) async fn record_fork_spawn_requested(
        &self,
        parent_thread_id: ThreadId,
        spawn_call_id: &str,
        fork_mode: &SpawnAgentForkMode,
    ) -> String {
        let fork_turns = match fork_mode {
            SpawnAgentForkMode::FullHistory => WorkflowForkTurns::FullHistory,
            SpawnAgentForkMode::LastNTurns(count) => {
                WorkflowForkTurns::LastNTurns(u32::try_from(*count).unwrap_or(u32::MAX).max(1))
            }
        };
        self.fork_metrics
            .spawn_requested(
                self.workflow_store(),
                parent_thread_id,
                spawn_call_id,
                fork_turns,
            )
            .await
    }

    pub(crate) async fn update_fork_projection(
        &self,
        fork_id: &str,
        projected_fork_bytes: i64,
        projected_fork_tokens: i64,
        context_entries: Vec<WorkflowForkContextEntry>,
    ) {
        self.fork_metrics
            .update_projection(
                self.workflow_store(),
                fork_id,
                projected_fork_bytes,
                projected_fork_tokens,
                context_entries,
            )
            .await;
    }

    pub(crate) async fn record_fork_child_created(&self, fork_id: &str, child_thread_id: ThreadId) {
        self.fork_metrics
            .child_created(self.workflow_store(), fork_id, child_thread_id)
            .await;
    }

    pub(crate) async fn record_fork_event(&self, child_thread_id: ThreadId, event: &EventMsg) {
        self.fork_metrics
            .observe_event(self.workflow_store(), child_thread_id, event)
            .await;
    }

    pub(crate) async fn record_fork_provider_usage(
        &self,
        child_thread_id: ThreadId,
        usage: &TokenUsage,
    ) {
        self.fork_metrics
            .observe_usage(self.workflow_store(), child_thread_id, usage)
            .await;
    }

    pub(crate) async fn claim_fork_compaction_warning(
        &self,
        fork_id: &str,
        projected_tokens: i64,
        limit_tokens: i64,
    ) -> bool {
        self.fork_metrics
            .claim_compaction_warning(
                self.workflow_store(),
                fork_id,
                projected_tokens,
                limit_tokens,
            )
            .await
    }
}
