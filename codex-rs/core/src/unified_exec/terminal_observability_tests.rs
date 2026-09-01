use super::*;
use pretty_assertions::assert_eq;
use std::sync::Arc;
use std::sync::Mutex;

#[derive(Clone, Debug)]
struct FakeClock(Arc<Mutex<u64>>);

impl FakeClock {
    fn new(now_ms: u64) -> Self {
        Self(Arc::new(Mutex::new(now_ms)))
    }

    fn set(&self, now_ms: u64) {
        *self.0.lock().expect("fake clock lock") = now_ms;
    }
}

impl TerminalClock for FakeClock {
    fn now_ms(&self) -> u64 {
        *self.0.lock().expect("fake clock lock")
    }
}

fn store(clock: &FakeClock, threshold_ms: u64) -> Arc<TerminalObservabilityStore> {
    TerminalObservabilityStore::new(Arc::new(clock.clone()), threshold_ms)
}

#[test]
fn snapshot_is_bounded_and_redacts_command_and_output() {
    let clock = FakeClock::new(1_000);
    let store = store(&clock, 100);
    store.register(
        std::sync::Weak::new(),
        "session-1".to_string(),
        42,
        "call-1".to_string(),
        "curl --header 'Authorization: Bearer abcdefghijklmnop' --data token=super-secret",
        None,
    );

    clock.set(1_250);
    store.mark_output(
        42,
        b"Authorization: Bearer output-secret token=another-secret",
        None,
    );
    let snapshot = store.snapshot(42, None).expect("snapshot should exist");

    assert_eq!(snapshot.session_id, "session-1");
    assert_eq!(snapshot.pid, 42);
    assert_eq!(snapshot.started_at, 1_000);
    assert_eq!(snapshot.elapsed_ms, 250);
    assert_eq!(snapshot.last_activity_at, 1_250);
    assert_eq!(snapshot.last_output_at, Some(1_250));
    assert_eq!(snapshot.last_output_bytes, 56);
    assert_eq!(snapshot.output_bytes, 56);
    assert_eq!(snapshot.state, TerminalProcessState::Running);
    assert!(!snapshot.command.contains("abcdefghijklmnop"));
    assert!(!snapshot.command.contains("super-secret"));
    assert!(
        !snapshot
            .last_output_preview
            .as_deref()
            .unwrap_or_default()
            .contains("output-secret")
    );
    assert!(
        !snapshot
            .last_output_preview
            .as_deref()
            .unwrap_or_default()
            .contains("another-secret")
    );
    assert!(snapshot.command.len() <= MAX_COMMAND_SUMMARY_BYTES);
    assert!(
        snapshot
            .last_output_preview
            .as_deref()
            .is_some_and(|preview| preview.len() <= MAX_OUTPUT_PREVIEW_BYTES)
    );
}

#[test]
fn output_and_write_activity_clear_attention_without_sleeping() {
    let clock = FakeClock::new(10);
    let store = store(&clock, 100);
    store.register(
        std::sync::Weak::new(),
        "session-1".to_string(),
        7,
        "call-1".to_string(),
        "sleep 1",
        None,
    );

    assert_eq!(
        store.heartbeat(7, true, Some(109)),
        Some(ObservationChange::default())
    );
    assert_eq!(
        store.heartbeat(7, true, Some(111)),
        Some(ObservationChange {
            state_changed: true,
            entered_needs_attention: true,
            cleared_needs_attention: false,
        })
    );
    assert_eq!(
        store.mark_output(7, b"ready\n", Some(112)),
        Some(ObservationChange {
            state_changed: true,
            entered_needs_attention: false,
            cleared_needs_attention: true,
        })
    );
    assert_eq!(
        store.snapshot(7, Some(112)).expect("snapshot").state,
        TerminalProcessState::Running
    );
    assert_eq!(
        store.mark_write(7, "", Some(113)),
        Some(ObservationChange {
            state_changed: true,
            entered_needs_attention: false,
            cleared_needs_attention: false,
        })
    );
    assert_eq!(
        store.snapshot(7, Some(113)).expect("snapshot").state,
        TerminalProcessState::Waiting
    );
}

#[test]
fn terminal_transitions_and_final_receipt_are_idempotent() {
    let clock = FakeClock::new(100);
    let store = store(&clock, 100);
    store.register(
        std::sync::Weak::new(),
        "session-1".to_string(),
        9,
        "call-1".to_string(),
        "command",
        None,
    );

    assert_eq!(
        store.mark_state(9, TerminalProcessState::Cancelled, Some(150)),
        Some(ObservationChange {
            state_changed: true,
            entered_needs_attention: false,
            cleared_needs_attention: false,
        })
    );
    assert_eq!(
        store.mark_state(9, TerminalProcessState::Cancelled, Some(151)),
        Some(ObservationChange::default())
    );
    assert!(store.mark_final_receipt(9));
    assert!(!store.mark_final_receipt(9));
    assert!(store.final_receipt_emitted(9));
    assert_eq!(
        store.snapshot(9, Some(151)).expect("snapshot").state,
        TerminalProcessState::Cancelled
    );
    assert!(store.remove(9).is_some());
    assert!(store.snapshot(9, Some(151)).is_none());
}

#[test]
fn serialized_snapshot_uses_bounded_wire_names() {
    let clock = FakeClock::new(1);
    let store = store(&clock, 100);
    store.register(
        std::sync::Weak::new(),
        "session-1".to_string(),
        1,
        "call-1".to_string(),
        "true",
        None,
    );
    let value = serde_json::to_value(store.snapshot(1, Some(2)).expect("snapshot"))
        .expect("snapshot serializes");
    assert_eq!(value["sessionId"], "session-1");
    assert_eq!(value["pid"], 1);
    assert_eq!(value["command"], "true");
    assert_eq!(value["startedAt"], 1);
    assert_eq!(value["elapsedMs"], 1);
    assert_eq!(value["state"], "running");
}
