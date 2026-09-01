use super::AgentControl;
use super::fleet_types::*;
use crate::config::Config;
use codex_agent_graph_store::ThreadSpawnEdgeStatus;
use codex_protocol::AgentPath;
use codex_protocol::ThreadId;
use codex_protocol::error::CodexErr;
use codex_protocol::error::CodexErrorDetails;
use codex_protocol::error::Result as CodexResult;
use codex_protocol::protocol::SessionSource;
use codex_protocol::protocol::SubAgentSource;
use codex_state::FleetMemberResult;
use codex_state::FleetOperation;
use codex_state::FleetOperationKind;
use codex_state::FleetOperationSnapshot;
use codex_state::FleetOperationStatus;
use codex_state::FleetRootState;
use codex_state::WorkflowCheckpointCreate;
use codex_state::WorkflowStore;
use codex_thread_store::ReadThreadParams;
use serde_json::json;
use std::cmp::Reverse;
use std::sync::Arc;
impl AgentControl {
    pub(crate) async fn fleet_status(
        &self,
        root_thread_id: ThreadId,
    ) -> CodexResult<FleetStatusSnapshot> {
        self.validate_fleet_root(root_thread_id)?;
        let workflow = self.require_workflow_store()?;
        let root = workflow
            .get_fleet_state(&root_thread_id.to_string())
            .await
            .map_err(fleet_coordination_error)?
            .ok_or_else(|| {
                CodexErr::InvalidRequest("fleet root has no durable state".to_string())
            })?;
        let members = self
            .fleet_members(root_thread_id, /*include_closed*/ true)
            .await?;
        let mut statuses = Vec::with_capacity(members.len());
        for member in members {
            statuses.push(FleetMemberStatus {
                thread_id: member.thread_id,
                state: fleet_member_state(
                    root.state,
                    member.edge_status,
                    self.get_status(member.thread_id).await,
                ),
                depth: member.depth,
                order: member.order,
            });
        }
        let operation = match root.active_operation_id.as_deref() {
            Some(operation_id) => workflow
                .get_fleet_operation_status(operation_id)
                .await
                .map_err(fleet_coordination_error)?,
            None => None,
        };
        Ok(FleetStatusSnapshot {
            root_thread_id,
            root,
            members: statuses,
            operation,
        })
    }

    /// Suspend open members from leaves to root after sealing admissions.
    pub(crate) async fn suspend_fleet(
        &self,
        root_thread_id: ThreadId,
        expected_generation: i64,
    ) -> CodexResult<FleetOperationSnapshot> {
        self.validate_fleet_root(root_thread_id)?;
        let workflow = self.require_workflow_store()?;
        let (operation, mut members) = self
            .begin_fleet_operation(
                &workflow,
                root_thread_id,
                FleetOperationKind::Suspend,
                expected_generation,
            )
            .await?;
        members.sort_by_key(|member| (Reverse(member.depth), Reverse(member.order)));

        let mut had_failure = false;
        for member in members {
            let previous = self.get_status(member.thread_id).await;
            let transition = self.suspend_member(root_thread_id, member.thread_id).await;
            let success = transition.is_ok();
            had_failure |= !success;
            if let Err(record_error) = self
                .record_member_transition(
                    &workflow,
                    &operation,
                    member,
                    FleetMemberTransition {
                        requested_state: "suspended",
                        previous,
                        success,
                        final_state: success.then_some("suspended"),
                        error: (!success).then_some("suspension failed"),
                    },
                )
                .await
            {
                return Err(self
                    .finalize_after_member_result_error(&workflow, &operation, record_error)
                    .await);
            }
        }
        let final_status = if had_failure {
            FleetOperationStatus::Recoverable
        } else {
            FleetOperationStatus::Complete
        };
        self.finalize_fleet_operation(&workflow, &operation, final_status)
            .await
    }

