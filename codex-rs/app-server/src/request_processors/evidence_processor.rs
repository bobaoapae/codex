//! Experimental host-side evidence request handling.
//!
//! The workflow database owns only a rebuildable metadata projection. The
//! canonical `receipt.attached` item is appended to the thread rollout first;
//! the projection is then reconciled best-effort before the RPC acknowledges
//! the request. No method in this processor is registered as a model tool.

#[path = "evidence_processor_support.rs"]
mod support;

use codex_app_server_protocol::ClientResponsePayload;
use codex_app_server_protocol::EvidenceAttachParams;
use codex_app_server_protocol::EvidenceAttachResponse;
use codex_app_server_protocol::EvidenceExportParams;
use codex_app_server_protocol::EvidenceExportResponse;
use codex_app_server_protocol::EvidenceListParams;
use codex_app_server_protocol::EvidenceListResponse;
use codex_app_server_protocol::JSONRPCErrorError;
use codex_core::ThreadManager;
use codex_protocol::ThreadId;
use codex_protocol::protocol::ThreadMemoryMode;
use codex_rollout::StateDbHandle;
use codex_state::WorkflowReceiptCursor;
use codex_state::WorkflowReceiptExportSelection;
use codex_state::WorkflowReceiptFilter;
use codex_state::WorkflowReceiptListRequest;
use codex_state::WorkflowStore;
use codex_thread_store::AppendReceiptOutcome;
use codex_thread_store::AppendReceiptParams;
use codex_thread_store::ReadThreadParams;
use codex_thread_store::ResumeThreadParams;
use codex_thread_store::ThreadPersistenceMetadata;
use codex_thread_store::ThreadStore;
use std::sync::Arc;

use crate::error_code::internal_error;
use crate::error_code::invalid_params;
use crate::error_code::invalid_request;

use self::support::api_evidence;
use self::support::api_evidence_with_redaction;
use self::support::evidence_status_name;
use self::support::receipt_created_at_ms;
use self::support::receipt_error;
use self::support::receipt_item;
use self::support::resolve_run_id;
use self::support::thread_store_error;
use self::support::validate_client_json;
use self::support::workflow_receipt_input;

const EVIDENCE_LIST_DEFAULT_LIMIT: u32 = 50;
const EVIDENCE_LIST_MAX_LIMIT: u32 = 200;

/// Handles the host-only evidence/list, evidence/attach, and evidence/export
/// methods. The processor is intentionally absent from model tool specs.
#[derive(Clone)]
pub(crate) struct EvidenceRequestProcessor {
    workflow: Option<WorkflowStore>,
    thread_manager: Arc<ThreadManager>,
    thread_store: Arc<dyn ThreadStore>,
}

impl EvidenceRequestProcessor {
    pub(crate) fn new(
        state_db: Option<StateDbHandle>,
        thread_manager: Arc<ThreadManager>,
        thread_store: Arc<dyn ThreadStore>,
    ) -> Self {
        Self {
            workflow: state_db.map(|state_db| state_db.workflow_store().clone()),
            thread_manager,
            thread_store,
        }
    }

    fn workflow_store(&self) -> Result<&WorkflowStore, JSONRPCErrorError> {
        self.workflow
            .as_ref()
            .ok_or_else(|| invalid_request("evidence APIs require sqlite state"))
    }

    pub(crate) async fn evidence_list(
        &self,
        params: EvidenceListParams,
    ) -> Result<Option<ClientResponsePayload>, JSONRPCErrorError> {
        let workflow = self.workflow_store()?;
        let filter = WorkflowReceiptFilter {
            thread_id: params.thread_id,
            job_id: params.job_id,
            plan_snapshot_id: params.plan_snapshot_id,
            status: params.status.map(evidence_status_name).map(str::to_string),
            kind: params.kind,
        };
        let cursor = params
            .cursor
            .as_deref()
            .map(WorkflowReceiptCursor::decode)
            .transpose()
            .map_err(|error| {
                invalid_request(format!("invalid evidence pagination cursor: {error}"))
            })?;
        let limit = params
            .limit
            .unwrap_or(EVIDENCE_LIST_DEFAULT_LIMIT)
            .clamp(1, EVIDENCE_LIST_MAX_LIMIT);
        let request = WorkflowReceiptListRequest::new(filter, cursor, limit)
            .map_err(|error| invalid_request(format!("invalid evidence list request: {error}")))?;
        let page = workflow.list_receipts(&request).await.map_err(|error| {
            internal_error(format!("failed to list evidence receipts: {error}"))
        })?;
        let next_cursor = page
            .next_cursor
            .map(|cursor| cursor.encode())
            .transpose()
            .map_err(|error| {
                internal_error(format!("failed to encode evidence cursor: {error}"))
            })?;
        Ok(Some(
            EvidenceListResponse {
                data: page.receipts.into_iter().map(api_evidence).collect(),
                next_cursor,
            }
            .into(),
        ))
    }

