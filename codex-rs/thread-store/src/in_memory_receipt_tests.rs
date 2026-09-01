use crate::AppendReceiptOutcome;
use crate::AppendReceiptParams;
use crate::CreateThreadParams;
use crate::LoadThreadHistoryParams;
use crate::ThreadPersistenceMetadata;
use codex_extension_items::receipt::ReceiptAttachedItem;
use codex_extension_items::receipt::ReceiptStatus;
use codex_protocol::ThreadId;
use codex_protocol::models::BaseInstructions;
use codex_protocol::protocol::SessionSource;
use codex_protocol::protocol::ThreadHistoryMode;
use codex_protocol::protocol::ThreadMemoryMode;
use codex_rollout::RolloutItem;
use pretty_assertions::assert_eq;
use serde_json::json;
use std::sync::Arc;
use uuid::Uuid;

use super::InMemoryThreadStore;

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

#[tokio::test]
async fn same_receipt_id_is_atomic_in_memory() {
    let store = Arc::new(InMemoryThreadStore::default());
    let thread_id = ThreadId::from_u128(1);
    store
        .create_thread(create_params(thread_id))
        .await
        .expect("create thread");
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
    let history = store
        .load_history(LoadThreadHistoryParams {
            thread_id,
            include_archived: true,
        })
        .await
        .expect("load history");
    let count = history
        .items
        .iter()
        .filter(|item| matches!(item, RolloutItem::EventMsg(_)))
        .count();
    assert_eq!(count, 1);
}