    pub(crate) async fn resume_fleet(
        &self,
        root_thread_id: ThreadId,
        expected_generation: i64,
        config: Config,
    ) -> CodexResult<FleetOperationSnapshot> {
        self.validate_fleet_root(root_thread_id)?;
        let state = self.upgrade()?;
        let workflow = self.require_workflow_store()?;
        let (operation, mut members) = self
            .begin_fleet_operation(
                &workflow,
                root_thread_id,
                FleetOperationKind::Resume,
                expected_generation,
            )
            .await?;
        members.sort_by_key(|member| (member.depth, member.order));

        let mut had_failure = false;
        for member in members {
            let previous = self.get_status(member.thread_id).await;
            let transition = self
                .resume_member(&state, &config, root_thread_id, member)
                .await;
            let success = transition.is_ok();
            had_failure |= !success;
            if let Err(record_error) = self
                .record_member_transition(
                    &workflow,
                    &operation,
                    member,
                    FleetMemberTransition {
                        requested_state: "running",
                        previous,
                        success,
                        final_state: success.then_some("running"),
                        error: (!success).then_some("resume failed"),
                    },
                )
                .await
            {
                return Err(self
                    .finalize_after_member_result_error(&workflow, &operation, record_error)
                    .await);
            }
        }
        let final_status = if had_failure {
            FleetOperationStatus::Recoverable
        } else {
            FleetOperationStatus::Complete
        };
        self.finalize_fleet_operation(&workflow, &operation, final_status)
            .await
    }

    pub(crate) async fn close_fleet(
        &self,
        root_thread_id: ThreadId,
        expected_generation: i64,
    ) -> CodexResult<FleetOperationSnapshot> {
        self.validate_fleet_root(root_thread_id)?;
        let workflow = self.require_workflow_store()?;
        let members = self
            .fleet_members(root_thread_id, /*include_closed*/ false)
            .await?;
        for member in &members {
            if !is_close_ready(self.get_status(member.thread_id).await) {
                return Err(CodexErr::InvalidRequest(
                    "fleet close requires every member to be idle or final".to_string(),
                ));
            }
        }
        let (operation, mut members) = self
            .begin_fleet_operation(
                &workflow,
                root_thread_id,
                FleetOperationKind::Close,
                expected_generation,
            )
            .await?;
        members.sort_by_key(|member| (Reverse(member.depth), Reverse(member.order)));

        let mut had_failure = false;
        for member in members {
            let previous = self.get_status(member.thread_id).await;
            let transition = self.close_member(root_thread_id, member.thread_id).await;
            let success = transition.is_ok();
            had_failure |= !success;
            if let Err(record_error) = self
                .record_member_transition(
                    &workflow,
                    &operation,
                    member,
                    FleetMemberTransition {
                        requested_state: "closed",
                        previous,
                        success,
                        final_state: success.then_some("closed"),
                        error: (!success).then_some("close failed"),
                    },
                )
                .await
            {
                return Err(self
                    .finalize_after_member_result_error(&workflow, &operation, record_error)
                    .await);
            }
        }
        let final_status = if had_failure {
            FleetOperationStatus::Recoverable
        } else {
            FleetOperationStatus::Complete
        };
        self.finalize_fleet_operation(&workflow, &operation, final_status)
            .await
    }

    pub(super) async fn ensure_fleet_data_admission(
        &self,
        parent_thread_id: ThreadId,
    ) -> CodexResult<()> {
        let Some(workflow) = self.workflow_store() else {
            return Ok(());
        };
        let root_thread_id = self
            .state
            .agent_id_for_path(&AgentPath::root())
            .unwrap_or(parent_thread_id);
        let root = workflow
            .get_fleet_state(&root_thread_id.to_string())
            .await
            .map_err(fleet_coordination_error)?;
        if root.is_some_and(|root| {
            root.admissions_sealed || matches!(root.state, FleetRootState::Closed)
        }) {
            return Err(CodexErr::InvalidRequest(
                "fleet admissions are sealed".to_string(),
            ));
        }
        Ok(())
    }

