use super::*;
use crate::SqliteConfig;
use crate::runtime::test_support::unique_temp_dir;
use codex_utils_absolute_path::test_support::PathExt;
use pretty_assertions::assert_eq;

fn sqlite_config() -> SqliteConfig {
    let home = unique_temp_dir();
    SqliteConfig::new_for_testing(home.as_path().abs())
}

fn observation(updated_at_ms: i64) -> WorkflowTerminalObservation {
    WorkflowTerminalObservation {
        session_id: "session-1".to_string(),
        process_id: 7,
        command_summary: "echo safe".to_string(),
        started_at_ms: 100,
        elapsed_ms: 50,
        last_activity_at_ms: 140,
        last_output_at_ms: Some(140),
        last_output_preview: Some("safe output".to_string()),
        last_output_bytes: 11,
        output_bytes: 11,
        state: WorkflowTerminalProcessState::Running,
        final_receipt_emitted: false,
        updated_at_ms,
    }
}

#[tokio::test]
async fn terminal_observation_round_trips_and_stale_writes_do_not_regress_state() {
    let store = WorkflowStore::open(&sqlite_config())
        .await
        .expect("open workflow store");
    store
        .upsert_terminal_observation(&observation(200))
        .await
        .expect("insert terminal observation");

    let mut stale = observation(199);
    stale.state = WorkflowTerminalProcessState::NeedsAttention;
    store
        .upsert_terminal_observation(&stale)
        .await
        .expect("stale terminal observation should be ignored");
    let current = store
        .get_terminal_observation("session-1", 7)
        .await
        .expect("read terminal observation")
        .expect("terminal observation exists");
    assert_eq!(current, observation(200));

    let mut final_state = observation(201);
    final_state.state = WorkflowTerminalProcessState::Exited;
    final_state.final_receipt_emitted = true;
    store
        .upsert_terminal_observation(&final_state)
        .await
        .expect("update terminal observation");
    assert_eq!(
        store
            .list_terminal_observations("session-1")
            .await
            .expect("list terminal observations"),
        vec![final_state]
    );
    assert!(
        store
            .delete_terminal_observation("session-1", 7)
            .await
            .expect("delete terminal observation")
    );
    assert!(
        store
            .get_terminal_observation("session-1", 7)
            .await
            .expect("read deleted terminal observation")
            .is_none()
    );
    store.close().await;
}

#[tokio::test]
async fn terminal_observations_are_cleared_by_session() {
    let store = WorkflowStore::open(&sqlite_config())
        .await
        .expect("open workflow store");
    let first = observation(1);
    store
        .upsert_terminal_observation(&first)
        .await
        .expect("insert first terminal observation");
    let mut second = first.clone();
    second.process_id = 8;
    store
        .upsert_terminal_observation(&second)
        .await
        .expect("insert second terminal observation");
    assert_eq!(
        store
            .delete_terminal_observations_for_session("session-1")
            .await
            .expect("clear terminal observations"),
        2
    );
    assert!(
        store
            .list_terminal_observations("session-1")
            .await
            .expect("list cleared observations")
            .is_empty()
    );
    store.close().await;
}
