use super::*;
use codex_protocol::protocol::ErrorEvent;
use codex_protocol::protocol::TurnAbortReason;
use codex_protocol::protocol::TurnAbortedEvent;
use codex_protocol::protocol::TurnCompleteEvent;
use pretty_assertions::assert_eq;

fn complete(turn_id: &str) -> RolloutItem {
    RolloutItem::EventMsg(EventMsg::TurnComplete(TurnCompleteEvent {
        turn_id: turn_id.to_string(),
        started_at: None,
        last_agent_message: Some("done".to_string()),
        error: None,
        completed_at: None,
        duration_ms: None,
        time_to_first_token_ms: None,
    }))
}

fn aborted(turn_id: &str) -> RolloutItem {
    RolloutItem::EventMsg(EventMsg::TurnAborted(TurnAbortedEvent {
        turn_id: Some(turn_id.to_string()),
        started_at: None,
        reason: TurnAbortReason::Interrupted,
        completed_at: None,
        duration_ms: None,
    }))
}

#[test]
fn restart_reconstructs_terminal_generation_without_active_slot() {
    let items = vec![complete("turn-0"), complete("turn-1"), aborted("turn-2")];

    let (status, generation, active) = reconstruct_agent_lifecycle(&items);

    assert_eq!(status, AgentStatus::Interrupted);
    assert_eq!(generation, 2);
    assert!(!active);
}

#[test]
fn restart_reconstructs_terminal_error_as_failed_generation() {
    let items = vec![RolloutItem::EventMsg(EventMsg::TurnComplete(
        TurnCompleteEvent {
            turn_id: "turn-0".to_string(),
            started_at: None,
            last_agent_message: None,
            error: Some(ErrorEvent {
                misalignment: None,
                message: "boom".to_string(),
                codex_error_info: None,
            }),
            completed_at: None,
            duration_ms: None,
            time_to_first_token_ms: None,
        },
    ))];

    let (status, generation, active) = reconstruct_agent_lifecycle(&items);

    assert_eq!(status, AgentStatus::Errored("boom".to_string()));
    assert_eq!(generation, 0);
    assert!(!active);
}
