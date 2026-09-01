use super::*;
use crate::SqliteConfig;
use crate::runtime::test_support::unique_temp_dir;
use codex_utils_absolute_path::AbsolutePathBuf;
use pretty_assertions::assert_eq;
use serde_json::json;
use std::path::Path;

fn sqlite_config(home: &Path) -> SqliteConfig {
    SqliteConfig::new_for_testing(
        AbsolutePathBuf::from_absolute_path(home).expect("temporary home is absolute"),
    )
}

fn receipt(receipt_id: &str, created_at_ms: i64) -> WorkflowReceiptCreate {
    WorkflowReceiptCreate {
        receipt_id: receipt_id.to_string(),
        run_id: None,
        thread_id: Some("thread-a".to_string()),
        turn_id: Some("turn-a".to_string()),
        job_id: Some("job-a".to_string()),
        plan_snapshot_id: Some("plan-a".to_string()),
        schema_version: 7,
        kind: "test.receipt".to_string(),
        subject: "test subject".to_string(),
        status: "pass".to_string(),
        source: "test".to_string(),
        provenance: Some(json!({"source": "unit-test"})),
        tags: vec![WorkflowReceiptTag {
            key: "scope".to_string(),
            value: "focused".to_string(),
        }],
        payload: Some(json!({"attempt": 1})),
        references: vec![WorkflowReceiptReference {
            kind: "rolloutItem".to_string(),
            id: "item-a".to_string(),
        }],
        created_at_ms: Some(created_at_ms),
    }
}

#[tokio::test]
async fn insert_is_idempotent_for_same_content_and_conflicts_on_different_content() {
    let home = unique_temp_dir();
    let sqlite = sqlite_config(&home);
    let store = WorkflowStore::open(&sqlite)
        .await
        .expect("open workflow store");
    let first_input = receipt("receipt-1", 100);
    let first = store
        .insert_receipt(&first_input)
        .await
        .expect("insert receipt");

    let mut retry_input = first_input.clone();
    retry_input.created_at_ms = Some(200);
    assert_eq!(
        store
            .insert_receipt(&retry_input)
            .await
            .expect("idempotent retry"),
        first
    );

    let mut divergent_input = first_input;
    divergent_input.status = "failed".to_string();
    let error = store
        .insert_receipt(&divergent_input)
        .await
        .expect_err("different receipt content must conflict");
    assert!(error.to_string().contains("different content"));
    store.close().await;
}

#[tokio::test]
async fn filters_and_keyset_cursor_are_stable_and_filter_bound() {
    let home = unique_temp_dir();
    let store = WorkflowStore::open(&sqlite_config(&home))
        .await
        .expect("open workflow store");
    for (receipt_id, created_at_ms) in [("receipt-a", 100), ("receipt-b", 100)] {
        store
            .insert_receipt(&receipt(receipt_id, created_at_ms))
            .await
            .expect("insert receipt");
    }
    let mut other = receipt("receipt-c", 200);
    other.thread_id = Some("thread-b".to_string());
    other.job_id = Some("job-b".to_string());
    other.plan_snapshot_id = Some("plan-b".to_string());
    other.status = "blocked".to_string();
    other.kind = "other.receipt".to_string();
    store
        .insert_receipt(&other)
        .await
        .expect("insert other receipt");

    let filter = WorkflowReceiptFilter {
        thread_id: Some("thread-a".to_string()),
        job_id: Some("job-a".to_string()),
        plan_snapshot_id: Some("plan-a".to_string()),
        status: Some("pass".to_string()),
        kind: Some("test.receipt".to_string()),
    };
    let first_page = store
        .list_receipts(&WorkflowReceiptListRequest::new(filter.clone(), None, 1).unwrap())
        .await
        .expect("list first page");
    assert_eq!(first_page.receipts.len(), 1);
    assert_eq!(first_page.receipts[0].receipt_id, "receipt-b");
    let cursor = first_page.next_cursor.expect("next cursor");
    assert_eq!(cursor.filter, filter);

    let second_page = store
        .list_receipts(
            &WorkflowReceiptListRequest::new(filter.clone(), Some(cursor.clone()), 1).unwrap(),
        )
        .await
        .expect("list second page");
    assert_eq!(
        second_page
            .receipts
            .iter()
            .map(|receipt| receipt.receipt_id.as_str())
            .collect::<Vec<_>>(),
        ["receipt-a"]
    );
    assert!(second_page.next_cursor.is_none());

    let mut mismatched_filter = filter;
    mismatched_filter.kind = Some("other.receipt".to_string());
    assert!(
        WorkflowReceiptListRequest::new(mismatched_filter, Some(cursor), 1).is_err(),
        "cursor must be bound to the original filters"
    );
    store.close().await;
}

