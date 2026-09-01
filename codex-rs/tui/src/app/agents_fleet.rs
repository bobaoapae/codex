//! Durable fleet status and lifecycle requests for the `/agents` dashboard.
//! The app-server owns state and generation compare-and-swap.
use super::agents_fleet_view::AgentsFleetView;
use super::agents_fleet_view::AgentsFleetViewState;
use super::agents_fleet_view::FleetDashboardStatus;
use super::agents_fleet_view::fleet_actions_params;
use super::agents_fleet_view::fleet_close_confirmation_params;
use super::*;
use crate::app_event::AgentFleetOperationResponse;
use crate::app_event::AppEvent;
use codex_app_server_protocol::AgentFleetCloseParams;
use codex_app_server_protocol::AgentFleetCloseResponse;
use codex_app_server_protocol::AgentFleetResumeParams;
use codex_app_server_protocol::AgentFleetResumeResponse;
use codex_app_server_protocol::AgentFleetStatusParams;
use codex_app_server_protocol::AgentFleetStatusResponse;
use codex_app_server_protocol::AgentFleetSuspendParams;
use codex_app_server_protocol::AgentFleetSuspendResponse;
use codex_app_server_protocol::ClientRequest;
use codex_app_server_protocol::FleetMember;
use codex_app_server_protocol::FleetOperationKind;
use codex_app_server_protocol::RequestId;
use codex_protocol::ThreadId;
use std::collections::HashSet;
use std::sync::Arc;
use std::sync::Mutex;
use uuid::Uuid;
const FLEET_PAGE_SIZE: u32 = 50;
const FLEET_MAX_MEMBERS: usize = 200;
#[derive(Default)]
pub(super) struct AgentsFleetState {
    pub(super) root_thread_id: Option<ThreadId>,
    pub(super) generation: i64,
    pub(super) sealed: bool,
    pub(super) operation_id: Option<String>,
    pub(super) members: Vec<FleetMember>,
    pub(super) status: FleetDashboardStatus,
    pub(super) notice: Option<String>,
    pub(super) status_request_id: Option<Uuid>,
    pub(super) operation_request_id: Option<Uuid>,
    status_task: Option<tokio::task::AbortHandle>,
    operation_task: Option<tokio::task::AbortHandle>,
    pub(super) view_state: Arc<Mutex<AgentsFleetViewState>>,
}

