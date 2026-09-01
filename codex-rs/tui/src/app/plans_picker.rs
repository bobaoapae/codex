//! FORK: `/plans` — browse the plans Plan mode persisted under `$CODEX_HOME/plans/`.
//!
//! The plan list and the plan body both come from the app-server (`plan/list`, `plan/read`), so
//! the TUI never touches the plans directory itself.

use std::collections::HashSet;

use super::*;
use crate::app_event::SavedPlanAction;
use codex_app_server_protocol::PlanApproveParams;
use codex_app_server_protocol::PlanApproveResponse;
use codex_app_server_protocol::PlanLifecycle;
use codex_app_server_protocol::PlanListParams;
use codex_app_server_protocol::PlanListResponse;
use codex_app_server_protocol::PlanReadParams;
use codex_app_server_protocol::PlanReadResponse;
use codex_app_server_protocol::PlanSummary;
use codex_app_server_protocol::RequestId;

const PLANS_PICKER_PAGE_SIZE: u32 = 100;
const PLANS_PICKER_MAX_PLANS: usize = 500;
const PLANS_EMPTY_MESSAGE: &str = "No saved plans yet.";
const PLANS_EMPTY_HINT: &str = "Plans you approve in Plan mode are saved to ~/.codex/plans.";

impl App {
    pub(super) fn open_plans_picker(&mut self, app_server: &AppServerSession) {
        let request_id = self.chat_widget.begin_plans_picker_request();
        let request_handle = app_server.request_handle();
        let app_event_tx = self.app_event_tx.clone();
        tokio::spawn(async move {
            let result = async {
                let mut plans: Vec<PlanSummary> = Vec::new();
                let mut cursor: Option<String> = None;
                let mut seen_cursors = HashSet::new();
                loop {
                    if !seen_cursors.insert(cursor.clone()) {
                        return Err("plan/list returned a repeated pagination cursor".to_string());
                    }
                    let page = request_handle
                        .request_typed::<PlanListResponse>(ClientRequest::PlanList {
                            request_id: RequestId::String(format!("plan-list-{}", Uuid::new_v4())),
                            params: PlanListParams {
                                cursor: cursor.clone(),
                                limit: Some(PLANS_PICKER_PAGE_SIZE),
                            },
                        })
                        .await
                        .map_err(|err| err.to_string())?;
                    plans.extend(
                        page.data
                            .into_iter()
                            .take(PLANS_PICKER_MAX_PLANS - plans.len()),
                    );
                    match page.next_cursor {
                        Some(next) if plans.len() < PLANS_PICKER_MAX_PLANS => cursor = Some(next),
                        _ => break,
                    }
                }
                Ok(plans)
            }
            .await;

            app_event_tx.send(AppEvent::PlansPickerLoaded { request_id, result });
        });
    }

    pub(super) fn apply_plans_picker_result(
        &mut self,
        request_id: Uuid,
        result: Result<Vec<PlanSummary>, String>,
    ) {
        if !self.chat_widget.finish_plans_picker_request(request_id) {
            return;
        }
        match result {
            Ok(plans) if plans.is_empty() => {
                self.chat_widget.add_info_message(
                    PLANS_EMPTY_MESSAGE.to_string(),
                    Some(PLANS_EMPTY_HINT.to_string()),
                );
            }
            Ok(plans) => self.chat_widget.show_plans_picker(plans),
            Err(err) => self
                .chat_widget
                .add_error_message(format!("Failed to list saved plans: {err}")),
        }
    }

    pub(super) fn load_saved_plan(
        &mut self,
        app_server: &AppServerSession,
        id: String,
        revision: u32,
        lifecycle: PlanLifecycle,
        action: SavedPlanAction,
    ) {
        let request_id = self.chat_widget.begin_saved_plan_load();
        let request_handle = app_server.request_handle();
        let app_event_tx = self.app_event_tx.clone();
        tokio::spawn(async move {
            let result = request_handle
                .request_typed::<PlanReadResponse>(ClientRequest::PlanRead {
                    request_id: RequestId::String(format!("plan-read-{}", Uuid::new_v4())),
                    params: PlanReadParams {
                        id,
                        revision: match lifecycle {
                            PlanLifecycle::Draft => None,
                            PlanLifecycle::Approved | PlanLifecycle::Superseded => Some(revision),
                        },
                    },
                })
                .await
                .map_err(|err| err.to_string());
            app_event_tx.send(AppEvent::SavedPlanLoaded {
                request_id,
                expected_revision: revision,
                action,
                result,
            });
        });
    }

    pub(super) fn apply_saved_plan_loaded(
        &mut self,
        request_id: Uuid,
        expected_revision: u32,
        action: SavedPlanAction,
        result: Result<PlanReadResponse, String>,
    ) {
        if !self.chat_widget.finish_saved_plan_load(request_id) {
            return;
        }
        match result {
            Ok(plan) => self
                .chat_widget
                .apply_loaded_plan(plan, expected_revision, action),
            Err(err) => self
                .chat_widget
                .add_error_message(format!("Failed to load saved plan: {err}")),
        }
    }

    pub(super) fn approve_saved_plan(
        &mut self,
        app_server: &AppServerSession,
        id: String,
        expected_revision: u32,
        action: SavedPlanAction,
    ) {
        let request_id = self.chat_widget.begin_saved_plan_approval();
        let request_handle = app_server.request_handle();
        let app_event_tx = self.app_event_tx.clone();
        tokio::spawn(async move {
            let result = request_handle
                .request_typed::<PlanApproveResponse>(ClientRequest::PlanApprove {
                    request_id: RequestId::String(format!("plan-approve-{}", Uuid::new_v4())),
                    params: PlanApproveParams {
                        id,
                        expected_revision,
                    },
                })
                .await
                .map_err(|err| err.to_string());
            app_event_tx.send(AppEvent::PlanApprovalLoaded {
                request_id,
                action,
                result,
            });
        });
    }

    pub(super) fn apply_plan_approval_result(
        &mut self,
        request_id: Uuid,
        action: SavedPlanAction,
        result: Result<PlanApproveResponse, String>,
    ) {
        if !self.chat_widget.finish_saved_plan_approval(request_id) {
            return;
        }
        match result {
            Ok(response) if response.plan.lifecycle == PlanLifecycle::Approved => self
                .chat_widget
                .apply_approved_plan(response.plan, response.approved_plan, action),
            Ok(_) => self.chat_widget.show_saved_plan_stale(),
            Err(error) => self.chat_widget.show_saved_plan_approval_conflict(error),
        }
    }
}
