//! FORK extension: experimental APIs for draft and approved Plan-mode plans.
//!
//! The filesystem-backed `codex-plans` crate owns validation, locking, CAS, and
//! immutable snapshot writes. This module only merges its draft/snapshot views
//! into the app-server wire model and applies bounded keyset pagination.

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use codex_app_server_protocol::ApprovedPlanRef;
use codex_app_server_protocol::ClientResponsePayload;
use codex_app_server_protocol::JSONRPCErrorError;
use codex_app_server_protocol::PlanApproveParams;
use codex_app_server_protocol::PlanApproveResponse;
use codex_app_server_protocol::PlanLifecycle;
use codex_app_server_protocol::PlanListParams;
use codex_app_server_protocol::PlanListResponse;
use codex_app_server_protocol::PlanReadParams;
use codex_app_server_protocol::PlanReadResponse;
use codex_app_server_protocol::PlanSummary;
use codex_core::config::Config;
use codex_plans::ApprovedPlanSummary;
use codex_plans::PlanApprovalError;
use codex_plans::PlanOrigin;
use codex_plans::SavedPlanSummary;
use serde::Deserialize;
use serde::Serialize;
use std::cmp::Ordering;
use std::sync::Arc;

use crate::error_code::internal_error;
use crate::error_code::invalid_params;
use crate::error_code::invalid_request;

const PLAN_LIST_DEFAULT_LIMIT: usize = 50;
const PLAN_LIST_MAX_LIMIT: usize = 200;

#[derive(Clone)]
pub(crate) struct PlanRequestProcessor {
    config: Arc<Config>,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct PlanCursor {
    id: String,
    updated_at: i64,
    revision: u32,
    lifecycle: PlanLifecycle,
}

impl PlanRequestProcessor {
    pub(crate) fn new(config: Arc<Config>) -> Self {
        Self { config }
    }

    pub(crate) async fn plan_list(
        &self,
        params: PlanListParams,
    ) -> Result<Option<ClientResponsePayload>, JSONRPCErrorError> {
        let mut plans = codex_plans::list_plans(&self.config.codex_home)
            .await
            .map_err(|error| internal_error(format!("failed to list plans: {error}")))?
            .into_iter()
            .map(api_draft_plan)
            .collect::<Vec<_>>();
        let approved = codex_plans::list_approved_plans(&self.config.codex_home)
            .await
            .map_err(|error| internal_error(format!("failed to list approved plans: {error}")))?;
        plans.extend(approved.into_iter().map(api_approved_plan));
        plans.sort_by(compare_plans);

        let start = params
            .cursor
            .as_deref()
            .map(|cursor| cursor_start(&plans, cursor))
            .transpose()?
            .map_or(0, |index| index + 1);
        let limit = params
            .limit
            .map(|limit| limit as usize)
            .unwrap_or(PLAN_LIST_DEFAULT_LIMIT)
            .clamp(1, PLAN_LIST_MAX_LIMIT);
        let has_more = start.saturating_add(limit) < plans.len();
        let data = plans
            .into_iter()
            .skip(start)
            .take(limit)
            .collect::<Vec<_>>();
        let next_cursor = has_more
            .then(|| data.last())
            .flatten()
            .map(encode_cursor)
            .transpose()?;

        Ok(Some(PlanListResponse { data, next_cursor }.into()))
    }

    pub(crate) async fn plan_read(
        &self,
        params: PlanReadParams,
    ) -> Result<Option<ClientResponsePayload>, JSONRPCErrorError> {
        if !codex_plans::is_valid_plan_id(&params.id) {
            return Err(invalid_params(format!("invalid plan id: {}", params.id)));
        }
        if let Some(revision) = params.revision {
            let plan = codex_plans::read_approved_plan(
                &self.config.codex_home,
                &params.id,
                Some(revision),
            )
            .await
            .map_err(plan_approval_error)?
            .ok_or_else(|| {
                invalid_params(format!(
                    "approved plan revision not found: {}@{revision}",
                    params.id
                ))
            })?;
            return Ok(Some(
                PlanReadResponse {
                    plan: api_approved_plan(plan.summary),
                    markdown: plan.markdown,
                }
                .into(),
            ));
        }

        let plan = codex_plans::read_plan(&self.config.codex_home, &params.id)
            .await
            .map_err(|error| internal_error(format!("failed to read plan: {error}")))?
            .ok_or_else(|| invalid_params(format!("plan not found: {}", params.id)))?;
        Ok(Some(
            PlanReadResponse {
                plan: api_draft_plan(plan.summary),
                markdown: plan.markdown,
            }
            .into(),
        ))
    }

