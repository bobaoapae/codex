//! Idempotent receipt appends for local JSONL rollouts.

use codex_extension_items::receipt::ReceiptAttachedItem;
use serde_json::Value;
use std::path::Path;
use tokio::sync::OwnedMutexGuard;

use super::LocalThreadStore;
use super::live_writer;
use crate::AppendReceiptOutcome;
use crate::AppendReceiptParams;
use crate::ThreadStoreError;
use crate::ThreadStoreResult;
use crate::receipt_append::canonical_receipt_item;
use crate::receipt_append::receipt_from_rollout_item;
use crate::receipt_append::receipts_equivalent;
use crate::receipt_append::validate_append_receipt;

pub(super) async fn append_receipt(
    store: &LocalThreadStore,
    params: AppendReceiptParams,
) -> ThreadStoreResult<AppendReceiptOutcome> {
    validate_append_receipt(&params)?;
    let thread_id = params.thread_id;
    let thread_id_string = thread_id.to_string();
    if params.receipt.thread_id.as_deref() != Some(thread_id_string.as_str()) {
        return Err(ThreadStoreError::InvalidRequest {
            message: "receipt thread id does not match append thread".to_string(),
        });
    }
    let _lifecycle_guard = store.live_writer_locks.reserve_lifecycle(thread_id).await;
    let live_writer_guard = store.live_writer_locks.lock(thread_id).await;
    let was_cold = !store.live_recorders.lock().await.contains_key(&thread_id);
    if was_cold {
        let resume = params
            .resume
            .ok_or(ThreadStoreError::ThreadNotFound { thread_id })?;
        live_writer::resume_thread_locked(store, resume, &live_writer_guard).await?;
    }

    let result = append_receipt_locked(
        store,
        thread_id,
        &params.receipt,
        params.completed_at_ms,
        &live_writer_guard,
    )
    .await;
    drop(live_writer_guard);

    if was_cold {
        match result {
            Ok(outcome) => {
                live_writer::shutdown_thread(store, thread_id).await?;
                Ok(outcome)
            }
            Err(error) => {
                let _ = live_writer::discard_thread(store, thread_id).await;
                Err(error)
            }
        }
    } else {
        result
    }
}

async fn append_receipt_locked(
    store: &LocalThreadStore,
    thread_id: codex_protocol::ThreadId,
    receipt: &ReceiptAttachedItem,
    completed_at_ms: i64,
    live_writer_guard: &OwnedMutexGuard<()>,
) -> ThreadStoreResult<AppendReceiptOutcome> {
    let (recorder, _rollout_id, _history_mode) =
        live_writer::live_writer_parts(store, thread_id).await?;
    if let Some(existing) = find_receipt(recorder.rollout_path(), &receipt.receipt_id).await? {
        if receipts_equivalent(&existing, receipt) {
            return Ok(AppendReceiptOutcome::Existing(existing));
        }
        return Err(ThreadStoreError::Conflict {
            message: "receipt id already exists with different content".to_string(),
        });
    }

    live_writer::write_and_project_locked(
        store,
        thread_id,
        live_writer::RolloutWriteOp::AppendItems(vec![canonical_receipt_item(
            thread_id,
            receipt,
            completed_at_ms,
        )]),
        live_writer_guard,
    )
    .await?;
    Ok(AppendReceiptOutcome::Created(receipt.clone()))
}

async fn find_receipt(
    path: &Path,
    receipt_id: &str,
) -> ThreadStoreResult<Option<ReceiptAttachedItem>> {
    let mut lines = codex_rollout::open_rollout_line_reader(path)
        .await
        .map_err(scan_error)?;
    while let Some(line) = lines.next_line().await.map_err(scan_error)? {
        if line.trim().is_empty() {
            continue;
        }
        let value = serde_json::from_str::<Value>(&line).map_err(scan_error)?;
        let rollout_line = codex_rollout::decode_rollout_line(value).map_err(scan_error)?;
        if let Some(receipt) = receipt_from_rollout_item(&rollout_line.item)
            && receipt.receipt_id == receipt_id
        {
            return Ok(Some(receipt.clone()));
        }
    }
    Ok(None)
}

fn scan_error<E>(_error: E) -> ThreadStoreError {
    ThreadStoreError::Internal {
        message: "failed to scan rollout for receipt identity".to_string(),
    }
}
