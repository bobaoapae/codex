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

#[test]
fn omitted_wait_mode_keeps_bounded_compatibility_default() {
    let args: WaitArgs = serde_json::from_value(serde_json::json!({
        "timeout_ms": 100,
    }))
    .expect("wait arguments should parse");

    assert_eq!(args.mode, WaitMode::Bounded);
}

#[test]
fn until_change_wait_mode_is_explicit_and_snake_case() {
    let args: WaitArgs = serde_json::from_value(serde_json::json!({
        "mode": "until_change",
    }))
    .expect("until_change mode should parse");

    assert_eq!(args.mode, WaitMode::UntilChange);
}