    fn validate_fleet_root(&self, root_thread_id: ThreadId) -> CodexResult<()> {
        match self.state.agent_id_for_path(&AgentPath::root()) {
            Some(registered_root) if registered_root == root_thread_id => Ok(()),
            Some(_) => Err(CodexErr::InvalidRequest(
                "fleet operation target must be the registered root agent".to_string(),
            )),
            None => Err(CodexErr::InvalidRequest(
                "fleet root agent is not registered".to_string(),
            )),
        }
    }

    fn require_workflow_store(&self) -> CodexResult<WorkflowStore> {
        self.workflow_store().ok_or_else(|| {
            CodexErr::UnsupportedOperation("fleet workflow state is unavailable".to_string())
        })
    }

    async fn fleet_members(
        &self,
        root_thread_id: ThreadId,
        include_closed: bool,
    ) -> CodexResult<Vec<FleetMemberSpec>> {
        let state = self.upgrade()?;
        let Some(graph) = state.agent_graph_store() else {
            return Ok(vec![FleetMemberSpec {
                thread_id: root_thread_id,
                parent_thread_id: None,
                depth: 0,
                order: 0,
                edge_status: None,
            }]);
        };
        let status_filter = (!include_closed).then_some(ThreadSpawnEdgeStatus::Open);
        let details = graph
            .list_thread_spawn_edge_details(root_thread_id, status_filter)
            .await
            .map_err(|error| CodexErr::Fatal(format!("failed to read fleet graph: {error}")))?;
        let mut members = Vec::with_capacity(details.len().saturating_add(1));
        members.push(FleetMemberSpec {
            thread_id: root_thread_id,
            parent_thread_id: None,
            depth: 0,
            order: 0,
            edge_status: None,
        });
        members.extend(details.into_iter().map(|detail| FleetMemberSpec {
            thread_id: detail.child_id,
            parent_thread_id: Some(detail.parent_id),
            depth: detail.depth,
            order: detail.order.saturating_add(1),
            edge_status: Some(detail.status),
        }));
        Ok(members)
    }

    async fn begin_fleet_operation(
        &self,
        workflow: &WorkflowStore,
        root_thread_id: ThreadId,
        kind: FleetOperationKind,
        expected_generation: i64,
    ) -> CodexResult<(FleetOperation, Vec<FleetMemberSpec>)> {
        let members = self
            .fleet_members(root_thread_id, /*include_closed*/ false)
            .await?;
        let expected_member_count = u32::try_from(members.len()).map_err(|_| {
            CodexErr::InvalidRequest("fleet member count exceeds the coordination limit".into())
        })?;
        self.recover_running_fleet_operation(workflow, root_thread_id, kind, expected_generation)
            .await?;
        let operation = workflow
            .begin_fleet_operation(
                &root_thread_id.to_string(),
                kind,
                expected_generation,
                expected_member_count,
            )
            .await
            .map_err(fleet_coordination_error)?;
        Ok((operation, members))
    }

    /// An explicit resume or close may recover an operation left running by a
    /// previous process. This is deliberately scoped to the user-requested
    /// lifecycle call; startup and status reads never retry or recover work.
    async fn recover_running_fleet_operation(
        &self,
        workflow: &WorkflowStore,
        root_thread_id: ThreadId,
        kind: FleetOperationKind,
        expected_generation: i64,
    ) -> CodexResult<()> {
        let Some(root) = workflow
            .get_fleet_state(&root_thread_id.to_string())
            .await
            .map_err(fleet_coordination_error)?
        else {
            return Ok(());
        };
        if root.generation != expected_generation {
            return Ok(());
        }
        let Some(operation_id) = root.active_operation_id else {
            return Ok(());
        };
        let Some(operation) = workflow
            .get_fleet_operation_status(&operation_id)
            .await
            .map_err(fleet_coordination_error)?
        else {
            return Err(CodexErr::Fatal(
                "fleet root references a missing active operation".into(),
            ));
        };
        if operation.operation.status == FleetOperationStatus::Running
            && matches!(kind, FleetOperationKind::Resume | FleetOperationKind::Close)
        {
            workflow
                .recover_fleet_operation(&operation_id)
                .await
                .map_err(fleet_coordination_error)?;
        }
        Ok(())
    }

