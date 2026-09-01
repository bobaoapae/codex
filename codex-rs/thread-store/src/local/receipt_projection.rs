//! Projection of canonical `receipt.attached` items into workflow state.
//!
//! Rollout JSONL remains authoritative.  This module only consumes the
//! already-decoded records supplied by the existing rollout scanner and writes
//! bounded metadata to `workflow_receipts`.  Projection failures are returned
//! to the caller so generation backfills can remain unpublished, while live
//! callers can record the failure and continue the canonical write.

use codex_extension_items::ExtensionItem;
use codex_extension_items::receipt::ReceiptAttachedItem;
use codex_extension_items::receipt::ReceiptStatus;
use codex_protocol::ThreadId;
use codex_protocol::items::TurnItem;
use codex_protocol::protocol::EventMsg;
use codex_rollout::RolloutItem;
use codex_state::WorkflowReceiptCreate;
use codex_state::WorkflowReceiptReference;
use codex_state::WorkflowReceiptTag;
use codex_state::WorkflowStore;
use std::collections::HashMap;

use super::search_index_projection::parse_timestamp_ms;

/// A receipt item found while scanning one physical rollout.
#[derive(Debug, Clone)]
pub(super) struct ReceiptProjectionCandidate {
    pub(super) receipt: ReceiptAttachedItem,
    pub(super) rollout_thread_id: ThreadId,
    pub(super) turn_id: Option<String>,
    pub(super) event_time_ms: Option<i64>,
}

/// Redacted projection failure classification.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum ReceiptProjectionError {
    /// The same receipt ID was already persisted with different metadata.
    Conflict { receipt_id: String },
    /// The workflow database could not accept or read projection state.
    Storage,
}

impl ReceiptProjectionError {
    pub(super) fn message(&self) -> String {
        match self {
            Self::Conflict { receipt_id } => {
                format!("receipt projection conflict for receipt id {receipt_id}")
            }
            Self::Storage => "receipt projection database operation failed".to_string(),
        }
    }
}

/// Extract a typed receipt from an item already decoded by the rollout scan.
///
/// The scanner invokes this for every physical record, including records that
/// are outside a child rollout's visible history base.  Therefore every
/// persisted canonical occurrence reaches the workflow projection exactly
/// once per scan pass without introducing a second file traversal.
pub(super) fn candidate_from_rollout_item(
    item: &RolloutItem,
    rollout_thread_id: ThreadId,
    event_time_ms: Option<i64>,
) -> Option<ReceiptProjectionCandidate> {
    let RolloutItem::EventMsg(EventMsg::ItemCompleted(event)) = item else {
        return None;
    };
    let TurnItem::Extension(ExtensionItem::ReceiptAttached(receipt)) = &event.item else {
        return None;
    };
    Some(ReceiptProjectionCandidate {
        receipt: receipt.clone(),
        rollout_thread_id,
        turn_id: Some(event.turn_id.clone()),
        event_time_ms,
    })
}

/// Project all receipts found by one scanner pass.
pub(super) async fn project_receipts(
    workflow: &WorkflowStore,
    candidates: &[ReceiptProjectionCandidate],
) -> Result<usize, ReceiptProjectionError> {
    let mut thread_run_ids = HashMap::<String, Option<String>>::new();
    let mut job_run_ids = HashMap::<String, Option<String>>::new();
    let mut projected = 0;
    for candidate in candidates {
        let thread_id = candidate
            .receipt
            .thread_id
            .clone()
            .unwrap_or_else(|| candidate.rollout_thread_id.to_string());
        let run_id = resolve_run_id(
            workflow,
            &thread_id,
            candidate.receipt.job_id.as_deref(),
            &mut thread_run_ids,
            &mut job_run_ids,
        )
        .await?;
        let input = receipt_create_input(candidate, thread_id, run_id)?;
        workflow.insert_receipt(&input).await.map_err(|error| {
            if error.to_string().contains("different content") {
                ReceiptProjectionError::Conflict {
                    receipt_id: input.receipt_id.clone(),
                }
            } else {
                ReceiptProjectionError::Storage
            }
        })?;
        projected += 1;
    }
    Ok(projected)
}

async fn resolve_run_id(
    workflow: &WorkflowStore,
    thread_id: &str,
    job_id: Option<&str>,
    thread_run_ids: &mut HashMap<String, Option<String>>,
    job_run_ids: &mut HashMap<String, Option<String>>,
) -> Result<Option<String>, ReceiptProjectionError> {
    if let Some(job_id) = job_id {
        let run_id = if let Some(run_id) = job_run_ids.get(job_id) {
            run_id.clone()
        } else {
            let run_id = workflow
                .get_run(job_id)
                .await
                .map_err(|_| ReceiptProjectionError::Storage)?
                .map(|run| run.run_id);
            job_run_ids.insert(job_id.to_string(), run_id.clone());
            run_id
        };
        if run_id.is_some() {
            return Ok(run_id);
        }
    }
    if let Some(run_id) = thread_run_ids.get(thread_id) {
        return Ok(run_id.clone());
    }
    let run_id = workflow
        .get_runs_by_thread_id(thread_id)
        .await
        .map_err(|_| ReceiptProjectionError::Storage)?
        .into_iter()
        .next()
        .map(|run| run.run_id);
    thread_run_ids.insert(thread_id.to_string(), run_id.clone());
    Ok(run_id)
}

fn receipt_create_input(
    candidate: &ReceiptProjectionCandidate,
    thread_id: String,
    run_id: Option<String>,
) -> Result<WorkflowReceiptCreate, ReceiptProjectionError> {
    let receipt = &candidate.receipt;
    let schema_version =
        i64::try_from(receipt.schema_version).map_err(|_| ReceiptProjectionError::Storage)?;
    Ok(WorkflowReceiptCreate {
        receipt_id: receipt.receipt_id.clone(),
        run_id,
        thread_id: Some(thread_id),
        turn_id: receipt
            .turn_id
            .clone()
            .or_else(|| candidate.turn_id.clone()),
        job_id: receipt.job_id.clone(),
        plan_snapshot_id: receipt.plan_snapshot_id.clone(),
        schema_version,
        kind: receipt.kind.clone(),
        subject: receipt.subject.clone(),
        status: receipt_status(receipt.status).to_string(),
        source: receipt.source.clone(),
        provenance: receipt.provenance.clone(),
        tags: receipt
            .tags
            .iter()
            .map(|(key, value)| WorkflowReceiptTag {
                key: key.clone(),
                value: value.clone(),
            })
            .collect(),
        payload: receipt.metadata.clone(),
        references: receipt
            .refs
            .iter()
            .map(|reference| WorkflowReceiptReference {
                kind: reference.kind.clone(),
                id: reference.id.clone(),
            })
            .collect(),
        created_at_ms: parse_timestamp_ms(&receipt.created_at).or(candidate.event_time_ms),
    })
}

fn receipt_status(status: ReceiptStatus) -> &'static str {
    match status {
        ReceiptStatus::Pass => "pass",
        ReceiptStatus::Fail => "fail",
        ReceiptStatus::Blocked => "blocked",
        ReceiptStatus::Inconclusive => "inconclusive",
        ReceiptStatus::Informational => "informational",
    }
}

#[cfg(test)]
#[path = "receipt_projection_tests.rs"]
mod tests;
