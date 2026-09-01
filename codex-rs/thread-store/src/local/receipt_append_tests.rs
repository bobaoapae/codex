use crate::AppendReceiptOutcome;
use crate::AppendReceiptParams;
use crate::CreateThreadParams;
use crate::PersistContext;
use crate::ResumeThreadParams;
use crate::ThreadPersistenceMetadata;
use crate::ThreadStore;
use crate::ThreadStoreError;
use codex_extension_items::ExtensionItem;
use codex_extension_items::receipt::ReceiptAttachedItem;
use codex_extension_items::receipt::ReceiptStatus;
use codex_protocol::ThreadId;
use codex_protocol::items::TurnItem;
use codex_protocol::models::BaseInstructions;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::SessionSource;
use codex_protocol::protocol::ThreadHistoryMode;
use codex_protocol::protocol::ThreadMemoryMode;
use codex_rollout::RolloutItem;
use codex_rollout::RolloutRecorder;
use codex_state::StateRuntime;
use pretty_assertions::assert_eq;
use serde_json::json;
use std::fs;
use std::sync::Arc;
use tempfile::TempDir;
use uuid::Uuid;

use super::LocalThreadStore;
use super::test_support::test_config;

fn create_params(thread_id: ThreadId) -> CreateThreadParams {
    CreateThreadParams {
        session_id: thread_id.into(),
        thread_id,
        extra_config: None,
        forked_from_id: None,
        parent_thread_id: None,
        source: SessionSource::Exec,
        thread_source: None,
        originator: "test".to_string(),
        base_instructions: BaseInstructions::default(),
        dynamic_tools: Vec::new(),
        selected_capability_roots: Vec::new(),
        multi_agent_version: None,
        history_mode: ThreadHistoryMode::Legacy,
        history_base: None,
        subagent_history_start_ordinal: None,
        initial_window_id: Uuid::now_v7().to_string(),
        metadata: ThreadPersistenceMetadata {
            cwd: Some(std::env::current_dir().expect("cwd")),
            model_provider: "test-provider".to_string(),
            memory_mode: ThreadMemoryMode::Enabled,
        },
    }
}

fn receipt(thread_id: ThreadId, status: ReceiptStatus) -> ReceiptAttachedItem {
    let mut receipt = ReceiptAttachedItem::new(
        "receipt-1",
        1,
        "test.receipt",
        "test receipt",
        status,
        "2026-08-31T12:00:00.000Z",
        "test-hook",
    )
    .expect("valid receipt");
    receipt.thread_id = Some(thread_id.to_string());
    receipt.turn_id = Some("turn-1".to_string());
    receipt.metadata = Some(json!({"result": "ok"}));
    receipt
}

fn receipt_count(items: &[RolloutItem]) -> usize {
    items
        .iter()
        .filter(|item| {
            matches!(
                item,
                RolloutItem::EventMsg(EventMsg::ItemCompleted(event))
                    if matches!(
                        &event.item,
                        TurnItem::Extension(ExtensionItem::ReceiptAttached(receipt))
                            if receipt.receipt_id == "receipt-1"
                    )
            )
        })
        .count()
}

#[tokio::test]
async fn concurrent_same_receipt_id_appends_once_and_divergent_content_conflicts() {
    let home = TempDir::new().expect("temp home");
    let store = Arc::new(LocalThreadStore::new(
        test_config(home.path()),
        /*state_db*/ None,
    ));
    let thread_id = ThreadId::from_u128(1);
    store
        .create_thread(create_params(thread_id))
        .await
        .expect("create thread");
    store
        .persist_thread(thread_id, PersistContext::Standard)
        .await
        .expect("persist thread");

    let first = AppendReceiptParams {
        thread_id,
        receipt: receipt(thread_id, ReceiptStatus::Pass),
        completed_at_ms: 1_000,
        resume: None,
    };
    let mut second_receipt = receipt(thread_id, ReceiptStatus::Pass);
    second_receipt.created_at = "2026-08-31T12:00:01.000Z".to_string();
    let second = AppendReceiptParams {
        receipt: second_receipt,
        ..first.clone()
    };
    let (left, right) = tokio::join!(store.append_receipt(first), store.append_receipt(second));
    assert!(matches!(left, Ok(AppendReceiptOutcome::Created(_))));
    assert!(matches!(right, Ok(AppendReceiptOutcome::Existing(_))));

    let path = store
        .live_rollout_path(thread_id)
        .await
        .expect("rollout path");
    let (items, _, _) = RolloutRecorder::load_rollout_items(&path)
        .await
        .expect("load rollout");
    assert_eq!(receipt_count(&items), 1);

    let conflict = store
        .append_receipt(AppendReceiptParams {
            thread_id,
            receipt: receipt(thread_id, ReceiptStatus::Fail),
            completed_at_ms: 2_000,
            resume: None,
        })
        .await
        .expect_err("divergent receipt must conflict");
    assert!(matches!(conflict, ThreadStoreError::Conflict { .. }));
    store
        .shutdown_thread(thread_id)
        .await
        .expect("shutdown thread");
}