    pub(crate) async fn evidence_attach(
        &self,
        params: EvidenceAttachParams,
    ) -> Result<Option<ClientResponsePayload>, JSONRPCErrorError> {
        let workflow = self.workflow_store()?.clone();
        let thread_id = ThreadId::from_string(&params.thread_id)
            .map_err(|error| invalid_params(format!("invalid thread id: {error}")))?;
        let created_at_ms = params
            .created_at
            .map(|seconds| {
                if seconds < 0 {
                    Err(invalid_params("createdAt must be non-negative"))
                } else {
                    Ok(seconds.saturating_mul(1_000))
                }
            })
            .transpose()?
            .unwrap_or_else(|| chrono::Utc::now().timestamp_millis());
        let tags = params.tags.unwrap_or_default();
        let refs = params.refs.unwrap_or_default();
        validate_client_json(params.provenance.as_ref(), "provenance")?;
        validate_client_json(params.metadata.as_ref(), "metadata")?;

        let receipt = receipt_item(
            params.receipt_id,
            params.schema_version,
            params.kind,
            params.subject,
            params.status,
            thread_id,
            params.turn_id,
            params.job_id,
            params.plan_snapshot_id,
            created_at_ms,
            params.source,
            params.provenance,
            tags,
            refs,
            params.metadata,
        )?;
        let resume = if let Ok(thread) = self.thread_manager.get_thread(thread_id).await {
            if thread.config_snapshot().await.ephemeral {
                return Err(invalid_request("ephemeral threads do not support evidence"));
            }
            None
        } else {
            let stored = self
                .thread_store
                .read_thread(ReadThreadParams {
                    thread_id,
                    include_archived: true,
                    include_history: false,
                })
                .await
                .map_err(|error| thread_store_error(thread_id, error))?;
            Some(ResumeThreadParams {
                thread_id,
                rollout_path: stored.rollout_path,
                history: None,
                include_archived: true,
                metadata: ThreadPersistenceMetadata {
                    cwd: Some(stored.cwd),
                    model_provider: stored.model_provider,
                    memory_mode: ThreadMemoryMode::Disabled,
                },
            })
        };
        let outcome = self
            .thread_store
            .append_receipt(AppendReceiptParams {
                thread_id,
                receipt,
                completed_at_ms: created_at_ms,
                resume,
            })
            .await
            .map_err(|error| thread_store_error(thread_id, error))?;
        let canonical_receipt = match outcome {
            AppendReceiptOutcome::Created(receipt) | AppendReceiptOutcome::Existing(receipt) => {
                receipt
            }
        };
        let run_id = resolve_run_id(&workflow, thread_id, canonical_receipt.job_id.as_deref())
            .await
            .map_err(|error| internal_error(format!("failed to resolve evidence run: {error}")))?;
        let input_created_at_ms =
            receipt_created_at_ms(&canonical_receipt).unwrap_or(created_at_ms);
        let input = workflow_receipt_input(&canonical_receipt, run_id, input_created_at_ms)?;
        let stored = workflow
            .insert_receipt(&input)
            .await
            .map_err(receipt_error)?;
        Ok(Some(
            EvidenceAttachResponse {
                evidence: api_evidence(stored),
            }
            .into(),
        ))
    }

    pub(crate) async fn evidence_export(
        &self,
        params: EvidenceExportParams,
    ) -> Result<Option<ClientResponsePayload>, JSONRPCErrorError> {
        let workflow = self.workflow_store()?;
        if params.receipt_ids.is_empty() {
            return Err(invalid_params(
                "evidence export requires an explicit non-empty receiptIds selection",
            ));
        }
        let receipts = workflow
            .select_receipts_for_export(&WorkflowReceiptExportSelection {
                receipt_ids: params.receipt_ids,
            })
            .await
            .map_err(|error| {
                invalid_params(format!("invalid evidence export selection: {error}"))
            })?;
        let mut redacted_count: u32 = 0;
        let data = receipts
            .into_iter()
            .map(|receipt| {
                let (evidence, count) = api_evidence_with_redaction(receipt);
                redacted_count = redacted_count.saturating_add(count);
                evidence
            })
            .collect();
        Ok(Some(
            EvidenceExportResponse {
                data,
                redacted: redacted_count > 0,
                redacted_count,
            }
            .into(),
        ))
    }
}
