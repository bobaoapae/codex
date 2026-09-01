use codex_extension_items::ExtensionItem;
use codex_extension_items::receipt::ReceiptAttachedItem;
use codex_extension_items::receipt::ReceiptStatus;
use codex_protocol::ThreadId;
use codex_protocol::items::TurnItem;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::ItemCompletedEvent;
use codex_protocol::protocol::ThreadHistoryMode;

use crate::RolloutItem;
use crate::is_persisted_rollout_item;
use crate::persisted_rollout_items;

fn receipt_rollout_item() -> RolloutItem {
    let receipt = ReceiptAttachedItem::new(
        "receipt-1",
        1,
        "test.result",
        "receipt persistence",
        ReceiptStatus::Pass,
        "2026-08-31T12:00:00Z",
        "tester",
    )
    .expect("valid receipt");
    RolloutItem::EventMsg(EventMsg::ItemCompleted(ItemCompletedEvent {
        thread_id: ThreadId::new(),
        turn_id: "turn-1".to_string(),
        item: TurnItem::Extension(ExtensionItem::ReceiptAttached(receipt)),
        started_at_ms: None,
        completed_at_ms: 1,
    }))
}

#[test]
fn receipt_attached_is_persisted_in_legacy_and_paginated_history() {
    let item = receipt_rollout_item();
    for history_mode in [ThreadHistoryMode::Legacy, ThreadHistoryMode::Paginated] {
        assert!(is_persisted_rollout_item(&item, history_mode));
        let persisted = persisted_rollout_items(std::slice::from_ref(&item), history_mode);
        assert_eq!(persisted.len(), 1);
        assert_eq!(
            serde_json::to_value(&persisted[0]).expect("serialize persisted receipt"),
            serde_json::to_value(&item).expect("serialize receipt"),
        );
    }
}
