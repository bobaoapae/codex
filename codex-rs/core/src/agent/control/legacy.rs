use super::*;
use codex_protocol::error::CodexErrorDetails;
use codex_thread_store::PersistContext;

impl AgentControl {
    /// Submit a shutdown request for a live agent without marking it explicitly closed in
    /// persisted spawn-edge state.
    pub(crate) async fn shutdown_live_agent(&self, agent_id: ThreadId) -> CodexResult<String> {
        let state = self.upgrade()?;
        let result = if let Ok(thread) = state.get_thread(agent_id).await {
            thread
                .session
                .ensure_rollout_materialized(PersistContext::Standard)
                .await;
            thread.session.flush_rollout().await?;
            let result = if matches!(thread.agent_status().await, AgentStatus::Shutdown) {
                Ok(String::new())
            } else {
                state
                    .send_op(
                        agent_id,
                        Op::Shutdown {},
                        /*parent_turn_id*/ None,
                        /*root_turn_id*/ None,
                    )
                    .await
            };
            thread.wait_until_terminated().await;
            result
        } else if self.state.agent_metadata_for_thread(agent_id).is_some() {
            // Residency eviction and an earlier terminal event can remove the
            // runtime while retaining the identity and rollout.  There is no
            // shutdown request to submit in that state.
            Ok(String::new())
        } else {
            state
                .send_op(
                    agent_id,
                    Op::Shutdown {},
                    /*parent_turn_id*/ None,
                    /*root_turn_id*/ None,
                )
                .await
        };
        let _ = state.remove_thread(&agent_id).await;
        self.release_agent_leases(agent_id).await;
        self.forget_v2_residency(agent_id);
        self.state.release_active_slot(agent_id);
        result
    }

    /// Mark `agent_id` as explicitly closed in persisted spawn-edge state, then shut down the
    /// agent and every affected descendant deepest-first.  The registry keeps
    /// the identities and rollouts for a later explicit resume.
    pub(crate) async fn close_agent(&self, agent_id: ThreadId) -> CodexResult<String> {
        let state = self.upgrade()?;
        let known_agent = self.state.agent_metadata_for_thread(agent_id).is_some();
        // FORK: archive the ChatGPT Web conversations of the agent and its live
        // descendants before they shut down (an eviction keeps them).
        self.archive_chatgpt_web_conversations(agent_id).await;

        let members = self.close_subtree_members(agent_id).await?;
        let mut target_result = Ok(String::new());
        for member_id in members {
            let shutdown = self.shutdown_live_agent(member_id).await;
            self.state.mark_agent_closed(member_id);
            if let Some(agent_graph_store) = state.agent_graph_store()
                && let Err(err) = agent_graph_store
                    .set_thread_spawn_edge_status(
                        member_id,
                        codex_agent_graph_store::ThreadSpawnEdgeStatus::Closed,
                    )
                    .await
            {
                return Err(CodexErr::Fatal(format!(
                    "failed to persist thread-spawn edge status for {member_id}: {err}"
                )));
            }

            if member_id == agent_id {
                target_result = shutdown;
            } else if let Err(err) = shutdown
                && !matches!(
                    err.details(),
                    CodexErrorDetails::ThreadNotFound(_) | CodexErrorDetails::InternalAgentDied
                )
            {
                return Err(err);
            }
        }

        match target_result {
            Err(err)
                if known_agent
                    && matches!(
                        err.details(),
                        CodexErrorDetails::ThreadNotFound(_) | CodexErrorDetails::InternalAgentDied
                    ) =>
            {
                Ok(String::new())
            }
            result => result,
        }
    }

    /// FORK: archives the ChatGPT Web conversations backing `root` and its live
    /// spawn descendants. Called when a thread ends for good (root shutdown,
    /// explicit close) — never on an eviction, which rebuilds the agent later.
    pub(crate) async fn archive_chatgpt_web_conversations(&self, root: ThreadId) {
        let Ok(state) = self.upgrade() else {
            return;
        };
        let descendants = self
            .live_thread_spawn_descendants(root)
            .await
            .unwrap_or_default();
        for id in std::iter::once(root).chain(descendants) {
            if let Ok(thread) = state.get_thread(id).await {
                thread.session.archive_chatgpt_web_conversation().await;
            }
        }
    }

    /// Shut down `agent_id` and any live descendants reachable from the in-memory spawn tree.
    pub(crate) async fn shutdown_agent_tree(&self, agent_id: ThreadId) -> CodexResult<String> {
        let mut members = self.close_subtree_members(agent_id).await?;
        members.retain(|member_id| *member_id != agent_id);
        let result = self.shutdown_live_agent(agent_id).await;
        for descendant_id in members {
            match self.shutdown_live_agent(descendant_id).await {
                Ok(_) => {}
                Err(err)
                    if matches!(
                        err.details(),
                        CodexErrorDetails::ThreadNotFound(_) | CodexErrorDetails::InternalAgentDied
                    ) => {}
                Err(err) => return Err(err),
            }
        }
        result
    }

    /// Resolve all known descendants without using an Open graph edge as a
    /// proxy for a live runtime.  Persisted edges provide lineage after an
    /// eviction; loaded edges and retained registry paths cover stores without
    /// graph data (including ephemeral test threads).
    async fn close_subtree_members(&self, root_thread_id: ThreadId) -> CodexResult<Vec<ThreadId>> {
        let state = self.upgrade()?;
        let mut depths = HashMap::from([(root_thread_id, 0_usize)]);

        if let Some(graph) = state.agent_graph_store()
            && let Ok(details) = graph
                .list_thread_spawn_edge_details(root_thread_id, /*status_filter*/ None)
                .await
        {
            for detail in details {
                depths
                    .entry(detail.child_id)
                    .or_insert(detail.depth as usize);
            }
        }

        let mut changed = true;
        while changed {
            changed = false;
            for (parent_thread_id, child_thread_id) in state.list_live_thread_spawn_edges().await {
                if let Some(parent_depth) = depths.get(&parent_thread_id).copied()
                    && depths
                        .insert(child_thread_id, parent_depth.saturating_add(1))
                        .is_none()
                {
                    changed = true;
                }
            }
        }

        if let Some(root_path) = self
            .state
            .agent_metadata_for_thread(root_thread_id)
            .and_then(|metadata| metadata.agent_path)
        {
            for (thread_id, path) in self.state.all_agent_entries_for_prefix(Some(&root_path)) {
                let depth = path
                    .as_str()
                    .split('/')
                    .filter(|part| !part.is_empty())
                    .count()
                    .saturating_sub(
                        root_path
                            .as_str()
                            .split('/')
                            .filter(|part| !part.is_empty())
                            .count(),
                    );
                depths.entry(thread_id).or_insert(depth);
            }
        }

        let mut members = depths.into_iter().collect::<Vec<_>>();
        members.sort_by(|(left_id, left_depth), (right_id, right_depth)| {
            right_depth
                .cmp(left_depth)
                .then_with(|| right_id.to_string().cmp(&left_id.to_string()))
        });
        Ok(members
            .into_iter()
            .map(|(thread_id, _)| thread_id)
            .collect())
    }
}
