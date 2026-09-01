use super::*;
use crate::SqliteConfig;
use crate::runtime::test_support::unique_temp_dir;
use codex_utils_absolute_path::test_support::PathExt;
use pretty_assertions::assert_eq;
use std::sync::Arc;

fn sqlite_config() -> SqliteConfig {
    let home = unique_temp_dir();
    SqliteConfig::new_for_testing(home.as_path().abs())
}

fn member_result(operation_id: &str, member_id: &str, order_index: i64) -> FleetMemberResult {
    FleetMemberResult {
        operation_id: operation_id.to_string(),
        member_id: member_id.to_string(),
        thread_id: Some(format!("thread-{member_id}")),
        run_id: Some(format!("run-{member_id}")),
        requested_state: "suspended".to_string(),
        previous_state: Some("running".to_string()),
        final_state: Some("suspended".to_string()),
        success: true,
        error: None,
        depth: order_index % 2,
        order_index,
        updated_at_ms: 0,
    }
}

#[tokio::test]
async fn begin_uses_generation_cas_and_allows_only_one_active_operation() {
    let store = Arc::new(WorkflowStore::open(&sqlite_config()).await.unwrap());
    let first = store
        .begin_fleet_operation("root-1", FleetOperationKind::Suspend, 0, 1)
        .await
        .unwrap();
    assert_eq!(first.expected_generation, 0);
    assert_eq!(first.new_generation, 1);
    assert_eq!(first.status, FleetOperationStatus::Running);

    let stale = store
        .begin_fleet_operation("root-1", FleetOperationKind::Suspend, 0, 1)
        .await
        .expect_err("an active operation must block a stale begin");
    assert!(stale.to_string().contains("stale fleet generation"));
    assert!(
        store
            .begin_fleet_operation("root-1", FleetOperationKind::Suspend, 1, 1)
            .await
            .is_err()
    );

    let status = store
        .get_fleet_operation_status(&first.operation_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(status.operation.result_count, 0);
    assert_eq!(status.results, Vec::new());
    store.close().await;
}

#[tokio::test]
async fn concurrent_begins_have_one_generation_winner() {
    let store = Arc::new(WorkflowStore::open(&sqlite_config()).await.unwrap());
    let first_store = Arc::clone(&store);
    let second_store = Arc::clone(&store);
    let (first, second) = tokio::join!(
        first_store.begin_fleet_operation("root-concurrent", FleetOperationKind::Close, 0, 0),
        second_store.begin_fleet_operation("root-concurrent", FleetOperationKind::Close, 0, 0),
    );
    assert!(first.is_ok() ^ second.is_ok());
    store.close().await;
}

#[tokio::test]
async fn member_results_are_idempotent_and_finalize_requires_all_members() {
    let store = WorkflowStore::open(&sqlite_config()).await.unwrap();
    let operation = store
        .begin_fleet_operation("root-2", FleetOperationKind::Suspend, 0, 2)
        .await
        .unwrap();
    let first = member_result(&operation.operation_id, "a", 0);
    let recorded = store.record_fleet_member_result(&first).await.unwrap();
    let FleetMemberResultOutcome::Recorded(recorded) = recorded else {
        panic!("first result must be recorded");
    };
    assert!(recorded.updated_at_ms > 0);
    let repeated = store.record_fleet_member_result(&first).await.unwrap();
    let FleetMemberResultOutcome::AlreadyRecorded(repeated) = repeated else {
        panic!("same result must be idempotent");
    };
    assert_eq!(repeated, recorded);

    let mut conflict = first.clone();
    conflict.success = false;
    assert!(store.record_fleet_member_result(&conflict).await.is_err());

    let partial = store
        .finalize_fleet_operation(&operation.operation_id, FleetOperationStatus::Complete)
        .await
        .unwrap();
    assert_eq!(partial.status, FleetOperationStatus::Recoverable);
    assert!(partial.partial);
    assert_eq!(partial.result_count, 1);

    let second = member_result(&operation.operation_id, "b", 1);
    store.record_fleet_member_result(&second).await.unwrap();
    let complete = store
        .finish_fleet_operation(&operation.operation_id, FleetOperationStatus::Complete)
        .await
        .unwrap();
    assert_eq!(complete.status, FleetOperationStatus::Complete);
    assert!(!complete.partial);
    assert_eq!(complete.result_count, 2);
    let root = store.get_fleet_state("root-2").await.unwrap().unwrap();
    assert_eq!(root.state, FleetRootState::Suspended);
    assert!(root.admissions_sealed);
    assert_eq!(root.active_operation_id, None);
    store.close().await;
}

#[tokio::test]
async fn seal_is_generation_cas_and_resume_reopens_admissions_explicitly() {
    let store = WorkflowStore::open(&sqlite_config()).await.unwrap();
    let sealed = store.seal_fleet_admissions("root-3", 0).await.unwrap();
    assert_eq!(sealed.generation, 1);
    assert!(sealed.admissions_sealed);
    assert_eq!(
        store.seal_fleet_admissions("root-3", 1).await.unwrap(),
        sealed
    );
    assert!(store.seal_fleet_admissions("root-3", 0).await.is_err());

    let suspend = store
        .begin_fleet_operation("root-3", FleetOperationKind::Suspend, 1, 0)
        .await
        .unwrap();
    let finished = store
        .finalize_fleet_operation(&suspend.operation_id, FleetOperationStatus::Complete)
        .await
        .unwrap();
    assert_eq!(finished.status, FleetOperationStatus::Complete);
    let operation = store
        .begin_fleet_operation("root-3", FleetOperationKind::Resume, 2, 0)
        .await
        .unwrap();
    let finished = store
        .finalize_fleet_operation(&operation.operation_id, FleetOperationStatus::Complete)
        .await
        .unwrap();
    assert_eq!(finished.status, FleetOperationStatus::Complete);
    let root = store.get_fleet_state("root-3").await.unwrap().unwrap();
    assert_eq!(root.generation, 3);
    assert_eq!(root.state, FleetRootState::Active);
    assert!(!root.admissions_sealed);
    store.close().await;
}

#[tokio::test]
async fn running_operation_can_be_recovered_after_reopen_without_retrying() {
    let sqlite = sqlite_config();
    let store = WorkflowStore::open(&sqlite).await.unwrap();
    let operation = store
        .begin_fleet_operation("root-4", FleetOperationKind::Suspend, 0, 1)
        .await
        .unwrap();
    store.close().await;

    let reopened = WorkflowStore::open(&sqlite).await.unwrap();
    let before = reopened
        .get_fleet_operation_status(&operation.operation_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(before.operation.status, FleetOperationStatus::Running);
    assert!(
        reopened
            .recover_fleet_operation(&operation.operation_id)
            .await
            .unwrap()
    );
    let after = reopened
        .get_fleet_operation_status(&operation.operation_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(after.operation.status, FleetOperationStatus::Recoverable);
    assert!(
        !reopened
            .recover_fleet_operation(&operation.operation_id)
            .await
            .unwrap()
    );
    assert_eq!(
        reopened
            .get_fleet_state("root-4")
            .await
            .unwrap()
            .unwrap()
            .active_operation_id,
        Some(operation.operation_id)
    );
    reopened.close().await;
}

#[tokio::test]
async fn close_is_sticky_and_operation_results_are_ordered_and_bounded() {
    let store = WorkflowStore::open(&sqlite_config()).await.unwrap();
    let operation = store
        .begin_fleet_operation("root-5", FleetOperationKind::Close, 0, 2)
        .await
        .unwrap();
    let mut first = member_result(&operation.operation_id, "a", 1);
    first.error = Some("redacted\nerror".to_string());
    store.record_fleet_member_result(&first).await.unwrap();
    store
        .record_fleet_member_result(&member_result(&operation.operation_id, "b", 0))
        .await
        .unwrap();
    let finished = store
        .finalize_fleet_operation(&operation.operation_id, FleetOperationStatus::Complete)
        .await
        .unwrap();
    assert_eq!(finished.status, FleetOperationStatus::Complete);
    let snapshot = store
        .get_fleet_operation_status(&operation.operation_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(snapshot.results[0].member_id, "b");
    assert_eq!(snapshot.results[1].member_id, "a");
    assert_eq!(snapshot.results[1].error.as_deref(), Some("redactederror"));

    let root = store.get_fleet_state("root-5").await.unwrap().unwrap();
    assert_eq!(root.state, FleetRootState::Closed);
    assert!(root.admissions_sealed);
    assert!(
        store
            .begin_fleet_operation("root-5", FleetOperationKind::Resume, root.generation, 0)
            .await
            .is_err()
    );
    assert_eq!(
        store
            .seal_fleet_admissions("root-5", root.generation)
            .await
            .unwrap(),
        root
    );
    store.close().await;
}
