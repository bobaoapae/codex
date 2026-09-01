use super::*;
use crate::migrations::WORKFLOW_MIGRATOR;
use crate::runtime::test_support::unique_temp_dir;
use codex_utils_absolute_path::test_support::PathExt;
use pretty_assertions::assert_eq;
use sqlx::migrate::Migrator;
use std::borrow::Cow;

fn sqlite_config(home: &std::path::Path) -> crate::SqliteConfig {
    crate::SqliteConfig::new_for_testing(home.abs())
}

fn watermark() -> WorkflowBackfillWatermark {
    WorkflowBackfillWatermark::new(100, "rollout-100").expect("watermark")
}

fn journal(rollout_id: &str, source_path: &str) -> WorkflowBackfillJournalCreate {
    WorkflowBackfillJournalCreate {
        rollout_id: rollout_id.to_string(),
        source_path: source_path.to_string(),
        source_size_bytes: Some(10),
        source_mtime_ms: Some(20),
    }
}

fn begin(owner_id: &str) -> WorkflowBackfillBeginRequest {
    WorkflowBackfillBeginRequest {
        watermark: watermark(),
        owner_id: owner_id.to_string(),
        lease_duration_ms: 60_000,
    }
}

fn update(
    claim: &WorkflowBackfillJournalClaim,
    source_path: &str,
    status: WorkflowBackfillJournalStatus,
) -> WorkflowBackfillJournalUpdate {
    WorkflowBackfillJournalUpdate {
        rollout_id: claim.entry.rollout_id.clone(),
        owner_id: claim.owner_id.clone(),
        token: claim.token.clone(),
        generation: claim.generation,
        source_path: source_path.to_string(),
        byte_offset: 50,
        rollout_ordinal: 3,
        status,
        error: None,
        generation_id: Some(9),
        cursor_json: Some(r#"{"ordinal":3}"#.to_string()),
        source_size_bytes: Some(30),
        source_mtime_ms: Some(40),
        lease_duration_ms: 60_000,
    }
}

fn migration_through(version: i64) -> Migrator {
    Migrator {
        migrations: Cow::Owned(
            WORKFLOW_MIGRATOR
                .migrations
                .iter()
                .filter(|migration| migration.version <= version)
                .cloned()
                .collect(),
        ),
        ignore_missing: WORKFLOW_MIGRATOR.ignore_missing,
        locking: WORKFLOW_MIGRATOR.locking,
        table_name: WORKFLOW_MIGRATOR.table_name.clone(),
        create_schemas: WORKFLOW_MIGRATOR.create_schemas.clone(),
        no_tx: WORKFLOW_MIGRATOR.no_tx,
    }
}

#[tokio::test]
async fn backfill_claims_are_fenced_and_finalize_is_idempotent_after_reopen() {
    let home = unique_temp_dir();
    let sqlite = sqlite_config(&home);
    let store = WorkflowStore::open(&sqlite)
        .await
        .expect("open workflow store");
    let claim = store.begin_backfill(&begin("owner-a")).await.unwrap();
    assert_eq!(claim.watermark, watermark());
    assert!(matches!(
        store.begin_backfill(&begin("owner-b")).await,
        Err(error) if error.downcast_ref::<WorkflowBackfillError>() == Some(&WorkflowBackfillError::Busy)
    ));

    let registered = store
        .register_backfill_rollout(&journal("rollout-a", "a.jsonl"))
        .await
        .unwrap();
    assert_eq!(registered.status, WorkflowBackfillJournalStatus::Pending);
    let journal_claim = store
        .claim_backfill_journal(&WorkflowBackfillJournalClaimRequest {
            rollout_id: registered.rollout_id.clone(),
            owner_id: "owner-a".to_string(),
            lease_duration_ms: 60_000,
        })
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        journal_claim.entry.status,
        WorkflowBackfillJournalStatus::Processing
    );

    let updated = store
        .update_backfill_journal(&update(
            &journal_claim,
            "a.jsonl",
            WorkflowBackfillJournalStatus::Complete,
        ))
        .await
        .unwrap();
    assert_eq!(updated.byte_offset, 50);
    assert_eq!(updated.status, WorkflowBackfillJournalStatus::Complete);
    let stale = store
        .update_backfill_journal(&update(
            &journal_claim,
            "a.jsonl",
            WorkflowBackfillJournalStatus::Complete,
        ))
        .await
        .expect_err("old journal fence must fail");
    assert!(matches!(
        stale.downcast_ref::<WorkflowBackfillError>(),
        Some(WorkflowBackfillError::Stale)
    ));

    let final_state = store
        .finalize_backfill(&WorkflowBackfillFinalizeRequest {
            owner_id: claim.owner_id.clone(),
            token: claim.token.clone(),
            generation: claim.generation,
        })
        .await
        .unwrap();
    assert_eq!(final_state.status, WorkflowBackfillStatus::Complete);
    store.close().await;

    let reopened = WorkflowStore::open(&sqlite)
        .await
        .expect("reopen workflow store");
    assert_eq!(
        reopened
            .get_backfill_coordinator_state()
            .await
            .unwrap()
            .status,
        WorkflowBackfillStatus::Complete
    );
    let duplicate = reopened
        .register_backfill_rollout(&journal("rollout-a", "a.zst"))
        .await
        .unwrap();
    assert_eq!(duplicate.source_path, "a.zst");
    assert_eq!(reopened.list_backfill_journal().await.unwrap().len(), 1);
    reopened.close().await;
}

