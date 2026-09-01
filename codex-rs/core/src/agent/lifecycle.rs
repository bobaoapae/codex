use codex_protocol::protocol::AgentStatus;
use serde::Deserialize;
use serde::Serialize;

/// The stable lifecycle projection shared by agent-management tools.
///
/// `AgentStatus` is a protocol compatibility type and includes provider-facing
/// details such as the final message.  This type intentionally contains only
/// the state that a coordinator needs when deciding whether an agent is still
/// runnable or waitable.
#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub(crate) enum AgentLifecycleStatus {
    Running,
    WaitingForTool,
    WaitingForApproval,
    WaitingForUser,
    Completed,
    Failed,
    Interrupted,
    NotFound,
}

/// A lifecycle projection together with the logical generation of the agent.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct AgentLifecycle {
    pub(crate) status: AgentLifecycleStatus,
    pub(crate) generation: u64,
}

impl AgentLifecycleStatus {
    /// Resolve a protocol status and optional activity label to the shared
    /// management projection.
    pub(crate) fn from_agent_status(status: &AgentStatus, activity_label: Option<&str>) -> Self {
        match status {
            AgentStatus::PendingInit => Self::WaitingForUser,
            AgentStatus::Running => running_status(activity_label),
            AgentStatus::Interrupted => Self::Interrupted,
            AgentStatus::Completed(_) => Self::Completed,
            AgentStatus::Errored(_) => Self::Failed,
            // Shutdown means that no live runtime is available.  A shutdown
            // is not the same as a provider failure at this boundary.
            AgentStatus::Shutdown | AgentStatus::NotFound => Self::NotFound,
        }
    }

    /// Whether this status ends the current logical generation for waiting and
    /// capacity accounting.  Interrupted remains follow-up capable, but it no
    /// longer occupies a running/spawn slot.
    pub(crate) fn is_terminal(self) -> bool {
        matches!(
            self,
            Self::Completed | Self::Failed | Self::Interrupted | Self::NotFound
        )
    }
}

impl AgentLifecycle {
    pub(crate) fn from_agent_status(
        status: &AgentStatus,
        generation: u64,
        activity_label: Option<&str>,
    ) -> Self {
        Self {
            status: AgentLifecycleStatus::from_agent_status(status, activity_label),
            generation,
        }
    }
}

fn running_status(activity_label: Option<&str>) -> AgentLifecycleStatus {
    let Some(label) = activity_label.map(str::to_ascii_lowercase) else {
        return AgentLifecycleStatus::Running;
    };
    let is_waiting = label.contains("waiting") || label.contains("awaiting");
    if !is_waiting {
        return AgentLifecycleStatus::Running;
    }
    if label.contains("approval") || label.contains("approve") {
        AgentLifecycleStatus::WaitingForApproval
    } else if label.contains("tool") || label.contains("terminal") {
        AgentLifecycleStatus::WaitingForTool
    } else if label.contains("user") || label.contains("input") {
        AgentLifecycleStatus::WaitingForUser
    } else {
        AgentLifecycleStatus::Running
    }
}

#[cfg(test)]
#[path = "lifecycle_tests.rs"]
mod tests;
