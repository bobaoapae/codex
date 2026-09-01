use super::*;
use crate::migrations::WORKFLOW_MIGRATOR;
use crate::runtime::test_support::unique_temp_dir;
use codex_utils_absolute_path::AbsolutePathBuf;
use pretty_assertions::assert_eq;
use serde_json::json;
use sqlx::migrate::Migrator;
use std::borrow::Cow;
use std::path::Path;

fn sqlite_config(home: &Path) -> crate::SqliteConfig {
    crate::SqliteConfig::new_for_testing(
        AbsolutePathBuf::from_absolute_path(home).expect("temporary home is absolute"),
    )
}

fn message(
    message_id: &str,
    root_run_id: &str,
    recipient_run_id: &str,
    channel: WorkflowMailboxChannel,
    created_at_ms: i64,
) -> WorkflowMailboxMessageCreate {
    WorkflowMailboxMessageCreate {
        message_id: message_id.to_string(),
        root_run_id: root_run_id.to_string(),
        sender_run_id: "sender".to_string(),
        recipient_run_id: recipient_run_id.to_string(),
        channel,
        payload: json!({"message": message_id}),
        created_at_ms: Some(created_at_ms),
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
async fn mailbox_lifecycle_is_idempotent_and_survives_reopen() {
    let home = unique_temp_dir();
    let sqlite = sqlite_config(&home);
    let store = WorkflowStore::open(&sqlite)
        .await
        .expect("open workflow store");
    let input = message(
        "message-1",
        "root-1",
        "recipient-1",
        WorkflowMailboxChannel::Data,
        100,
    );
    let inserted = store
        .enqueue_mailbox_message(&input)
        .await
        .expect("enqueue mailbox message");
    assert_eq!(inserted.state, WorkflowMailboxState::Pending);
    assert_eq!(inserted.sequence, 0);
    assert_eq!(
        store
            .mailbox_depth("root-1", WorkflowMailboxChannel::Data)
            .await
            .unwrap(),
        1
    );

    let mut retry = input.clone();
    retry.created_at_ms = Some(200);
    assert_eq!(
        store.insert_mailbox_message(&retry).await.unwrap(),
        inserted
    );
    let mut divergent = input.clone();
    divergent.payload = json!({"message": "different"});
    let error = store
        .insert_mailbox_message(&divergent)
        .await
        .expect_err("divergent message id must conflict");
    assert!(matches!(
        error.downcast_ref::<WorkflowMailboxError>(),
        Some(WorkflowMailboxError::Conflict { message_id }) if message_id == "message-1"
    ));

    let claim = store
        .claim_mailbox_message(&WorkflowMailboxClaimRequest::new(
            "recipient-1",
            WorkflowMailboxChannel::Data,
            "worker-a",
            1_000,
        ))
        .await
        .expect("claim mailbox message")
        .expect("pending message");
    assert_eq!(claim.message.state, WorkflowMailboxState::Delivering);
    assert_eq!(claim.generation, 1);
    assert_eq!(claim.message.generation, 1);
    assert_eq!(claim.owner, "worker-a");
    assert!(!claim.token.is_empty());
    assert_eq!(
        claim.lease_expires_at_ms,
        claim.message.claim_expires_at_ms.unwrap()
    );

    let stale = store
        .ack_mailbox_message(&WorkflowMailboxAckRequest {
            message_id: claim.message.message_id.clone(),
            owner: "worker-a".to_string(),
            token: "stale-token".to_string(),
            generation: 0,
        })
        .await
        .expect_err("stale claim must not acknowledge");
    assert!(matches!(
        stale.downcast_ref::<WorkflowMailboxError>(),
        Some(WorkflowMailboxError::StaleClaim { message_id }) if message_id == "message-1"
    ));

    let delivered = store
        .ack_mailbox_claim(&claim)
        .await
        .expect("acknowledge claim");
    assert_eq!(delivered.state, WorkflowMailboxState::Delivered);
    assert_eq!(
        store
            .mailbox_depth("root-1", WorkflowMailboxChannel::Data)
            .await
            .unwrap(),
        0
    );
    let repeated = store
        .ack_mailbox_message(&WorkflowMailboxAckRequest {
            message_id: claim.message.message_id.clone(),
            owner: "another-worker".to_string(),
            token: "another-token".to_string(),
            generation: 999,
        })
        .await
        .expect("delivered acknowledgement is idempotent");
    assert_eq!(repeated, delivered);

    store.close().await;
    let reopened = WorkflowStore::open(&sqlite)
        .await
        .expect("reopen workflow store");
    assert_eq!(
        reopened
            .get_mailbox_message("message-1")
            .await
            .unwrap()
            .unwrap(),
        delivered
    );
    reopened.close().await;
}

#[tokio::test]
async fn mailbox_capacity_is_root_and_channel_scoped() {
    let home = unique_temp_dir();
    let store = WorkflowStore::open(&sqlite_config(&home))
        .await
        .expect("open workflow store");
    for index in 0..DEFAULT_WORKFLOW_MAILBOX_CAPACITY {
        store
            .enqueue_mailbox_message(&message(
                &format!("data-{index}"),
                "root-capacity",
                "recipient-data",
                WorkflowMailboxChannel::Data,
                i64::from(index),
            ))
            .await
            .expect("enqueue data message");
    }
    assert_eq!(
        store
            .mailbox_depth("root-capacity", WorkflowMailboxChannel::Data)
            .await
            .unwrap(),
        DEFAULT_WORKFLOW_MAILBOX_CAPACITY
    );
    let error = store
        .enqueue_mailbox_message(&message(
            "data-over-capacity",
            "root-capacity",
            "recipient-data",
            WorkflowMailboxChannel::Data,
            101,
        ))
        .await
        .expect_err("data queue must apply backpressure");
    assert!(matches!(
        error.downcast_ref::<WorkflowMailboxError>(),
        Some(WorkflowMailboxError::Backpressured { depth, capacity })
            if *depth == DEFAULT_WORKFLOW_MAILBOX_CAPACITY
                && *capacity == DEFAULT_WORKFLOW_MAILBOX_CAPACITY
    ));

    let control = store
        .enqueue_mailbox_message(&message(
            "control-1",
            "root-capacity",
            "recipient-data",
            WorkflowMailboxChannel::Control,
            102,
        ))
        .await
        .expect("control channel must not be blocked by data");
    assert_eq!(
        control.sequence,
        i64::from(DEFAULT_WORKFLOW_MAILBOX_CAPACITY)
    );
    assert_eq!(
        store
            .mailbox_depth("root-capacity", WorkflowMailboxChannel::Control)
            .await
            .unwrap(),
        1
    );
    store.close().await;
}

#[tokio::test]
async fn mailbox_claims_are_ordered_fenced_and_reclaimable_without_retry() {
    let home = unique_temp_dir();
    let store = WorkflowStore::open(&sqlite_config(&home))
        .await
        .expect("open workflow store");
    for (message_id, created_at_ms) in [("message-a", 1), ("message-b", 2)] {
        store
            .enqueue_mailbox_message(&message(
                message_id,
                "root-order",
                "recipient-order",
                WorkflowMailboxChannel::Data,
                created_at_ms,
            ))
            .await
            .expect("enqueue ordered message");
    }
    let request = WorkflowMailboxClaimRequest::new(
        "recipient-order",
        WorkflowMailboxChannel::Data,
        "worker-a",
        1,
    );
    let first = store
        .claim_mailbox_message(&request)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(first.message.message_id, "message-a");
    let pending = store
        .list_mailbox_pending(
            &WorkflowMailboxListRequest::new("recipient-order", WorkflowMailboxChannel::Data, 10)
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(
        pending
            .iter()
            .map(|message| message.message_id.as_str())
            .collect::<Vec<_>>(),
        ["message-b"]
    );

    let reclaimed = store
        .reclaim_expired_mailbox(first.lease_expires_at_ms)
        .await
        .expect("reclaim expired claim");
    assert_eq!(reclaimed.len(), 1);
    assert_eq!(reclaimed[0].message_id, "message-a");
    assert_eq!(reclaimed[0].state, WorkflowMailboxState::Pending);
    assert_eq!(reclaimed[0].generation, 2);
    let stale = store
        .ack_mailbox_claim(&first)
        .await
        .expect_err("reclaimed claim must be fenced");
    assert!(matches!(
        stale.downcast_ref::<WorkflowMailboxError>(),
        Some(WorkflowMailboxError::StaleClaim { .. })
    ));

    let second = store
        .claim_mailbox_message(&WorkflowMailboxClaimRequest::new(
            "recipient-order",
            WorkflowMailboxChannel::Data,
            "worker-b",
            1_000,
        ))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(second.message.message_id, "message-a");
    assert_eq!(second.generation, 3);
    store.close().await;
}

#[tokio::test]
async fn mailbox_rehydration_requeues_every_non_delivered_row_without_waiting_for_lease() {
    let home = unique_temp_dir();
    let store = WorkflowStore::open(&sqlite_config(&home))
        .await
        .expect("open workflow store");
    for (message_id, channel, created_at_ms) in [
        ("delivering", WorkflowMailboxChannel::Data, 1),
        ("delivered", WorkflowMailboxChannel::Data, 2),
        ("pending", WorkflowMailboxChannel::Control, 3),
    ] {
        store
            .enqueue_mailbox_message(&message(
                message_id,
                "root-rehydrate",
                "recipient-rehydrate",
                channel,
                created_at_ms,
            ))
            .await
            .expect("enqueue mailbox message");
    }
    let delivering_claim = store
        .claim_mailbox_message(&WorkflowMailboxClaimRequest::new(
            "recipient-rehydrate",
            WorkflowMailboxChannel::Data,
            "dead-process",
            86_400_000,
        ))
        .await
        .expect("claim mailbox message")
        .expect("delivering message");
    let delivered_claim = store
        .claim_mailbox_message(&WorkflowMailboxClaimRequest::new(
            "recipient-rehydrate",
            WorkflowMailboxChannel::Data,
            "live-process",
            86_400_000,
        ))
        .await
        .expect("claim second mailbox message")
        .expect("second delivering message");
    store
        .ack_mailbox_claim(&delivered_claim)
        .await
        .expect("ack delivered message");

    let recovered = store
        .requeue_undelivered_mailbox("recipient-rehydrate")
        .await
        .expect("requeue mailbox messages");
    assert_eq!(
        recovered
            .iter()
            .map(|message| message.message_id.as_str())
            .collect::<Vec<_>>(),
        ["delivering", "pending"]
    );
    assert_eq!(
        recovered
            .iter()
            .map(|message| (
                message.state,
                message.claim_owner.clone(),
                message.acked_at_ms
            ))
            .collect::<Vec<_>>(),
        [
            (WorkflowMailboxState::Pending, None, None),
            (WorkflowMailboxState::Pending, None, None),
        ]
    );
    assert_eq!(
        recovered
            .iter()
            .map(|message| (message.message_id.as_str(), message.generation))
            .collect::<Vec<_>>(),
        [("delivering", 2), ("pending", 0)]
    );
    assert_eq!(
        store
            .get_mailbox_message("delivered")
            .await
            .expect("read delivered message")
            .expect("delivered row")
            .state,
        WorkflowMailboxState::Delivered
    );

    let pending = store
        .list_mailbox_pending(
            &WorkflowMailboxListRequest::new(
                "recipient-rehydrate",
                WorkflowMailboxChannel::Data,
                10,
            )
            .unwrap(),
        )
        .await
        .expect("list requeued data messages");
    assert_eq!(
        pending
            .iter()
            .map(|message| message.message_id.as_str())
            .collect::<Vec<_>>(),
        ["delivering"]
    );
    assert_eq!(delivering_claim.message.message_id, "delivering");
    store.close().await;
}

#[tokio::test]
async fn mailbox_rehydration_is_idempotent_after_delivery_and_does_not_redeliver_delivered_rows() {
    let home = unique_temp_dir();
    let sqlite = sqlite_config(&home);
    let store = WorkflowStore::open(&sqlite)
        .await
        .expect("open workflow store");
    store
        .enqueue_mailbox_message(&message(
            "only-message",
            "root-rehydrate-idempotent",
            "recipient-rehydrate-idempotent",
            WorkflowMailboxChannel::Data,
            1,
        ))
        .await
        .expect("enqueue mailbox message");
    let recovered = store
        .requeue_undelivered_mailbox("recipient-rehydrate-idempotent")
        .await
        .expect("requeue mailbox message");
    assert_eq!(recovered.len(), 1);
    let claim = store
        .claim_mailbox_message(&WorkflowMailboxClaimRequest::new(
            "recipient-rehydrate-idempotent",
            WorkflowMailboxChannel::Data,
            "new-process",
            86_400_000,
        ))
        .await
        .expect("claim requeued message")
        .expect("requeued message");
    store
        .ack_mailbox_claim(&claim)
        .await
        .expect("ack requeued message");
    assert!(
        store
            .requeue_undelivered_mailbox("recipient-rehydrate-idempotent")
            .await
            .expect("repeat rehydration")
            .is_empty()
    );
    assert!(
        store
            .claim_mailbox_message(&WorkflowMailboxClaimRequest::new(
                "recipient-rehydrate-idempotent",
                WorkflowMailboxChannel::Data,
                "new-process",
                86_400_000,
            ))
            .await
            .expect("claim after delivery")
            .is_none()
    );
    store.close().await;
}

#[tokio::test]
async fn expired_mailbox_reclaim_can_be_scoped_to_one_recipient() {
    let home = unique_temp_dir();
    let store = WorkflowStore::open(&sqlite_config(&home))
        .await
        .expect("open workflow store");
    for recipient_run_id in ["recipient-a", "recipient-b"] {
        store
            .enqueue_mailbox_message(&message(
                recipient_run_id,
                "root-reclaim-scope",
                recipient_run_id,
                WorkflowMailboxChannel::Data,
                1,
            ))
            .await
            .expect("enqueue mailbox message");
    }
    let claim_a = store
        .claim_mailbox_message(&WorkflowMailboxClaimRequest::new(
            "recipient-a",
            WorkflowMailboxChannel::Data,
            "worker-a",
            1_000,
        ))
        .await
        .expect("claim recipient a")
        .expect("recipient a message");
    let claim_b = store
        .claim_mailbox_message(&WorkflowMailboxClaimRequest::new(
            "recipient-b",
            WorkflowMailboxChannel::Data,
            "worker-b",
            1_000,
        ))
        .await
        .expect("claim recipient b")
        .expect("recipient b message");

    let reclaimed = store
        .reclaim_expired_mailbox_for_recipient("recipient-a", i64::MAX)
        .await
        .expect("reclaim recipient a");
    assert_eq!(
        reclaimed
            .iter()
            .map(|message| message.message_id.as_str())
            .collect::<Vec<_>>(),
        ["recipient-a"]
    );
    assert_eq!(
        store
            .get_mailbox_message("recipient-b")
            .await
            .expect("read recipient b")
            .expect("recipient b row")
            .state,
        WorkflowMailboxState::Delivering
    );
    assert_eq!(claim_a.message.message_id, "recipient-a");
    assert_eq!(claim_b.message.message_id, "recipient-b");
    store.close().await;
}

#[tokio::test]
async fn concurrent_mailbox_claims_do_not_duplicate_messages() {
    let home = unique_temp_dir();
    let store = WorkflowStore::open(&sqlite_config(&home))
        .await
        .expect("open workflow store");
    for message_id in ["message-a", "message-b"] {
        store
            .enqueue_mailbox_message(&message(
                message_id,
                "root-concurrent",
                "recipient-concurrent",
                WorkflowMailboxChannel::Data,
                1,
            ))
            .await
            .expect("enqueue concurrent message");
    }
    let first_store = store.clone();
    let second_store = store.clone();
    let first_request = WorkflowMailboxClaimRequest::new(
        "recipient-concurrent",
        WorkflowMailboxChannel::Data,
        "worker-a",
        1_000,
    );
    let second_request = WorkflowMailboxClaimRequest::new(
        "recipient-concurrent",
        WorkflowMailboxChannel::Data,
        "worker-b",
        1_000,
    );
    let (first, second) = tokio::join!(
        first_store.claim_mailbox_message(&first_request),
        second_store.claim_mailbox_message(&second_request),
    );
    let first = first.unwrap().unwrap();
    let second = second.unwrap().unwrap();
    assert_ne!(first.message.message_id, second.message.message_id);
    assert_eq!(first.message.sequence, 0);
    assert_eq!(second.message.sequence, 1);
    store.close().await;
}

#[tokio::test]
async fn concurrent_mailbox_enqueues_allocate_monotonic_sequences() {
    let home = unique_temp_dir();
    let store = WorkflowStore::open(&sqlite_config(&home))
        .await
        .expect("open workflow store");
    let first_store = store.clone();
    let second_store = store.clone();
    let first = message(
        "concurrent-a",
        "root-enqueue",
        "recipient-enqueue",
        WorkflowMailboxChannel::Data,
        1,
    );
    let second = message(
        "concurrent-b",
        "root-enqueue",
        "recipient-enqueue",
        WorkflowMailboxChannel::Data,
        2,
    );
    let (first, second) = tokio::join!(
        first_store.enqueue_mailbox_message(&first),
        second_store.enqueue_mailbox_message(&second),
    );
    assert!(first.is_ok());
    assert!(second.is_ok());
    let pending = store
        .list_mailbox_pending(
            &WorkflowMailboxListRequest::new("recipient-enqueue", WorkflowMailboxChannel::Data, 10)
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(
        pending
            .iter()
            .map(|message| message.sequence)
            .collect::<Vec<_>>(),
        [0, 1]
    );
    store.close().await;
}

#[tokio::test]
async fn mailbox_rejects_payloads_over_64_kibibytes() {
    let home = unique_temp_dir();
    let store = WorkflowStore::open(&sqlite_config(&home))
        .await
        .expect("open workflow store");
    let mut oversized = message(
        "oversized",
        "root-payload",
        "recipient-payload",
        WorkflowMailboxChannel::Data,
        1,
    );
    oversized.payload = json!({"body": "x".repeat(66_000)});
    assert!(store.enqueue_mailbox_message(&oversized).await.is_err());
    assert_eq!(
        store
            .mailbox_depth("root-payload", WorkflowMailboxChannel::Data)
            .await
            .unwrap(),
        0
    );
    store.close().await;
}

#[tokio::test]
async fn mailbox_migration_maps_legacy_states_without_loss() {
    let home = unique_temp_dir();
    let sqlite = sqlite_config(&home);
    tokio::fs::create_dir_all(&home)
        .await
        .expect("create workflow sqlite home");
    let pool = sqlite
        .open_workflow_db(&migration_through(2), None)
        .await
        .expect("open legacy workflow schema");
    for (message_id, state, sequence) in [
        ("legacy-pending", "pending", 0_i64),
        ("legacy-claimed", "claimed", 1_i64),
        ("legacy-acked", "acked", 2_i64),
    ] {
        sqlx::query(
            "INSERT INTO workflow_mailbox
             (message_id, root_run_id, sender_run_id, recipient_run_id, sequence,
              channel, state, payload_json, created_at_ms, claim_owner, claim_token,
              claim_expires_at_ms, acked_at_ms)
             VALUES (?, 'legacy-root', 'legacy-sender', 'legacy-recipient', ?,
                     'data', ?, ?, ?, 'legacy-owner', 'legacy-token', 50, 60)",
        )
        .bind(message_id)
        .bind(sequence)
        .bind(state)
        .bind(format!(r#"{{"message":"{message_id}"}}"#))
        .bind(sequence)
        .execute(&pool)
        .await
        .expect("insert legacy mailbox row");
    }
    pool.close().await;

    let store = WorkflowStore::open(&sqlite)
        .await
        .expect("migrate workflow schema");
    let pending = store
        .get_mailbox_message("legacy-pending")
        .await
        .unwrap()
        .unwrap();
    let delivering = store
        .get_mailbox_message("legacy-claimed")
        .await
        .unwrap()
        .unwrap();
    let delivered = store
        .get_mailbox_message("legacy-acked")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(pending.state, WorkflowMailboxState::Pending);
    assert_eq!(delivering.state, WorkflowMailboxState::Delivering);
    assert_eq!(delivered.state, WorkflowMailboxState::Delivered);
    assert_eq!(pending.payload, json!({"message": "legacy-pending"}));
    assert_eq!(delivering.claim_owner.as_deref(), Some("legacy-owner"));
    assert_eq!(delivered.acked_at_ms, Some(60));
    assert_eq!(
        store
            .mailbox_depth("legacy-root", WorkflowMailboxChannel::Data)
            .await
            .unwrap(),
        2
    );
    store.close().await;
}

#[tokio::test]
async fn mailbox_apply_receipt_is_fenced_idempotent_and_survives_requeue() {
    let home = unique_temp_dir();
    let store = WorkflowStore::open(&sqlite_config(&home))
        .await
        .expect("open workflow store");
    store
        .enqueue_mailbox_message(&message(
            "apply-1",
            "root-apply",
            "recipient-apply",
            WorkflowMailboxChannel::Data,
            1,
        ))
        .await
        .expect("enqueue message");
    let claim = store
        .claim_mailbox_message(&WorkflowMailboxClaimRequest::new(
            "recipient-apply",
            WorkflowMailboxChannel::Data,
            "worker-apply",
            60_000,
        ))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(claim.message.applied_at_ms, None);

    let wrong_token = store
        .mark_mailbox_applied(&WorkflowMailboxAckRequest {
            message_id: "apply-1".to_string(),
            owner: claim.owner.clone(),
            token: "not-the-token".to_string(),
            generation: claim.generation,
        })
        .await
        .expect_err("wrong token must be fenced");
    assert!(matches!(
        wrong_token.downcast_ref::<WorkflowMailboxError>(),
        Some(WorkflowMailboxError::StaleClaim { .. })
    ));
    let wrong_generation = store
        .mark_mailbox_applied(&WorkflowMailboxAckRequest {
            message_id: "apply-1".to_string(),
            owner: claim.owner.clone(),
            token: claim.token.clone(),
            generation: claim.generation + 1,
        })
        .await
        .expect_err("wrong generation must be fenced");
    assert!(matches!(
        wrong_generation.downcast_ref::<WorkflowMailboxError>(),
        Some(WorkflowMailboxError::StaleClaim { .. })
    ));

    let request = WorkflowMailboxAckRequest {
        message_id: "apply-1".to_string(),
        owner: claim.owner.clone(),
        token: claim.token.clone(),
        generation: claim.generation,
    };
    let applied = store
        .mark_mailbox_applied(&request)
        .await
        .expect("apply under the live fence");
    let first_receipt = applied.applied_at_ms.expect("receipt recorded");
    let reapplied = store
        .mark_mailbox_applied(&request)
        .await
        .expect("repeated apply is idempotent");
    assert_eq!(reapplied.applied_at_ms, Some(first_receipt));

    // Session rehydration resets delivery state but must keep the receipt.
    let requeued = store
        .requeue_undelivered_mailbox("recipient-apply")
        .await
        .expect("requeue non-delivered rows");
    assert_eq!(requeued.len(), 1);
    assert_eq!(requeued[0].state, WorkflowMailboxState::Pending);
    assert_eq!(requeued[0].applied_at_ms, Some(first_receipt));

    // Expired-claim reclaims must keep the receipt too.
    let reclaim_lease = store
        .claim_mailbox_message(&WorkflowMailboxClaimRequest::new(
            "recipient-apply",
            WorkflowMailboxChannel::Data,
            "worker-reclaim",
            1,
        ))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(reclaim_lease.message.applied_at_ms, Some(first_receipt));
    let reclaimed = store
        .reclaim_expired_mailbox(reclaim_lease.lease_expires_at_ms)
        .await
        .expect("reclaim expired claim");
    assert_eq!(reclaimed.len(), 1);
    assert_eq!(reclaimed[0].applied_at_ms, Some(first_receipt));

    // The apply fence is the claim identity, not the lease clock: an apply
    // after lease expiry succeeds while the matching ack stays fenced.
    let expiring = store
        .claim_mailbox_message(&WorkflowMailboxClaimRequest::new(
            "recipient-apply",
            WorkflowMailboxChannel::Data,
            "worker-late",
            1,
        ))
        .await
        .unwrap()
        .unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    let late_request = WorkflowMailboxAckRequest {
        message_id: "apply-1".to_string(),
        owner: expiring.owner.clone(),
        token: expiring.token.clone(),
        generation: expiring.generation,
    };
    let late_apply = store
        .mark_mailbox_applied(&late_request)
        .await
        .expect("apply succeeds after lease expiry");
    assert_eq!(late_apply.applied_at_ms, Some(first_receipt));
    let late_ack = store
        .ack_mailbox_message(&late_request)
        .await
        .expect_err("ack stays fenced by the lease clock");
    assert!(matches!(
        late_ack.downcast_ref::<WorkflowMailboxError>(),
        Some(WorkflowMailboxError::StaleClaim { .. })
    ));

    // Reclaim, re-claim with a fresh lease, ack; the delivered row keeps the
    // original receipt and further applies are idempotent no-ops.
    store
        .reclaim_expired_mailbox(expiring.lease_expires_at_ms + 100)
        .await
        .expect("reclaim late claim");
    let final_claim = store
        .claim_mailbox_message(&WorkflowMailboxClaimRequest::new(
            "recipient-apply",
            WorkflowMailboxChannel::Data,
            "worker-final",
            60_000,
        ))
        .await
        .unwrap()
        .unwrap();
    let delivered = store
        .ack_mailbox_claim(&final_claim)
        .await
        .expect("ack under a fresh lease");
    assert_eq!(delivered.state, WorkflowMailboxState::Delivered);
    assert_eq!(delivered.applied_at_ms, Some(first_receipt));
    let after_delivery = store
        .mark_mailbox_applied(&late_request)
        .await
        .expect("apply on a delivered row is idempotent");
    assert_eq!(after_delivery.state, WorkflowMailboxState::Delivered);
    store.close().await;
}

#[tokio::test]
async fn mailbox_migration_adds_apply_receipt_column_without_loss() {
    let home = unique_temp_dir();
    let sqlite = sqlite_config(&home);
    tokio::fs::create_dir_all(&home)
        .await
        .expect("create workflow sqlite home");
    let pool = sqlite
        .open_workflow_db(&migration_through(9), None)
        .await
        .expect("open pre-receipt workflow schema");
    sqlx::query(
        "INSERT INTO workflow_mailbox
         (message_id, root_run_id, sender_run_id, recipient_run_id, sequence,
          channel, state, payload_json, created_at_ms, generation)
         VALUES ('pre-receipt', 'pre-root', 'pre-sender', 'pre-recipient', 0,
                 'data', 'pending', '{\"message\":\"pre-receipt\"}', 10, 0)",
    )
    .execute(&pool)
    .await
    .expect("insert pre-receipt mailbox row");
    pool.close().await;

    let store = WorkflowStore::open(&sqlite)
        .await
        .expect("migrate workflow schema");
    let migrated = store
        .get_mailbox_message("pre-receipt")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(migrated.state, WorkflowMailboxState::Pending);
    assert_eq!(migrated.applied_at_ms, None);
    assert_eq!(migrated.payload, json!({"message": "pre-receipt"}));
    store.close().await;
}