    pub(crate) async fn plan_approve(
        &self,
        params: PlanApproveParams,
    ) -> Result<Option<ClientResponsePayload>, JSONRPCErrorError> {
        let approved = codex_plans::approve_plan(codex_plans::ApprovePlanRequest {
            codex_home: self.config.codex_home.clone(),
            id: params.id,
            expected_revision: params.expected_revision,
            origin: PlanOrigin::default(),
            approved_at: None,
        })
        .await
        .map_err(plan_approval_error)?;
        let summary = api_approved_plan(approved.summary);
        let approved_plan = ApprovedPlanRef {
            id: summary.id.clone(),
            revision: summary.revision,
        };
        Ok(Some(
            PlanApproveResponse {
                plan: summary,
                approved_plan,
            }
            .into(),
        ))
    }
}

fn compare_plans(left: &PlanSummary, right: &PlanSummary) -> Ordering {
    right
        .updated_at
        .cmp(&left.updated_at)
        .then_with(|| right.id.cmp(&left.id))
        .then_with(|| right.revision.cmp(&left.revision))
        .then_with(|| lifecycle_rank(right.lifecycle).cmp(&lifecycle_rank(left.lifecycle)))
}

fn lifecycle_rank(lifecycle: PlanLifecycle) -> u8 {
    match lifecycle {
        PlanLifecycle::Draft => 0,
        PlanLifecycle::Approved => 1,
        PlanLifecycle::Superseded => 2,
    }
}

fn cursor_start(plans: &[PlanSummary], raw_cursor: &str) -> Result<usize, JSONRPCErrorError> {
    if let Ok(bytes) = URL_SAFE_NO_PAD.decode(raw_cursor)
        && let Ok(cursor) = serde_json::from_slice::<PlanCursor>(&bytes)
    {
        return plans
            .iter()
            .position(|plan| {
                plan.id == cursor.id
                    && plan.updated_at == cursor.updated_at
                    && plan.revision == cursor.revision
                    && plan.lifecycle == cursor.lifecycle
            })
            .ok_or_else(|| invalid_request("plan cursor is stale or incompatible"));
    }

    // Accept the original id cursor for clients that only ever saw draft plans.
    plans
        .iter()
        .position(|plan| plan.id == raw_cursor)
        .ok_or_else(|| invalid_request("plan cursor is unknown or incompatible"))
}

fn encode_cursor(plan: &PlanSummary) -> Result<String, JSONRPCErrorError> {
    let cursor = PlanCursor {
        id: plan.id.clone(),
        updated_at: plan.updated_at,
        revision: plan.revision,
        lifecycle: plan.lifecycle,
    };
    let bytes = serde_json::to_vec(&cursor)
        .map_err(|error| internal_error(format!("failed to encode plan cursor: {error}")))?;
    Ok(URL_SAFE_NO_PAD.encode(bytes))
}

fn api_draft_plan(plan: SavedPlanSummary) -> PlanSummary {
    PlanSummary {
        id: plan.id,
        title: plan.title,
        path: plan.path.to_string_lossy().to_string(),
        thread_id: plan.thread_id,
        turn_id: plan.turn_id,
        cwd: plan.cwd,
        model: plan.model,
        created_at: plan.created_at.timestamp(),
        updated_at: plan.updated_at.timestamp(),
        revision: plan.revision,
        lifecycle: PlanLifecycle::Draft,
    }
}

fn api_approved_plan(plan: ApprovedPlanSummary) -> PlanSummary {
    PlanSummary {
        id: plan.id,
        title: plan.title,
        path: plan.path.to_string_lossy().to_string(),
        thread_id: plan.thread_id,
        turn_id: plan.turn_id,
        cwd: plan.cwd,
        model: plan.model,
        created_at: plan.created_at.timestamp(),
        updated_at: plan.updated_at.timestamp(),
        revision: plan.revision,
        lifecycle: plan
            .superseded_by
            .map_or(PlanLifecycle::Approved, |_| PlanLifecycle::Superseded),
    }
}

fn plan_approval_error(error: PlanApprovalError) -> JSONRPCErrorError {
    match error {
        PlanApprovalError::Io(error) => internal_error(format!("plan storage failed: {error}")),
        PlanApprovalError::InvalidId(id) => invalid_params(format!("invalid plan id: {id}")),
        PlanApprovalError::DraftNotFound(id) => {
            invalid_params(format!("draft plan not found: {id}"))
        }
        PlanApprovalError::StaleDraft {
            id,
            expected,
            actual,
        } => invalid_request(format!(
            "draft plan {id} is stale: expected revision {expected}, current revision {actual}"
        )),
        PlanApprovalError::Conflict(message) => invalid_request(message),
        PlanApprovalError::TooLarge { actual, maximum } => invalid_request(format!(
            "approved plan is too large: {actual} tokens (maximum {maximum})"
        )),
    }
}