#[tokio::test]
async fn pending_failed_and_recoverable_journal_rows_block_finalize_until_permanent_outcomes() {
    let home = unique_temp_dir();
    let store = WorkflowStore::open(&sqlite_config(&home))
        .await
        .expect("open workflow store");
    let claim = store.begin_backfill(&begin("owner")).await.unwrap();
    for rollout_id in ["pending", "skipped"] {
        store
            .register_backfill_rollout(&journal(rollout_id, &format!("{rollout_id}.jsonl")))
            .await
            .unwrap();
    }
    let skipped_claim = store
        .claim_backfill_journal(&WorkflowBackfillJournalClaimRequest {
            rollout_id: "skipped".to_string(),
            owner_id: "worker".to_string(),
            lease_duration_ms: 60_000,
        })
        .await
        .unwrap()
        .unwrap();
    store
        .update_backfill_journal(&update(
            &skipped_claim,
            "skipped.jsonl",
            WorkflowBackfillJournalStatus::SkippedPermanent,
        ))
        .await
        .unwrap();
    let blocked = store
        .finalize_backfill(&WorkflowBackfillFinalizeRequest {
            owner_id: claim.owner_id.clone(),
            token: claim.token.clone(),
            generation: claim.generation,
        })
        .await
        .expect_err("pending work must block publication");
    assert!(matches!(
        blocked.downcast_ref::<WorkflowBackfillError>(),
        Some(WorkflowBackfillError::PendingWork { pending: 1, .. })
    ));

    let pending_claim = store
        .claim_backfill_journal(&WorkflowBackfillJournalClaimRequest {
            rollout_id: "pending".to_string(),
            owner_id: "worker".to_string(),
            lease_duration_ms: 60_000,
        })
        .await
        .unwrap()
        .unwrap();
    store
        .update_backfill_journal(&update(
            &pending_claim,
            "pending.jsonl",
            WorkflowBackfillJournalStatus::Complete,
        ))
        .await
        .unwrap();
    let completed = store
        .finalize_backfill(&WorkflowBackfillFinalizeRequest {
            owner_id: claim.owner_id,
            token: claim.token,
            generation: claim.generation,
        })
        .await
        .unwrap();
    assert_eq!(completed.status, WorkflowBackfillStatus::Complete);
    store.close().await;
}

