//! Bounded conversions and redaction for the experimental evidence processor.

use chrono::SecondsFormat;
use chrono::TimeZone;
use chrono::Utc;
use codex_app_server_protocol::Evidence;
use codex_app_server_protocol::EvidenceReference;
use codex_app_server_protocol::EvidenceStatus;
use codex_app_server_protocol::JSONRPCErrorError;
use codex_extension_items::receipt::ReceiptAttachedItem;
use codex_extension_items::receipt::ReceiptReference;
use codex_extension_items::receipt::ReceiptStatus;
use codex_protocol::ThreadId;
use codex_state::WorkflowReceipt;
use codex_state::WorkflowReceiptCreate;
use codex_state::WorkflowReceiptReference;
use codex_state::WorkflowReceiptTag;
use codex_state::WorkflowStore;
use serde_json::Map;
use serde_json::Value;
use std::collections::BTreeMap;

use crate::error_code::internal_error;
use crate::error_code::invalid_params;
use crate::error_code::invalid_request;

pub(super) fn receipt_item(
    receipt_id: String,
    schema_version: u64,
    kind: String,
    subject: String,
    status: EvidenceStatus,
    thread_id: ThreadId,
    turn_id: Option<String>,
    job_id: Option<String>,
    plan_snapshot_id: Option<String>,
    created_at_ms: i64,
    source: String,
    provenance: Option<Value>,
    tags: BTreeMap<String, String>,
    refs: Vec<EvidenceReference>,
    metadata: Option<Value>,
) -> Result<ReceiptAttachedItem, JSONRPCErrorError> {
    let created_at = Utc
        .timestamp_millis_opt(created_at_ms)
        .single()
        .map(|timestamp| timestamp.to_rfc3339_opts(SecondsFormat::Millis, true))
        .ok_or_else(|| invalid_params("createdAt is outside the supported timestamp range"))?;
    let mut receipt = ReceiptAttachedItem::new(
        receipt_id,
        schema_version,
        kind,
        subject,
        receipt_status(status),
        created_at,
        source,
    )
    .map_err(|error| invalid_params(format!("invalid evidence receipt: {error}")))?;
    receipt.thread_id = Some(thread_id.to_string());
    receipt.turn_id = turn_id.or_else(|| Some(receipt.receipt_id.clone()));
    receipt.job_id = job_id;
    receipt.plan_snapshot_id = plan_snapshot_id;
    receipt.provenance = provenance;
    receipt.tags = tags;
    receipt.refs = refs
        .into_iter()
        .map(|reference| ReceiptReference {
            kind: reference.kind,
            id: reference.id,
        })
        .collect();
    receipt.metadata = metadata;
    receipt
        .validate()
        .map_err(|error| invalid_params(format!("invalid evidence receipt: {error}")))?;
    Ok(receipt)
}

pub(super) fn workflow_receipt_input(
    receipt: &ReceiptAttachedItem,
    run_id: Option<String>,
    created_at_ms: i64,
) -> Result<WorkflowReceiptCreate, JSONRPCErrorError> {
    let schema_version = i64::try_from(receipt.schema_version)
        .map_err(|_| invalid_params("schemaVersion is too large"))?;
    if !(1..=i64::from(i32::MAX)).contains(&schema_version) {
        return Err(invalid_params(
            "schemaVersion must be between 1 and 2147483647",
        ));
    }
    Ok(WorkflowReceiptCreate {
        receipt_id: receipt.receipt_id.clone(),
        run_id,
        thread_id: receipt.thread_id.clone(),
        turn_id: receipt.turn_id.clone(),
        job_id: receipt.job_id.clone(),
        plan_snapshot_id: receipt.plan_snapshot_id.clone(),
        schema_version,
        kind: receipt.kind.clone(),
        subject: receipt.subject.clone(),
        status: receipt_status_name(receipt.status).to_string(),
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
        created_at_ms: Some(created_at_ms),
    })
}

pub(super) fn receipt_created_at_ms(receipt: &ReceiptAttachedItem) -> Option<i64> {
    chrono::DateTime::parse_from_rfc3339(&receipt.created_at)
        .ok()
        .map(|timestamp| timestamp.timestamp_millis())
}

pub(super) async fn resolve_run_id(
    workflow: &WorkflowStore,
    thread_id: ThreadId,
    job_id: Option<&str>,
) -> anyhow::Result<Option<String>> {
    if let Some(job_id) = job_id
        && let Some(run) = workflow.get_run(job_id).await?
    {
        return Ok(Some(run.run_id));
    }
    Ok(workflow
        .get_runs_by_thread_id(&thread_id.to_string())
        .await?
        .into_iter()
        .next()
        .map(|run| run.run_id))
}

pub(super) fn api_evidence(receipt: WorkflowReceipt) -> Evidence {
    api_evidence_with_redaction(receipt).0
}