#[tokio::test]
async fn unknown_schema_and_kind_round_trip_after_reopen_and_export_is_explicit() {
    let home = unique_temp_dir();
    let sqlite = sqlite_config(&home);
    let store = WorkflowStore::open(&sqlite)
        .await
        .expect("open workflow store");
    let mut input = receipt("receipt-future", 300);
    input.schema_version = 2_147_483_000;
    input.kind = "future.receipt.v9".to_string();
    let inserted = store
        .insert_receipt(&input)
        .await
        .expect("insert future receipt");
    assert_eq!(inserted.schema_version, 2_147_483_000);
    assert_eq!(inserted.kind, "future.receipt.v9");
    store.close().await;

    let reopened = WorkflowStore::open(&sqlite)
        .await
        .expect("reopen workflow store");
    let read = reopened
        .get_receipt("receipt-future")
        .await
        .expect("read receipt")
        .expect("receipt exists after reopen");
    assert_eq!(read, inserted);
    assert_eq!(
        reopened
            .select_receipts_for_export(&WorkflowReceiptExportSelection {
                receipt_ids: vec!["receipt-future".to_string()],
            })
            .await
            .expect("select explicit receipt"),
        vec![inserted]
    );
    assert!(
        reopened
            .select_receipts_for_export(&WorkflowReceiptExportSelection {
                receipt_ids: Vec::new(),
            })
            .await
            .is_err()
    );
    reopened.close().await;
}

#[tokio::test]
async fn validation_rejects_unbounded_tags_payload_and_references() {
    let home = unique_temp_dir();
    let store = WorkflowStore::open(&sqlite_config(&home))
        .await
        .expect("open workflow store");

    let mut too_many_tags = receipt("receipt-tags", 400);
    too_many_tags.tags = (0..33)
        .map(|index| WorkflowReceiptTag {
            key: format!("tag-{index}"),
            value: "value".to_string(),
        })
        .collect();
    assert!(store.insert_receipt(&too_many_tags).await.is_err());

    let mut long_tag = receipt("receipt-long-tag", 401);
    long_tag.tags[0].key = "k".repeat(65);
    assert!(store.insert_receipt(&long_tag).await.is_err());
    long_tag.tags[0].key = "key".to_string();
    long_tag.tags[0].value = "v".repeat(257);
    assert!(store.insert_receipt(&long_tag).await.is_err());

    let mut duplicate_tags = receipt("receipt-duplicate-tags", 405);
    duplicate_tags.tags.push(WorkflowReceiptTag {
        key: "scope".to_string(),
        value: "again".to_string(),
    });
    assert!(store.insert_receipt(&duplicate_tags).await.is_err());

    let mut large_payload = receipt("receipt-large-payload", 402);
    large_payload.payload = Some(json!({"metadata": "x".repeat(66_000)}));
    assert!(store.insert_receipt(&large_payload).await.is_err());

    let mut raw_payload = receipt("receipt-raw-payload", 406);
    raw_payload.payload = Some(json!({"stdout": "must not persist"}));
    assert!(store.insert_receipt(&raw_payload).await.is_err());

    let mut long_reference = receipt("receipt-long-reference", 403);
    long_reference.references[0].id = "r".repeat(257);
    assert!(store.insert_receipt(&long_reference).await.is_err());

    let mut nul_subject = receipt("receipt-nul", 404);
    nul_subject.subject = "bad\0subject".to_string();
    assert!(store.insert_receipt(&nul_subject).await.is_err());

    assert!(
        store
            .select_receipts_for_export(&WorkflowReceiptExportSelection {
                receipt_ids: vec!["duplicate".to_string(), "duplicate".to_string()],
            })
            .await
            .is_err()
    );
    store.close().await;
}
