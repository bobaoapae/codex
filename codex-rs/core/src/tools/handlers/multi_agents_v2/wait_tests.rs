use super::*;
use crate::agent::AgentChangeKind;

#[test]
fn needs_attention_change_wakes_wait_agent_with_typed_reason() {
    assert_eq!(
        wake_reason(AgentChangeKind::NeedsAttention),
        WaitAgentWakeReason::NeedsAttention
    );
}

#[test]
fn terminal_resume_change_wakes_wait_agent_with_status_reason() {
    assert_eq!(
        wake_reason(AgentChangeKind::StatusChanged),
        WaitAgentWakeReason::StatusChanged
    );
}