    async fn suspend_member(
        &self,
        root_thread_id: ThreadId,
        thread_id: ThreadId,
    ) -> CodexResult<()> {
        let state = self.upgrade()?;
        if state.get_thread(thread_id).await.is_err() {
            return self.suspend_unloaded_member(&state, thread_id).await;
        }
        if thread_id == root_thread_id {
            let thread = state.get_thread(thread_id).await?;
            match thread.suspend_turn_and_shutdown().await? {
                codex_protocol::turn_input::SuspendTurnOutcome::Suspended { .. } => {
                    let _ = state.remove_thread(&thread_id).await;
                    self.forget_v2_residency(thread_id);
                    self.state.release_spawned_thread(thread_id);
                    Ok(())
                }
                codex_protocol::turn_input::SuspendTurnOutcome::NotActive
                | codex_protocol::turn_input::SuspendTurnOutcome::UnsupportedTask => {
                    self.shutdown_live_agent(thread_id).await.map(|_| ())
                }
                codex_protocol::turn_input::SuspendTurnOutcome::HasLiveDescendants => Err(
                    CodexErr::InvalidRequest("fleet root still has live descendants".into()),
                ),
            }
        } else {
            self.shutdown_live_agent(thread_id).await.map(|_| ())
        }
    }

    /// An evicted member has no runtime to receive a shutdown operation. Its
    /// durable thread identity (or retained registry metadata) is the source
    /// of truth, so suspension records a successful no-op and lets the fleet
    /// projection report the member from its lifecycle.
    async fn suspend_unloaded_member(
        &self,
        state: &Arc<crate::thread_manager::ThreadManagerState>,
        thread_id: ThreadId,
    ) -> CodexResult<()> {
        if self.state.agent_metadata_for_thread(thread_id).is_some() {
            return Ok(());
        }
        match state
            .read_stored_thread(ReadThreadParams {
                thread_id,
                include_archived: true,
                include_history: false,
            })
            .await
        {
            Ok(_) => Ok(()),
            Err(error) if matches!(error.details(), CodexErrorDetails::ThreadNotFound(_)) => Ok(()),
            Err(error) => Err(error),
        }
    }

