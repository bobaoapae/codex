//! Storage-neutral idempotent appends for host-owned receipt items.

use codex_extension_items::ExtensionItem;
use codex_extension_items::receipt::ReceiptAttachedItem;
use codex_protocol::ThreadId;
use codex_protocol::items::TurnItem;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::ItemCompletedEvent;
use codex_rollout::RolloutItem;

use crate::ResumeThreadParams;
use crate::ThreadStoreError;
use crate::ThreadStoreResult;

/// Parameters for one canonical, idempotent receipt append.
#[derive(Clone, Debug)]
pub struct AppendReceiptParams {
    /// Thread whose current rollout receives the receipt.
    pub thread_id: ThreadId,
    /// Complete host-owned receipt to compare or append.
    pub receipt: ReceiptAttachedItem,
    /// Completion timestamp persisted on the canonical lifecycle event.
    pub completed_at_ms: i64,
    /// Metadata needed to temporarily reopen a cold thread. Implementations
    /// must ignore this value when a live writer already exists.
    pub resume: Option<ResumeThreadParams>,
}

/// Result of an idempotent receipt append.
#[derive(Clone, Debug, PartialEq)]
pub enum AppendReceiptOutcome {
    /// The receipt was appended to the canonical rollout.
    Created(ReceiptAttachedItem),
    /// An equivalent receipt was already present in the canonical rollout.
    Existing(ReceiptAttachedItem),
}

/// Build the canonical persisted lifecycle item for a receipt.
pub(crate) fn canonical_receipt_item(
    thread_id: ThreadId,
    receipt: &ReceiptAttachedItem,
    completed_at_ms: i64,
) -> RolloutItem {
    RolloutItem::EventMsg(EventMsg::ItemCompleted(ItemCompletedEvent {
        thread_id,
        turn_id: receipt
            .turn_id
            .clone()
            .unwrap_or_else(|| receipt.receipt_id.clone()),
        item: TurnItem::Extension(ExtensionItem::ReceiptAttached(receipt.clone())),
        started_at_ms: None,
        completed_at_ms,
    }))
}

pub(crate) fn validate_append_receipt(params: &AppendReceiptParams) -> ThreadStoreResult<()> {
    params
        .receipt
        .validate()
        .map_err(|error| ThreadStoreError::InvalidRequest {
            message: format!("invalid receipt: {error}"),
        })?;
    if params.completed_at_ms < 0 {
        return Err(ThreadStoreError::InvalidRequest {
            message: "receipt completion timestamp must be non-negative".to_string(),
        });
    }
    if params
        .resume
        .as_ref()
        .is_some_and(|resume| resume.thread_id != params.thread_id)
    {
        return Err(ThreadStoreError::InvalidRequest {
            message: "receipt resume thread id does not match append thread".to_string(),
        });
    }
    Ok(())
}

/// Return the host-owned receipt carried by a canonical rollout item.
pub(crate) fn receipt_from_rollout_item(item: &RolloutItem) -> Option<&ReceiptAttachedItem> {
    let RolloutItem::EventMsg(EventMsg::ItemCompleted(event)) = item else {
        return None;
    };
    let TurnItem::Extension(ExtensionItem::ReceiptAttached(receipt)) = &event.item else {
        return None;
    };
    Some(receipt)
}

/// Compare the immutable receipt content used for idempotency.
///
/// Creation/update/finish timestamps are host-assigned and may differ when a
/// client retries a request whose timestamp was omitted. All other fields are
/// part of the receipt identity and a mismatch must be reported as a conflict.
pub(crate) fn receipts_equivalent(left: &ReceiptAttachedItem, right: &ReceiptAttachedItem) -> bool {
    left.receipt_id == right.receipt_id
        && left.schema_version == right.schema_version
        && left.kind == right.kind
        && left.subject == right.subject
        && left.status == right.status
        && left.thread_id == right.thread_id
        && left.turn_id == right.turn_id
        && left.job_id == right.job_id
        && left.plan_snapshot_id == right.plan_snapshot_id
        && left.source == right.source
        && left.provenance == right.provenance
        && left.tags == right.tags
        && left.refs == right.refs
        && left.metadata == right.metadata
}
