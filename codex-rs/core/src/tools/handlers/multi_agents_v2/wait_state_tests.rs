use super::*;
use codex_protocol::protocol::AgentStatus;

#[test]
fn timeout_keeps_the_observed_revision() {
    let result = WaitAgentResult::from_outcome(
        WaitOutcome::TimedOut {
            revision: 17,
            needs_attention: false,
        },
        None,
        1_000,
        Vec::new(),
        Vec::new(),
    );

    assert_eq!(
        result,
        WaitAgentResult {
            message: "Wait timed out.".to_string(),
            timed_out: true,
            revision: 17,
            reason: WaitAgentWakeReason::Timeout,
            targets: Vec::new(),
            agents: Vec::new(),
        }
    );
}

#[test]
fn response_uses_camel_case_causal_values_and_target_fields() {
    let result = WaitAgentResult::from_outcome(
        WaitOutcome::TimedOut {
            revision: 4,
            needs_attention: true,
        },
        None,
        1_000,
        vec![WaitAgentTargetSnapshot {
            canonical_path: "/root/worker".to_string(),
            status: WaitAgentTargetStatus::WaitingForTool,
            generation: 0,
            last_activity_at: Some(10),
            idle_ms: Some(20),
            waiting_terminal: None,
            waiting_tool: Some("tool".to_string()),
        }],
        Vec::new(),
    );
    let value = serde_json::to_value(result).expect("wait result should serialize");

    assert_eq!(value["reason"], "needsAttention");
    assert_eq!(value["targets"][0]["canonicalPath"], "/root/worker");
    assert_eq!(value["targets"][0]["status"], "waitingForTool");
    assert_eq!(value["targets"][0]["generation"], 0);
    assert_eq!(value["targets"][0]["lastActivityAt"], 10);
    assert_eq!(value["targets"][0]["idleMs"], 20);
    assert_eq!(value["targets"][0]["waitingTool"], "tool");
}

#[test]
fn needs_attention_progress_is_immediate_and_not_a_timeout() {
    let result = WaitAgentResult::from_outcome(
        WaitOutcome::Progress {
            revision: 18,
            reason: WaitAgentWakeReason::NeedsAttention,
        },
        None,
        1_000,
        Vec::new(),
        Vec::new(),
    );

    assert_eq!(result.reason, WaitAgentWakeReason::NeedsAttention);
    assert!(!result.timed_out);
    assert_eq!(result.message, "Wait completed.");
}

#[test]
fn explicit_waiting_activity_maps_to_tool_status() {
    let activity = AgentActivity {
        at_ms: 10,
        label: "waiting for tool".to_string(),
    };

    assert_eq!(
        wait_status(AgentStatus::Running, Some(&activity)),
        (
            WaitAgentTargetStatus::WaitingForTool,
            Some("tool".to_string())
        )
    );
}

#[test]
fn waiting_terminal_activity_is_reported_as_an_awaited_tool() {
    let activity = AgentActivity {
        at_ms: 10,
        label: "waiting for terminal".to_string(),
    };

    assert_eq!(
        wait_status(AgentStatus::Running, Some(&activity)),
        (WaitAgentTargetStatus::WaitingForTool, None)
    );
    let snapshot = target_snapshot(
        AgentPath::try_from("/root/worker").expect("agent path"),
        AgentStatus::Running,
        Some(activity),
    );
    assert!(snapshot.waiting_terminal.is_none());
}

#[test]
fn typed_waiting_terminal_overrides_running_status_and_preserves_metadata() {
    let terminal = TerminalProcessSnapshot {
        session_id: "session-1".to_string(),
        pid: 42,
        command: "cargo test".to_string(),
        started_at: 100,
        elapsed_ms: 900,
        last_activity_at: 800,
        last_output_at: Some(750),
        last_output_preview: Some("waiting for input".to_string()),
        last_output_bytes: 17,
        output_bytes: 17,
        state: TerminalProcessState::NeedsAttention,
    };
    let snapshot = target_snapshot_with_lifecycle(
        AgentPath::try_from("/root/worker").expect("agent path"),
        AgentLifecycle::from_agent_status(&AgentStatus::Running, 3, None),
        None,
        Some(terminal.clone()),
    );

    assert_eq!(snapshot.status, WaitAgentTargetStatus::WaitingForTool);
    assert_eq!(snapshot.waiting_terminal, Some(terminal));
    let value = serde_json::to_value(snapshot).expect("typed terminal snapshot serializes");
    assert_eq!(value["waitingTerminal"]["sessionId"], "session-1");
    assert_eq!(value["waitingTerminal"]["pid"], 42);
    assert_eq!(value["waitingTerminal"]["command"], "cargo test");
    assert_eq!(value["waitingTerminal"]["state"], "needsAttention");
    assert_eq!(value["waitingTerminal"]["lastOutputBytes"], 17);
}