#[tokio::test]
async fn cold_compressed_rollout_is_scanned_before_idempotent_append() {
    let home = TempDir::new().expect("temp home");
    let config = test_config(home.path());
    let thread_id = ThreadId::from_u128(2);
    let store = LocalThreadStore::new(config.clone(), /*state_db*/ None);
    store
        .create_thread(create_params(thread_id))
        .await
        .expect("create thread");
    store
        .persist_thread(thread_id, PersistContext::Standard)
        .await
        .expect("persist thread");
    let created = store
        .append_receipt(AppendReceiptParams {
            thread_id,
            receipt: receipt(thread_id, ReceiptStatus::Pass),
            completed_at_ms: 1_000,
            resume: None,
        })
        .await
        .expect("append receipt");
    assert!(matches!(created, AppendReceiptOutcome::Created(_)));
    let plain_path = store
        .live_rollout_path(thread_id)
        .await
        .expect("rollout path");
    store
        .shutdown_thread(thread_id)
        .await
        .expect("shutdown initial writer");

    let compressed_path = plain_path.with_file_name(format!(
        "{}.zst",
        plain_path
            .file_name()
            .and_then(|name| name.to_str())
            .expect("rollout filename")
    ));
    let compressed = zstd::stream::encode_all(
        fs::read(&plain_path)
            .expect("read plain rollout")
            .as_slice(),
        3,
    )
    .expect("compress rollout");
    fs::write(&compressed_path, compressed).expect("write compressed rollout");
    fs::remove_file(&plain_path).expect("remove plain rollout");

    let resumed_store = LocalThreadStore::new(config, /*state_db*/ None);
    let existing = resumed_store
        .append_receipt(AppendReceiptParams {
            thread_id,
            receipt: receipt(thread_id, ReceiptStatus::Pass),
            completed_at_ms: 2_000,
            resume: Some(ResumeThreadParams {
                thread_id,
                rollout_path: Some(compressed_path),
                history: None,
                include_archived: true,
                metadata: ThreadPersistenceMetadata {
                    cwd: Some(std::env::current_dir().expect("cwd")),
                    model_provider: "test-provider".to_string(),
                    memory_mode: ThreadMemoryMode::Enabled,
                },
            }),
        })
        .await
        .expect("idempotent compressed append");
    assert!(matches!(existing, AppendReceiptOutcome::Existing(_)));
    let (items, _, _) = RolloutRecorder::load_rollout_items(&plain_path)
        .await
        .expect("load materialized rollout");
    assert_eq!(receipt_count(&items), 1);
}

#[tokio::test]
async fn missing_workflow_projection_does_not_trigger_a_second_rollout_append() {
    let home = TempDir::new().expect("temp home");
    let config = test_config(home.path());
    let runtime = StateRuntime::init(
        config.sqlite.clone(),
        config.default_model_provider_id.clone(),
    )
    .await
    .expect("state runtime");
    let store = LocalThreadStore::new(config, Some(runtime.clone()));
    let thread_id = ThreadId::from_u128(3);
    store
        .create_thread(create_params(thread_id))
        .await
        .expect("create thread");
    store
        .persist_thread(thread_id, PersistContext::Standard)
        .await
        .expect("persist thread");
    let first = AppendReceiptParams {
        thread_id,
        receipt: receipt(thread_id, ReceiptStatus::Pass),
        completed_at_ms: 1_000,
        resume: None,
    };
    store
        .append_receipt(first.clone())
        .await
        .expect("append receipt");
    sqlx::query("DELETE FROM workflow_receipts")
        .execute(runtime.workflow().pool())
        .await
        .expect("remove derived projection");
    assert!(
        runtime
            .workflow()
            .get_receipt("receipt-1")
            .await
            .unwrap()
            .is_none()
    );

    let retry = store.append_receipt(first).await.expect("retry receipt");
    assert!(matches!(retry, AppendReceiptOutcome::Existing(_)));
    let path = store
        .live_rollout_path(thread_id)
        .await
        .expect("rollout path");
    let (items, _, _) = RolloutRecorder::load_rollout_items(&path)
        .await
        .expect("load rollout");
    assert_eq!(receipt_count(&items), 1);
    store
        .shutdown_thread(thread_id)
        .await
        .expect("shutdown thread");
    runtime.close().await;
}