pub(super) fn api_evidence_with_redaction(receipt: WorkflowReceipt) -> (Evidence, u32) {
    let (provenance, provenance_redacted) = redacted_json(receipt.provenance);
    let (metadata, metadata_redacted) = redacted_json(receipt.payload);
    let redacted_count = provenance_redacted.saturating_add(metadata_redacted);
    (
        Evidence {
            receipt_id: receipt.receipt_id,
            schema_version: u64::try_from(receipt.schema_version).unwrap_or_default(),
            kind: receipt.kind,
            subject: receipt.subject,
            status: evidence_status(receipt.status.as_str()),
            thread_id: receipt.thread_id,
            turn_id: receipt.turn_id,
            job_id: receipt.job_id,
            plan_snapshot_id: receipt.plan_snapshot_id,
            created_at: receipt.created_at_ms / 1_000,
            source: receipt.source,
            provenance,
            tags: receipt
                .tags
                .into_iter()
                .map(|tag| (tag.key, tag.value))
                .collect(),
            refs: receipt
                .references
                .into_iter()
                .map(|reference| EvidenceReference {
                    kind: reference.kind,
                    id: reference.id,
                })
                .collect(),
            metadata,
        },
        redacted_count,
    )
}

pub(super) fn evidence_status_name(status: EvidenceStatus) -> &'static str {
    match status {
        EvidenceStatus::Pass => "pass",
        EvidenceStatus::Fail => "fail",
        EvidenceStatus::Blocked => "blocked",
        EvidenceStatus::Inconclusive => "inconclusive",
        EvidenceStatus::Informational => "informational",
    }
}

fn receipt_status_name(status: ReceiptStatus) -> &'static str {
    match status {
        ReceiptStatus::Pass => "pass",
        ReceiptStatus::Fail => "fail",
        ReceiptStatus::Blocked => "blocked",
        ReceiptStatus::Inconclusive => "inconclusive",
        ReceiptStatus::Informational => "informational",
    }
}

pub(super) fn receipt_error(error: anyhow::Error) -> JSONRPCErrorError {
    if error.to_string().contains("different content") {
        invalid_request("receiptId already exists with different evidence content")
    } else {
        internal_error(format!("failed to persist evidence receipt: {error}"))
    }
}

pub(super) fn thread_store_error(
    thread_id: ThreadId,
    error: codex_thread_store::ThreadStoreError,
) -> JSONRPCErrorError {
    match error {
        codex_thread_store::ThreadStoreError::ThreadNotFound { .. } => {
            invalid_request(format!("thread not found: {thread_id}"))
        }
        codex_thread_store::ThreadStoreError::InvalidRequest { message } => {
            invalid_request(message)
        }
        codex_thread_store::ThreadStoreError::Conflict { message } => invalid_request(message),
        codex_thread_store::ThreadStoreError::Unsupported { operation } => {
            invalid_request(format!("thread store does not support {operation}"))
        }
        codex_thread_store::ThreadStoreError::Internal { message } => internal_error(message),
    }
}

pub(super) fn validate_client_json(
    value: Option<&Value>,
    field: &str,
) -> Result<(), JSONRPCErrorError> {
    let Some(value) = value else {
        return Ok(());
    };
    if let Some(key) = forbidden_json_key(value) {
        return Err(invalid_params(format!(
            "evidence {field} contains forbidden key `{key}`"
        )));
    }
    Ok(())
}

fn receipt_status(status: EvidenceStatus) -> ReceiptStatus {
    match status {
        EvidenceStatus::Pass => ReceiptStatus::Pass,
        EvidenceStatus::Fail => ReceiptStatus::Fail,
        EvidenceStatus::Blocked => ReceiptStatus::Blocked,
        EvidenceStatus::Inconclusive => ReceiptStatus::Inconclusive,
        EvidenceStatus::Informational => ReceiptStatus::Informational,
    }
}

fn evidence_status(status: &str) -> EvidenceStatus {
    match status {
        "pass" => EvidenceStatus::Pass,
        "fail" => EvidenceStatus::Fail,
        "blocked" => EvidenceStatus::Blocked,
        "inconclusive" => EvidenceStatus::Inconclusive,
        "informational" => EvidenceStatus::Informational,
        _ => EvidenceStatus::Inconclusive,
    }
}

fn forbidden_json_key(value: &Value) -> Option<String> {
    match value {
        Value::Object(values) => values.iter().find_map(|(key, value)| {
            is_forbidden_json_key(key)
                .then(|| key.clone())
                .or_else(|| forbidden_json_key(value))
        }),
        Value::Array(values) => values.iter().find_map(forbidden_json_key),
        Value::Null | Value::Bool(_) | Value::Number(_) | Value::String(_) => None,
    }
}

fn redacted_json(value: Option<Value>) -> (Option<Value>, u32) {
    let Some(value) = value else {
        return (None, 0);
    };
    let mut redacted_count = 0;
    let value = redact_json(value, &mut redacted_count);
    (Some(value), redacted_count)
}

fn redact_json(value: Value, redacted_count: &mut u32) -> Value {
    match value {
        Value::Object(values) => Value::Object(
            values
                .into_iter()
                .filter_map(|(key, value)| {
                    if is_forbidden_json_key(&key) {
                        *redacted_count = redacted_count.saturating_add(1);
                        None
                    } else {
                        Some((key, redact_json(value, redacted_count)))
                    }
                })
                .collect::<Map<_, _>>(),
        ),
        Value::Array(values) => Value::Array(
            values
                .into_iter()
                .map(|value| redact_json(value, redacted_count))
                .collect(),
        ),
        value => value,
    }
}

fn is_forbidden_json_key(key: &str) -> bool {
    codex_extension_items::receipt::is_forbidden_metadata_key(key)
}

#[cfg(test)]
#[path = "evidence_processor_support_tests.rs"]
mod tests;
