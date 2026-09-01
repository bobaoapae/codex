use super::*;
use codex_protocol::protocol::AgentStatus;
use pretty_assertions::assert_eq;

#[test]
fn lifecycle_projection_uses_one_shared_status_vocabulary() {
    let cases = [
        (
            AgentStatus::PendingInit,
            None,
            AgentLifecycleStatus::WaitingForUser,
        ),
        (
            AgentStatus::Running,
            Some("waiting for tool"),
            AgentLifecycleStatus::WaitingForTool,
        ),
        (
            AgentStatus::Running,
            Some("awaiting approval"),
            AgentLifecycleStatus::WaitingForApproval,
        ),
        (
            AgentStatus::Running,
            Some("waiting for user input"),
            AgentLifecycleStatus::WaitingForUser,
        ),
        (AgentStatus::Running, None, AgentLifecycleStatus::Running),
        (
            AgentStatus::Completed(None),
            None,
            AgentLifecycleStatus::Completed,
        ),
        (
            AgentStatus::Errored("boom".to_string()),
            None,
            AgentLifecycleStatus::Failed,
        ),
        (
            AgentStatus::Interrupted,
            None,
            AgentLifecycleStatus::Interrupted,
        ),
        (AgentStatus::Shutdown, None, AgentLifecycleStatus::NotFound),
        (AgentStatus::NotFound, None, AgentLifecycleStatus::NotFound),
    ];

    for (status, activity, expected) in cases {
        assert_eq!(
            AgentLifecycleStatus::from_agent_status(&status, activity),
            expected
        );
    }
}

#[test]
fn lifecycle_status_serializes_with_wait_agent_wire_names() {
    assert_eq!(
        serde_json::to_value(AgentLifecycleStatus::WaitingForApproval)
            .expect("lifecycle status should serialize"),
        "waitingForApproval"
    );
}
