use super::*;
use codex_protocol::protocol::TurnCompleteEvent;
use codex_state::SqliteConfig;
use codex_state::WorkflowRunCreate;
use codex_utils_absolute_path::AbsolutePathBuf;
use pretty_assertions::assert_eq;
use serde_json::json;
use tempfile::TempDir;

fn turn_complete(error: Option<ErrorEvent>) -> TurnCompleteEvent {
    TurnCompleteEvent {
        turn_id: "turn-1".to_string(),
        last_agent_message: None,
        error,
        started_at: None,
        completed_at: None,
        duration_ms: None,
        time_to_first_token_ms: None,
    }
}

fn error(codex_error_info: Option<CodexErrorInfo>) -> ErrorEvent {
    ErrorEvent {
        message: "test error".to_string(),
        codex_error_info,
        misalignment: None,
    }
}

#[test]
fn terminal_mapping_distinguishes_success_failure_and_blocked_budget() {
    assert_eq!(
        terminal_status_for_turn_complete(&turn_complete(None)),
        "succeeded"
    );
    assert_eq!(
        terminal_status_for_turn_complete(&turn_complete(Some(error(
            Some(CodexErrorInfo::Other,)
        )))),
        "failed"
    );
    assert_eq!(
        terminal_status_for_turn_complete(&turn_complete(Some(error(Some(
            CodexErrorInfo::SessionBudgetExceeded,
        ))))),
        "blocked"
    );
    assert_eq!(
        terminal_status_for_turn_complete(&turn_complete(Some(error(Some(
            CodexErrorInfo::UsageLimitExceeded,
        ))))),
        "blocked"
    );
}

#[test]
fn abort_mapping_only_reports_cancelled_for_an_explicit_cancel_request() {
    assert_eq!(terminal_status_for_turn_abort(true), "cancelled");
    assert_eq!(terminal_status_for_turn_abort(false), "aborted");
}

#[tokio::test]
async fn startup_recovery_marks_only_unloaded_runs_inconclusive() {
    let home = TempDir::new().expect("temporary workflow home");
    let sqlite = SqliteConfig::new_for_testing(
        AbsolutePathBuf::from_absolute_path(home.path()).expect("absolute temporary home"),
    );
    let workflow = WorkflowStore::open(&sqlite)
        .await
        .expect("open workflow store");
    for thread_id in [
        "00000000-0000-0000-0000-000000000001",
        "00000000-0000-0000-0000-000000000002",
    ] {
        workflow
            .create_run(&WorkflowRunCreate {
                run_id: thread_id.to_string(),
                thread_id: thread_id.to_string(),
                root_thread_id: None,
                parent_run_id: None,
                thread_class: WorkflowThreadClass::TransientJob,
                status: "running".to_string(),
                idempotency_key: None,
                provider: Some("test".to_string()),
                model: Some("test".to_string()),
                cwd: None,
                metadata: Some(json!({"test": true})),
            })
            .await
            .expect("create transient run");
    }
    let loaded = [ThreadId::from_u128(2)];

    assert_eq!(
        recover_unloaded_transient_jobs(&workflow, &loaded, i64::MAX)
            .await
            .expect("recover unloaded jobs"),
        1
    );
    assert_eq!(
        workflow
            .get_run("00000000-0000-0000-0000-000000000001")
            .await
            .expect("read unloaded run")
            .expect("unloaded run")
            .status,
        "inconclusive"
    );
    assert_eq!(
        workflow
            .get_run("00000000-0000-0000-0000-000000000002")
            .await
            .expect("read loaded run")
            .expect("loaded run")
            .status,
        "running"
    );
    workflow.close().await;
}

#[tokio::test]
async fn terminal_transition_is_idempotent_and_does_not_overwrite_a_winner() {
    let home = TempDir::new().expect("temporary workflow home");
    let sqlite = SqliteConfig::new_for_testing(
        AbsolutePathBuf::from_absolute_path(home.path()).expect("absolute temporary home"),
    );
    let workflow = WorkflowStore::open(&sqlite)
        .await
        .expect("open workflow store");
    workflow
        .create_run(&WorkflowRunCreate {
            run_id: "run-terminal".to_string(),
            thread_id: "run-terminal".to_string(),
            root_thread_id: None,
            parent_run_id: None,
            thread_class: WorkflowThreadClass::TransientJob,
            status: "running".to_string(),
            idempotency_key: None,
            provider: Some("test".to_string()),
            model: Some("test".to_string()),
            cwd: None,
            metadata: None,
        })
        .await
        .expect("create terminal run");

    transition_terminal(&workflow, "run-terminal", "succeeded").await;
    transition_terminal(&workflow, "run-terminal", "failed").await;

    assert_eq!(
        workflow
            .get_run("run-terminal")
            .await
            .expect("read terminal run")
            .expect("terminal run")
            .status,
        "succeeded"
    );
    workflow.close().await;
}

#[tokio::test]
async fn fork_invariant_job_terminal_state_comes_from_canonical_turn_events() {
    let home = TempDir::new().expect("temporary workflow home");
    let sqlite = SqliteConfig::new_for_testing(
        AbsolutePathBuf::from_absolute_path(home.path()).expect("absolute temporary home"),
    );
    let workflow = WorkflowStore::open(&sqlite)
        .await
        .expect("open workflow store");
    workflow
        .create_run(&WorkflowRunCreate {
            run_id: "run-failure-wins".to_string(),
            thread_id: "run-failure-wins".to_string(),
            root_thread_id: None,
            parent_run_id: None,
            thread_class: WorkflowThreadClass::TransientJob,
            status: "running".to_string(),
            idempotency_key: None,
            provider: Some("test".to_string()),
            model: Some("test".to_string()),
            cwd: None,
            metadata: None,
        })
        .await
        .expect("create transient run");

    // A ThreadStatus::Idle observation is not a terminal workflow signal.
    // Only the canonical TurnComplete event is allowed to choose the result.
    transition_terminal(&workflow, "run-failure-wins", "failed").await;
    transition_terminal(&workflow, "run-failure-wins", "succeeded").await;

    assert_eq!(
        workflow
            .get_run("run-failure-wins")
            .await
            .expect("read terminal run")
            .expect("terminal run")
            .status,
        "failed"
    );
    workflow.close().await;
}