#[tokio::test]
async fn expired_journal_claim_is_recoverable_and_source_rename_keeps_rollout_identity() {
    let home = unique_temp_dir();
    let store = WorkflowStore::open(&sqlite_config(&home))
        .await
        .expect("open workflow store");
    let coordinator = store.begin_backfill(&begin("owner")).await.unwrap();
    store
        .register_backfill_rollout(&journal("rollout-zst", "rollout.jsonl"))
        .await
        .unwrap();
    let claim = store
        .claim_backfill_journal(&WorkflowBackfillJournalClaimRequest {
            rollout_id: "rollout-zst".to_string(),
            owner_id: "worker".to_string(),
            lease_duration_ms: 1,
        })
        .await
        .unwrap()
        .unwrap();
    let renamed = store
        .register_backfill_rollout(&journal("rollout-zst", "rollout.jsonl.zst"))
        .await
        .unwrap();
    assert_eq!(renamed.source_path, "rollout.jsonl");
    let reclaimed = store
        .reclaim_expired_backfill_journal(claim.entry.lease_expires_at_ms.unwrap())
        .await
        .unwrap();
    assert_eq!(reclaimed.len(), 1);
    assert_eq!(
        reclaimed[0].status,
        WorkflowBackfillJournalStatus::Recoverable
    );
    let stale = store
        .update_backfill_journal(&update(
            &claim,
            "rollout.jsonl.zst",
            WorkflowBackfillJournalStatus::Complete,
        ))
        .await
        .expect_err("reclaimed worker must be fenced");
    assert!(matches!(
        stale.downcast_ref::<WorkflowBackfillError>(),
        Some(WorkflowBackfillError::Stale)
    ));
    let new_claim = store
        .claim_backfill_journal(&WorkflowBackfillJournalClaimRequest {
            rollout_id: "rollout-zst".to_string(),
            owner_id: "worker-2".to_string(),
            lease_duration_ms: 60_000,
        })
        .await
        .unwrap()
        .unwrap();
    store
        .update_backfill_journal(&update(
            &new_claim,
            "rollout.jsonl.zst",
            WorkflowBackfillJournalStatus::Complete,
        ))
        .await
        .unwrap();
    assert_eq!(
        store
            .get_backfill_journal("rollout-zst")
            .await
            .unwrap()
            .unwrap()
            .source_path,
        "rollout.jsonl.zst"
    );
    let incremental = store
        .get_incremental_backfill_state()
        .await
        .expect("incremental state");
    assert_eq!(incremental.status, WorkflowBackfillStatus::Pending);
    let incremental = store
        .request_incremental_backfill(&watermark())
        .await
        .expect("request incremental capture");
    assert_eq!(incremental.watermark, Some(watermark()));
    assert_eq!(coordinator.watermark, watermark());
    store.close().await;
}

#[tokio::test]
async fn concurrent_backfill_begin_claims_have_one_winner() {
    let home = unique_temp_dir();
    let store = WorkflowStore::open(&sqlite_config(&home))
        .await
        .expect("open workflow store");
    let first_store = store.clone();
    let second_store = store.clone();
    let first_request = begin("owner-a");
    let second_request = begin("owner-b");
    let (first, second) = tokio::join!(
        first_store.begin_backfill(&first_request),
        second_store.begin_backfill(&second_request),
    );
    assert!(first.is_ok() ^ second.is_ok());
    store.close().await;
}

#[tokio::test]
async fn backfill_migration_preserves_legacy_running_rows() {
    let home = unique_temp_dir();
    let sqlite = sqlite_config(&home);
    tokio::fs::create_dir_all(&home)
        .await
        .expect("create workflow sqlite home");
    let pool = sqlite
        .open_workflow_db(&migration_through(5), None)
        .await
        .expect("open legacy workflow schema");
    sqlx::query(
        "UPDATE workflow_backfill_state
         SET status = 'running', watermark_created_at_ms = 10,
             watermark_rollout_id = 'legacy-rollout', owner_id = 'legacy-owner'",
    )
    .execute(&pool)
    .await
    .expect("seed legacy coordinator");
    sqlx::query(
        "INSERT INTO workflow_backfill_journal
         (rollout_id, source_path, byte_offset, rollout_ordinal, status,
          updated_at_ms, owner_id, source_size_bytes, source_mtime_ms)
         VALUES ('legacy-rollout', 'legacy.jsonl', 4, 2, 'running', 10,
                 'legacy-owner', 20, 30)",
    )
    .execute(&pool)
    .await
    .expect("seed legacy journal");
    pool.close().await;

    let store = WorkflowStore::open(&sqlite)
        .await
        .expect("migrate workflow schema");
    assert_eq!(
        store.get_backfill_coordinator_state().await.unwrap().status,
        WorkflowBackfillStatus::Processing
    );
    assert_eq!(
        store
            .get_backfill_journal("legacy-rollout")
            .await
            .unwrap()
            .unwrap()
            .status,
        WorkflowBackfillJournalStatus::Processing
    );
    store.close().await;
}