    async fn resume_member(
        &self,
        state: &Arc<crate::thread_manager::ThreadManagerState>,
        config: &Config,
        root_thread_id: ThreadId,
        member: FleetMemberSpec,
    ) -> CodexResult<()> {
        if state.get_thread(member.thread_id).await.is_ok() {
            return Ok(());
        }
        if let Some(parent_thread_id) = member.parent_thread_id
            && state.get_thread(parent_thread_id).await.is_err()
        {
            return Err(CodexErr::InvalidRequest(
                "fleet parent must resume before its descendant".into(),
            ));
        }
        let stored = state
            .read_stored_thread(ReadThreadParams {
                thread_id: member.thread_id,
                include_archived: true,
                include_history: false,
            })
            .await?;
        let source = if member.thread_id == root_thread_id {
            stored.source
        } else {
            SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
                parent_thread_id: member
                    .parent_thread_id
                    .ok_or_else(|| CodexErr::InvalidRequest("fleet member has no parent".into()))?,
                depth: i32::try_from(member.depth)
                    .map_err(|_| CodexErr::InvalidRequest("fleet depth is too large".into()))?,
                agent_path: stored
                    .agent_path
                    .as_deref()
                    .map(AgentPath::try_from)
                    .transpose()
                    .map_err(|error| {
                        CodexErr::InvalidRequest(format!("invalid fleet agent path: {error}"))
                    })?,
                agent_nickname: stored.agent_nickname,
                agent_role: stored.agent_role,
            })
        };
        self.resume_single_agent_from_rollout(config.clone(), member.thread_id, source)
            .await
            .map(|_| ())
    }

    async fn close_member(&self, root_thread_id: ThreadId, thread_id: ThreadId) -> CodexResult<()> {
        let state = self.upgrade()?;
        let shutdown = match state.get_thread(thread_id).await {
            Ok(_) => self.shutdown_live_agent(thread_id).await,
            Err(error) if matches!(error.details(), CodexErrorDetails::ThreadNotFound(_)) => {
                Ok(String::new())
            }
            Err(error) => Err(error),
        }?;
        let _ = shutdown;
        if thread_id != root_thread_id
            && let Some(graph) = state.agent_graph_store()
        {
            graph
                .set_thread_spawn_edge_status(thread_id, ThreadSpawnEdgeStatus::Closed)
                .await
                .map_err(|error| {
                    CodexErr::Fatal(format!("failed to close fleet graph edge: {error}"))
                })?;
        }
        Ok(())
    }

    async fn record_member_transition(
        &self,
        workflow: &WorkflowStore,
        operation: &FleetOperation,
        member: FleetMemberSpec,
        transition: FleetMemberTransition<'_>,
    ) -> CodexResult<()> {
        let root_state = workflow
            .get_fleet_state(&operation.root_run_id)
            .await
            .map_err(fleet_coordination_error)?
            .ok_or_else(|| CodexErr::Fatal("fleet root disappeared while recording result".into()))?
            .state;
        workflow
            .record_fleet_member_result(&FleetMemberResult {
                operation_id: operation.operation_id.clone(),
                member_id: member.thread_id.to_string(),
                thread_id: Some(member.thread_id.to_string()),
                run_id: Some(member.thread_id.to_string()),
                requested_state: transition.requested_state.to_string(),
                previous_state: Some(
                    fleet_member_state(root_state, member.edge_status, transition.previous)
                        .as_str()
                        .to_string(),
                ),
                final_state: transition.final_state.map(str::to_string),
                success: transition.success,
                error: transition.error.map(str::to_string),
                depth: i64::from(member.depth),
                order_index: i64::try_from(member.order)
                    .map_err(|_| CodexErr::InvalidRequest("fleet order is too large".into()))?,
                updated_at_ms: 0,
            })
            .await
            .map_err(fleet_coordination_error)?;
        if workflow
            .get_run(&member.thread_id.to_string())
            .await
            .map_err(fleet_coordination_error)?
            .is_some()
        {
            workflow
                .append_checkpoint(&WorkflowCheckpointCreate {
                    run_id: member.thread_id.to_string(),
                    checkpoint_kind: "fleetMemberTransition".to_string(),
                    rollout_ordinal: None,
                    rollout_byte_offset: None,
                    payload: json!({
                        "operationId": operation.operation_id,
                        "memberId": member.thread_id,
                        "requestedState": transition.requested_state,
                        "success": transition.success,
                    }),
                })
                .await
                .map_err(fleet_coordination_error)?;
        }
        Ok(())
    }

    async fn finalize_fleet_operation(
        &self,
        workflow: &WorkflowStore,
        operation: &FleetOperation,
        status: FleetOperationStatus,
    ) -> CodexResult<FleetOperationSnapshot> {
        workflow
            .finalize_fleet_operation(&operation.operation_id, status)
            .await
            .map_err(fleet_coordination_error)?;
        workflow
            .get_fleet_operation_status(&operation.operation_id)
            .await
            .map_err(fleet_coordination_error)?
            .ok_or_else(|| CodexErr::Fatal("fleet operation disappeared after finalization".into()))
    }

    async fn finalize_after_member_result_error(
        &self,
        workflow: &WorkflowStore,
        operation: &FleetOperation,
        record_error: CodexErr,
    ) -> CodexErr {
        match self
            .finalize_fleet_operation(workflow, operation, FleetOperationStatus::Recoverable)
            .await
        {
            Ok(_) => record_error,
            Err(finalize_error) => CodexErr::Fatal(format!(
                "failed to record fleet member result: {record_error}; failed to finalize recoverable operation: {finalize_error}"
            )),
        }
    }
}

#[cfg(test)]
#[path = "fleet_tests.rs"]
mod tests;
