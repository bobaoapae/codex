//! Internal fleet status projections used by `AgentControl`.

use crate::agent::AgentStatus;
use crate::agent::status::is_final;
use codex_agent_graph_store::ThreadSpawnEdgeStatus;
use codex_protocol::ThreadId;
use codex_protocol::error::CodexErr;
use codex_state::FleetOperationSnapshot;
use codex_state::FleetRootState;
use codex_state::FleetState;
use serde::Serialize;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) enum FleetAgentState {
    Running,
    WaitingForTool,
    WaitingForApproval,
    WaitingForUser,
    Idle,
    Suspended,
    Closed,
    Failed,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct FleetMemberStatus {
    pub(crate) thread_id: ThreadId,
    pub(crate) state: FleetAgentState,
    pub(crate) depth: u32,
    pub(crate) order: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct FleetStatusSnapshot {
    pub(crate) root_thread_id: ThreadId,
    pub(crate) root: FleetState,
    pub(crate) members: Vec<FleetMemberStatus>,
    pub(crate) operation: Option<FleetOperationSnapshot>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct FleetMemberSpec {
    pub(super) thread_id: ThreadId,
    pub(super) parent_thread_id: Option<ThreadId>,
    pub(super) depth: u32,
    pub(super) order: u64,
    pub(super) edge_status: Option<ThreadSpawnEdgeStatus>,
}

pub(super) struct FleetMemberTransition<'a> {
    pub(super) requested_state: &'a str,
    pub(super) previous: AgentStatus,
    pub(super) success: bool,
    pub(super) final_state: Option<&'a str>,
    pub(super) error: Option<&'a str>,
}

impl FleetAgentState {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Running => "running",
            Self::WaitingForTool => "waitingForTool",
            Self::WaitingForApproval => "waitingForApproval",
            Self::WaitingForUser => "waitingForUser",
            Self::Idle => "idle",
            Self::Suspended => "suspended",
            Self::Closed => "closed",
            Self::Failed => "failed",
        }
    }
}

pub(crate) fn fleet_agent_state(status: AgentStatus) -> FleetAgentState {
    match status {
        AgentStatus::PendingInit => FleetAgentState::WaitingForUser,
        AgentStatus::Running => FleetAgentState::Running,
        AgentStatus::Completed(_) => FleetAgentState::Idle,
        AgentStatus::Errored(_) => FleetAgentState::Failed,
        AgentStatus::Interrupted => FleetAgentState::Suspended,
        AgentStatus::Shutdown | AgentStatus::NotFound => FleetAgentState::Closed,
    }
}

pub(crate) fn fleet_member_state(
    root_state: FleetRootState,
    edge_status: Option<ThreadSpawnEdgeStatus>,
    observed: AgentStatus,
) -> FleetAgentState {
    if !matches!(observed, AgentStatus::NotFound) {
        return fleet_agent_state(observed);
    }
    if matches!(root_state, FleetRootState::Closed)
        || matches!(edge_status, Some(ThreadSpawnEdgeStatus::Closed))
    {
        return FleetAgentState::Closed;
    }
    match root_state {
        FleetRootState::Active => FleetAgentState::Idle,
        FleetRootState::Suspended => FleetAgentState::Suspended,
        FleetRootState::Closed => FleetAgentState::Closed,
        FleetRootState::Failed => FleetAgentState::Failed,
    }
}

pub(crate) fn is_close_ready(status: AgentStatus) -> bool {
    is_final(&status) || matches!(status, AgentStatus::NotFound)
}

pub(crate) fn fleet_coordination_error(error: anyhow::Error) -> CodexErr {
    CodexErr::InvalidRequest(format!("fleet coordination failed: {error}"))
}
