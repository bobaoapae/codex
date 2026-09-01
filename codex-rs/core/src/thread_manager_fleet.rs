use crate::agent::control::fleet_types::FleetAgentState as InternalFleetAgentState;
use crate::config::Config;
use crate::thread_manager::ThreadManager;
use codex_protocol::ThreadId;
use codex_protocol::error::Result as CodexResult;
use codex_state::FleetOperationSnapshot;
use serde::Deserialize;
use serde::Serialize;

/// Observable lifecycle state for one member of an agent fleet.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum FleetAgentState {
    Running,
    WaitingForTool,
    WaitingForApproval,
    WaitingForUser,
    Idle,
    Suspended,
    Closed,
    Failed,
}

/// Public status projection for one fleet member.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FleetMemberStatus {
    pub thread_id: ThreadId,
    pub state: FleetAgentState,
    pub depth: u32,
    pub order: u64,
}

/// One durable member outcome from a fleet lifecycle operation.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FleetMemberOutcome {
    pub operation_id: String,
    pub member_id: String,
    pub thread_id: Option<String>,
    pub run_id: Option<String>,
    pub requested_state: String,
    pub previous_state: Option<String>,
    pub final_state: Option<String>,
    pub success: bool,
    pub error: Option<String>,
    pub depth: i64,
    pub order_index: i64,
    pub updated_at_ms: i64,
}

/// Public result returned by a fleet lifecycle operation.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FleetOperationResult {
    pub operation_id: String,
    pub root_run_id: String,
    pub kind: String,
    pub status: String,
    pub expected_generation: i64,
    pub new_generation: i64,
    pub expected_member_count: u32,
    pub result_count: u32,
    pub partial: bool,
    pub created_at_ms: i64,
    pub updated_at_ms: i64,
    pub members: Vec<FleetMemberOutcome>,
}

/// Public status snapshot returned by [`ThreadManager::fleet_status`].
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FleetStatus {
    pub root_thread_id: ThreadId,
    pub state: String,
    pub generation: i64,
    pub admissions_sealed: bool,
    pub active_operation_id: Option<String>,
    pub members: Vec<FleetMemberStatus>,
    pub operation: Option<FleetOperationResult>,
}

impl ThreadManager {
    /// Read the narrow public fleet status projection.
    pub async fn fleet_status(&self, root_thread_id: ThreadId) -> CodexResult<FleetStatus> {
        let control = self.fleet_agent_control(root_thread_id, None).await?;
        Ok(public_status(control.fleet_status(root_thread_id).await?))
    }

    /// Suspend a fleet through its owning agent control plane.
    pub async fn suspend_fleet(
        &self,
        root_thread_id: ThreadId,
        expected_generation: i64,
    ) -> CodexResult<FleetOperationResult> {
        let control = self.fleet_agent_control(root_thread_id, None).await?;
        Ok(public_operation(
            control
                .suspend_fleet(root_thread_id, expected_generation)
                .await?,
        ))
    }

    /// Resume a fleet with an explicit base configuration.
    pub async fn resume_fleet(
        &self,
        root_thread_id: ThreadId,
        expected_generation: i64,
        config: Config,
    ) -> CodexResult<FleetOperationResult> {
        let control = self
            .fleet_agent_control(root_thread_id, Some(config.clone()))
            .await?;
        Ok(public_operation(
            control
                .resume_fleet(root_thread_id, expected_generation, config)
                .await?,
        ))
    }

    /// Close a fleet after every member passes the idle/final preflight.
    pub async fn close_fleet(
        &self,
        root_thread_id: ThreadId,
        expected_generation: i64,
    ) -> CodexResult<FleetOperationResult> {
        let control = self.fleet_agent_control(root_thread_id, None).await?;
        Ok(public_operation(
            control
                .close_fleet(root_thread_id, expected_generation)
                .await?,
        ))
    }
}

fn public_status(status: crate::agent::control::fleet_types::FleetStatusSnapshot) -> FleetStatus {
    FleetStatus {
        root_thread_id: status.root_thread_id,
        state: status.root.state.as_str().to_string(),
        generation: status.root.generation,
        admissions_sealed: status.root.admissions_sealed,
        active_operation_id: status.root.active_operation_id,
        members: status
            .members
            .into_iter()
            .map(|member| FleetMemberStatus {
                thread_id: member.thread_id,
                state: public_agent_state(member.state),
                depth: member.depth,
                order: member.order,
            })
            .collect(),
        operation: status.operation.map(public_operation),
    }
}

fn public_operation(snapshot: FleetOperationSnapshot) -> FleetOperationResult {
    let operation = snapshot.operation;
    FleetOperationResult {
        operation_id: operation.operation_id,
        root_run_id: operation.root_run_id,
        kind: operation.kind.as_str().to_string(),
        status: operation.status.as_str().to_string(),
        expected_generation: operation.expected_generation,
        new_generation: operation.new_generation,
        expected_member_count: operation.expected_member_count,
        result_count: operation.result_count,
        partial: operation.partial,
        created_at_ms: operation.created_at_ms,
        updated_at_ms: operation.updated_at_ms,
        members: snapshot
            .results
            .into_iter()
            .map(|member| FleetMemberOutcome {
                operation_id: member.operation_id,
                member_id: member.member_id,
                thread_id: member.thread_id,
                run_id: member.run_id,
                requested_state: member.requested_state,
                previous_state: member.previous_state,
                final_state: member.final_state,
                success: member.success,
                error: member.error,
                depth: member.depth,
                order_index: member.order_index,
                updated_at_ms: member.updated_at_ms,
            })
            .collect(),
    }
}

fn public_agent_state(state: InternalFleetAgentState) -> FleetAgentState {
    match state {
        InternalFleetAgentState::Running => FleetAgentState::Running,
        InternalFleetAgentState::WaitingForTool => FleetAgentState::WaitingForTool,
        InternalFleetAgentState::WaitingForApproval => FleetAgentState::WaitingForApproval,
        InternalFleetAgentState::WaitingForUser => FleetAgentState::WaitingForUser,
        InternalFleetAgentState::Idle => FleetAgentState::Idle,
        InternalFleetAgentState::Suspended => FleetAgentState::Suspended,
        InternalFleetAgentState::Closed => FleetAgentState::Closed,
        InternalFleetAgentState::Failed => FleetAgentState::Failed,
    }
}
