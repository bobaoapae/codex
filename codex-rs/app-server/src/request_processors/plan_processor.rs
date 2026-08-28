//! FORK extension: `plan/list` and `plan/read`.
//!
//! Plan mode writes each approved plan under `$CODEX_HOME/plans/` (see the `codex-plans` crate).
//! These read-only methods let a client browse and load them in a later session.

use std::sync::Arc;

use codex_app_server_protocol::ClientResponsePayload;
use codex_app_server_protocol::JSONRPCErrorError;
use codex_app_server_protocol::PlanListParams;
use codex_app_server_protocol::PlanListResponse;
use codex_app_server_protocol::PlanReadParams;
use codex_app_server_protocol::PlanReadResponse;
use codex_app_server_protocol::PlanSummary;
use codex_core::config::Config;
use codex_plans::SavedPlanSummary;

use crate::error_code::internal_error;
use crate::error_code::invalid_params;

const PLAN_LIST_DEFAULT_LIMIT: usize = 50;
const PLAN_LIST_MAX_LIMIT: usize = 200;

#[derive(Clone)]
pub(crate) struct PlanRequestProcessor {
    config: Arc<Config>,
}

impl PlanRequestProcessor {
    pub(crate) fn new(config: Arc<Config>) -> Self {
        Self { config }
    }

    pub(crate) async fn plan_list(
        &self,
        params: PlanListParams,
    ) -> Result<Option<ClientResponsePayload>, JSONRPCErrorError> {
        let plans = codex_plans::list_plans(&self.config.codex_home)
            .await
            .map_err(|error| internal_error(format!("failed to list plans: {error}")))?;

        let limit = params
            .limit
            .map(|limit| limit as usize)
            .unwrap_or(PLAN_LIST_DEFAULT_LIMIT)
            .clamp(1, PLAN_LIST_MAX_LIMIT);

        // The cursor is the id of the last entry the client already received.
        let mut remaining: Vec<SavedPlanSummary> = match params.cursor {
            Some(cursor) => plans
                .into_iter()
                .skip_while(|plan| plan.id != cursor)
                .skip(1)
                .collect(),
            None => plans,
        };

        let next_cursor = if remaining.len() > limit {
            remaining.truncate(limit);
            remaining.last().map(|plan| plan.id.clone())
        } else {
            None
        };

        Ok(Some(
            PlanListResponse {
                data: remaining.into_iter().map(api_plan).collect(),
                next_cursor,
            }
            .into(),
        ))
    }

    pub(crate) async fn plan_read(
        &self,
        params: PlanReadParams,
    ) -> Result<Option<ClientResponsePayload>, JSONRPCErrorError> {
        if !codex_plans::is_valid_plan_id(&params.id) {
            return Err(invalid_params(format!("invalid plan id: {}", params.id)));
        }
        let plan = codex_plans::read_plan(&self.config.codex_home, &params.id)
            .await
            .map_err(|error| internal_error(format!("failed to read plan: {error}")))?
            .ok_or_else(|| invalid_params(format!("plan not found: {}", params.id)))?;

        Ok(Some(
            PlanReadResponse {
                plan: api_plan(plan.summary),
                markdown: plan.markdown,
            }
            .into(),
        ))
    }
}

fn api_plan(plan: SavedPlanSummary) -> PlanSummary {
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
    }
}