impl Drop for AgentsFleetState {
    fn drop(&mut self) {
        if let Some(task) = self.status_task.take() {
            task.abort();
        }
        if let Some(task) = self.operation_task.take() {
            task.abort();
        }
    }
}
impl App {
    pub(super) fn open_agents_fleet_overview(
        &mut self,
        app_server: &AppServerSession,
        root_thread_id: ThreadId,
    ) {
        let state = &mut self.agents_overview.fleet;
        state.root_thread_id = Some(root_thread_id);
        state.generation = 0;
        state.sealed = false;
        state.operation_id = None;
        state.members.clear();
        state.status = FleetDashboardStatus::Loading;
        state.notice = None;
        state.status_request_id = None;
        state.operation_request_id = None;
        if let Some(task) = state.status_task.take() {
            task.abort();
        }
        if let Some(task) = state.operation_task.take() {
            task.abort();
        }
        if let Ok(mut view_state) = state.view_state.lock() {
            view_state.selected_member_id = None;
            view_state.input.clear();
            view_state.search.clear();
            view_state.searching = false;
            view_state.renaming = false;
        }
        self.show_agents_fleet_view();
        self.refresh_agents_fleet_status(app_server);
    }
    pub(super) fn show_agents_fleet_view(&mut self) {
        let Some(root_thread_id) = self.agents_overview.fleet.root_thread_id else {
            return;
        };
        let state = &self.agents_overview.fleet;
        let view = AgentsFleetView::new(
            root_thread_id,
            state.members.clone(),
            state.generation,
            state.sealed,
            state.operation_id.clone(),
            state.status,
            state.notice.clone(),
            self.app_event_tx.clone(),
            self.keymap.clone(),
            Arc::clone(&state.view_state),
            self.primary_thread_id.is_none(),
        );
        self.chat_widget.show_bottom_pane_view(Box::new(view));
    }
    pub(super) fn refresh_agents_fleet_status(&mut self, app_server: &AppServerSession) {
        let Some(root_thread_id) = self.agents_overview.fleet.root_thread_id else {
            return;
        };
        if self.agents_overview.fleet.status_request_id.is_some() {
            return;
        }
        if self.agents_overview.fleet.operation_request_id.is_some() {
            self.agents_overview.fleet.notice =
                Some("A fleet operation is still in progress; wait for its result.".to_string());
            self.show_agents_fleet_view();
            return;
        }
        let request_id = Uuid::new_v4();
        self.agents_overview.fleet.status_request_id = Some(request_id);
        self.agents_overview.fleet.status = FleetDashboardStatus::Loading;
        self.agents_overview.fleet.notice = Some("Loading fleet status…".to_string());
        self.show_agents_fleet_view();
        let request_handle = app_server.request_handle();
        let app_event_tx = self.app_event_tx.clone();
        let status_task = tokio::spawn(async move {
            let result = async {
                let mut cursor = None;
                let mut seen_cursors = HashSet::new();
                let mut merged: Option<AgentFleetStatusResponse> = None;
                loop {
                    if !seen_cursors.insert(cursor.clone()) {
                        return Err(
                            "agent/fleet/status returned a repeated pagination cursor".to_string()
                        );
                    }
                    let page = request_handle
                        .request_typed::<AgentFleetStatusResponse>(
                            ClientRequest::AgentFleetStatus {
                                request_id: RequestId::String(format!(
                                    "agent-fleet-status-{}",
                                    Uuid::new_v4()
                                )),
                                params: AgentFleetStatusParams {
                                    root_thread_id: root_thread_id.to_string(),
                                    cursor: cursor.clone(),
                                    limit: Some(FLEET_PAGE_SIZE.min(200)),
                                },
                            },
                        )
                        .await
                        .map_err(|error| error.to_string())?;
                    let has_capacity = merged
                        .as_ref()
                        .is_none_or(|response| response.data.len() < FLEET_MAX_MEMBERS);
                    if has_capacity {
                        let response = merged.get_or_insert_with(|| AgentFleetStatusResponse {
                            root_thread_id: page.root_thread_id.clone(),
                            generation: page.generation,
                            sealed: page.sealed,
                            operation_id: page.operation_id.clone(),
                            data: Vec::new(),
                            next_cursor: page.next_cursor.clone(),
                        });
                        response.data.extend(
                            page.data
                                .into_iter()
                                .take(FLEET_MAX_MEMBERS - response.data.len()),
                        );
                        response.next_cursor = page.next_cursor.clone();
                    }
                    let Some(next_cursor) = page.next_cursor else {
                        break;
                    };
                    if merged
                        .as_ref()
                        .is_some_and(|response| response.data.len() >= FLEET_MAX_MEMBERS)
                    {
                        break;
                    }
                    cursor = Some(next_cursor);
                }
                merged.ok_or_else(|| "agent/fleet/status returned no pages".to_string())
            }
            .await;
            app_event_tx.send(AppEvent::AgentsFleetStatusLoaded {
                request_id,
                root_thread_id,
                result,
            });
        });
        self.agents_overview.fleet.status_task = Some(status_task.abort_handle());
    }
    pub(super) fn apply_agents_fleet_status(
        &mut self,
        request_id: Uuid,
        root_thread_id: ThreadId,
        result: Result<AgentFleetStatusResponse, String>,
    ) {
        let state = &mut self.agents_overview.fleet;
        if state.status_request_id != Some(request_id)
            || state.root_thread_id != Some(root_thread_id)
        {
            return;
        }
        state.status_request_id = None;
        state.status_task = None;
        match result {
            Ok(response) if response.root_thread_id == root_thread_id.to_string() => {
                state.generation = response.generation;
                state.sealed = response.sealed;
                state.operation_id = response.operation_id;
                state.members = response.data;
                state.status = if state.members.is_empty() {
                    FleetDashboardStatus::Empty
                } else {
                    FleetDashboardStatus::Ready
                };
                state.notice = None;
            }
            Ok(_) => {
                state.status = FleetDashboardStatus::Error;
                state.notice = Some("Fleet status response targeted a different root.".to_string());
            }
            Err(error) => {
                state.status = if state.members.is_empty() {
                    FleetDashboardStatus::Error
                } else {
                    FleetDashboardStatus::Ready
                };
                state.notice = Some(format!("Fleet status unavailable: {error}"));
            }
        }
        if self
            .chat_widget
            .selected_index_for_present_view(AGENTS_OVERVIEW_VIEW_ID)
            .is_some()
        {
            self.show_agents_fleet_view();
        }
    }
    pub(super) fn open_agents_fleet_actions(
        &mut self,
        root_thread_id: ThreadId,
        expected_generation: i64,
    ) {
        if !self.fleet_generation_is_current(root_thread_id, expected_generation) {
            self.show_agents_fleet_stale();
            return;
        }
        let state = &self.agents_overview.fleet;
        self.chat_widget.show_selection_view(fleet_actions_params(
            root_thread_id,
            expected_generation,
            state.sealed,
            state.operation_id.as_deref(),
            &self.keymap,
        ));
    }
    pub(super) fn open_agents_fleet_close_confirmation(
        &mut self,
        root_thread_id: ThreadId,
        expected_generation: i64,
    ) {
        if !self.fleet_generation_is_current(root_thread_id, expected_generation) {
            self.show_agents_fleet_stale();
            return;
        }
        self.chat_widget
            .show_selection_view(fleet_close_confirmation_params(
                root_thread_id,
                expected_generation,
                &self.keymap,
            ));
    }
    pub(super) fn request_agents_fleet_suspend(
        &mut self,
        app_server: &AppServerSession,
        root_thread_id: ThreadId,
        expected_generation: i64,
    ) {
        self.request_agents_fleet_operation(
            app_server,
            root_thread_id,
            expected_generation,
            FleetOperationKind::Suspend,
        );
    }
    pub(super) fn request_agents_fleet_resume(
        &mut self,
        app_server: &AppServerSession,
        root_thread_id: ThreadId,
        expected_generation: i64,
    ) {
        self.request_agents_fleet_operation(
            app_server,
            root_thread_id,
            expected_generation,
            FleetOperationKind::Resume,
        );
    }
    pub(super) fn request_agents_fleet_close(
        &mut self,
        app_server: &AppServerSession,
        root_thread_id: ThreadId,
        expected_generation: i64,
    ) {
        self.request_agents_fleet_operation(
            app_server,
            root_thread_id,
            expected_generation,
            FleetOperationKind::Close,
        );
    }
    fn request_agents_fleet_operation(
        &mut self,
        app_server: &AppServerSession,
        root_thread_id: ThreadId,
        expected_generation: i64,
        kind: FleetOperationKind,
    ) {
        if !self.fleet_generation_is_current(root_thread_id, expected_generation) {
            self.show_agents_fleet_stale();
            return;
        }
        if self.agents_overview.fleet.operation_request_id.is_some() {
            self.agents_overview.fleet.notice =
                Some("Another fleet operation is already in progress.".to_string());
            self.show_agents_fleet_view();
            return;
        }
        let request_id = Uuid::new_v4();
        self.agents_overview.fleet.operation_request_id = Some(request_id);
        self.agents_overview.fleet.notice = Some(format!(
            "{} fleet at generation {expected_generation}…",
            fleet_operation_label(kind)
        ));
        self.show_agents_fleet_view();
        let request_handle = app_server.request_handle();
        let app_event_tx = self.app_event_tx.clone();
        let operation_task = tokio::spawn(async move {
            let result = match kind {
                FleetOperationKind::Suspend => request_handle
                    .request_typed::<AgentFleetSuspendResponse>(ClientRequest::AgentFleetSuspend {
                        request_id: RequestId::String(format!(
                            "agent-fleet-suspend-{}",
                            Uuid::new_v4()
                        )),
                        params: AgentFleetSuspendParams {
                            root_thread_id: root_thread_id.to_string(),
                            expected_generation,
                        },
                    })
                    .await
                    .map(AgentFleetOperationResponse::Suspend)
                    .map_err(|error| error.to_string()),
                FleetOperationKind::Resume => request_handle
                    .request_typed::<AgentFleetResumeResponse>(ClientRequest::AgentFleetResume {
                        request_id: RequestId::String(format!(
                            "agent-fleet-resume-{}",
                            Uuid::new_v4()
                        )),
                        params: AgentFleetResumeParams {
                            root_thread_id: root_thread_id.to_string(),
                            expected_generation,
                        },
                    })
                    .await
                    .map(AgentFleetOperationResponse::Resume)
                    .map_err(|error| error.to_string()),
                FleetOperationKind::Close => request_handle
                    .request_typed::<AgentFleetCloseResponse>(ClientRequest::AgentFleetClose {
                        request_id: RequestId::String(format!(
                            "agent-fleet-close-{}",
                            Uuid::new_v4()
                        )),
                        params: AgentFleetCloseParams {
                            root_thread_id: root_thread_id.to_string(),
                            expected_generation,
                        },
                    })
                    .await
                    .map(AgentFleetOperationResponse::Close)
                    .map_err(|error| error.to_string()),
            };
            app_event_tx.send(AppEvent::AgentsFleetOperationLoaded {
                request_id,
                root_thread_id,
                expected_generation,
                result,
            });
        });
        self.agents_overview.fleet.operation_task = Some(operation_task.abort_handle());
    }
    pub(super) fn apply_agents_fleet_operation(
        &mut self,
        request_id: Uuid,
        root_thread_id: ThreadId,
        expected_generation: i64,
        result: Result<AgentFleetOperationResponse, String>,
    ) {
        let state = &mut self.agents_overview.fleet;
        if state.operation_request_id != Some(request_id)
            || state.root_thread_id != Some(root_thread_id)
        {
            return;
        }
        let dashboard_visible = self
            .chat_widget
            .selected_index_for_present_view(AGENTS_OVERVIEW_VIEW_ID)
            .is_some();
        state.operation_request_id = None;
        state.operation_task = None;
        let response = match result {
            Ok(response) => response,
            Err(error) => {
                let qualifier = if error.to_lowercase().contains("recoverable")
                    || error.to_lowercase().contains("partial")
                {
                    "recoverable/partial"
                } else {
                    "failed"
                };
                state.notice = Some(format!(
                    "Fleet operation {qualifier} at generation {expected_generation}: {error}"
                ));
                if dashboard_visible {
                    self.show_agents_fleet_view();
                }
                return;
            }
        };
        let (kind, generation, sealed, operation_id, results) = match response {
            AgentFleetOperationResponse::Suspend(response) => (
                FleetOperationKind::Suspend,
                response.generation,
                response.sealed,
                response.operation_id,
                response.results,
            ),
            AgentFleetOperationResponse::Resume(response) => (
                FleetOperationKind::Resume,
                response.generation,
                response.sealed,
                response.operation_id,
                response.results,
            ),
            AgentFleetOperationResponse::Close(response) => (
                FleetOperationKind::Close,
                response.generation,
                response.sealed,
                response.operation_id,
                response.results,
            ),
        };
        if generation < state.generation || expected_generation != state.generation {
            return;
        }
        state.generation = generation;
        state.sealed = sealed;
        state.operation_id = operation_id.clone();
        let failed = results.iter().filter(|result| !result.success).count();
        for result in results.iter() {
            if let Some(member) = state
                .members
                .iter_mut()
                .find(|member| member.member_id == result.member_id)
                && let Some(final_state) = result.final_state
            {
                member.state = final_state;
            }
        }
        let operation = operation_id.as_deref().unwrap_or("unknown");
        state.notice = if failed == 0 {
            Some(format!(
                "Fleet {} completed at generation {generation} ({operation}).",
                fleet_operation_label(kind)
            ))
        } else {
            Some(format!(
                "Fleet {} completed partially: {failed} member(s) are recoverable ({operation}).",
                fleet_operation_label(kind)
            ))
        };
        state.status = if state.members.is_empty() {
            FleetDashboardStatus::Empty
        } else {
            FleetDashboardStatus::Ready
        };
        if dashboard_visible {
            self.show_agents_fleet_view();
        }
    }
    fn fleet_generation_is_current(
        &self,
        root_thread_id: ThreadId,
        expected_generation: i64,
    ) -> bool {
        self.agents_overview.fleet.root_thread_id == Some(root_thread_id)
            && self.agents_overview.fleet.generation == expected_generation
            && self.agents_overview.fleet.status == FleetDashboardStatus::Ready
    }
    fn show_agents_fleet_stale(&mut self) {
        let notice = if self.agents_overview.fleet.status == FleetDashboardStatus::Ready {
            "Fleet status changed; refresh before sending this action again."
        } else {
            "Fleet status is not ready; refresh before sending this action."
        };
        self.agents_overview.fleet.notice = Some(notice.to_string());
        self.show_agents_fleet_view();
    }
}
fn fleet_operation_label(kind: FleetOperationKind) -> &'static str {
    match kind {
        FleetOperationKind::Suspend => "suspend",
        FleetOperationKind::Resume => "resume",
        FleetOperationKind::Close => "close",
    }
}
