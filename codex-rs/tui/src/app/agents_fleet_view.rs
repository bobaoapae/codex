//! Render and interact with the durable agent-fleet dashboard.
//!
//! The view is deliberately a thin state machine. Fleet state is supplied by the app-server and
//! lifecycle mutations are emitted as app events, so this module never guesses a generation or
//! retries an operation locally.

use super::agents_fleet_actions::fleet_state_display;
use super::agents_fleet_actions::fleet_state_label;
use super::agents_fleet_actions::fleet_state_rank;
use crate::app_event::AppEvent;
use crate::app_event_sender::AppEventSender;
use crate::bottom_pane::SelectionAction;
use crate::bottom_pane::SelectionItem;
use crate::bottom_pane::SelectionViewParams;
use crate::bottom_pane::ViewCompletion;
use crate::bottom_pane::popup_consts::standard_popup_hint_line_for_keymap;
use crate::keymap::AgentsKeymap;
use crate::keymap::ListKeymap;
use crate::keymap::RuntimeKeymap;
use codex_app_server_protocol::FleetMember;
use codex_app_server_protocol::FleetMemberState;
use codex_protocol::ThreadId;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::Stylize;
use ratatui::text::Line;
use ratatui::widgets::Widget;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::MutexGuard;
use std::sync::PoisonError;

#[path = "agents_fleet_view_input.rs"]
mod input;

#[cfg(test)]
#[path = "agents_fleet_view_tests.rs"]
mod tests;

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(super) enum FleetDashboardStatus {
    #[default]
    Loading,
    Ready,
    Empty,
    Error,
}

#[derive(Clone, Default)]
pub(super) struct AgentsFleetViewState {
    pub(super) input: String,
    pub(super) search: String,
    pub(super) searching: bool,
    pub(super) renaming: bool,
    pub(super) status_grouping: bool,
    pub(super) selected_member_id: Option<String>,
}

pub(super) struct AgentsFleetView {
    root_thread_id: ThreadId,
    members: Vec<FleetMember>,
    generation: i64,
    sealed: bool,
    operation_id: Option<String>,
    status: FleetDashboardStatus,
    notice: Option<String>,
    selected: usize,
    state: Arc<Mutex<AgentsFleetViewState>>,
    exit_on_cancel: bool,
    completion: Option<ViewCompletion>,
    app_event_tx: AppEventSender,
    keymap: ListKeymap,
    agents_keymap: AgentsKeymap,
}

impl AgentsFleetView {
    #[allow(clippy::too_many_arguments)]
    pub(super) fn new(
        root_thread_id: ThreadId,
        members: Vec<FleetMember>,
        generation: i64,
        sealed: bool,
        operation_id: Option<String>,
        status: FleetDashboardStatus,
        notice: Option<String>,
        app_event_tx: AppEventSender,
        keymap: RuntimeKeymap,
        state: Arc<Mutex<AgentsFleetViewState>>,
        exit_on_cancel: bool,
    ) -> Self {
        let selected_member_id = state
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .selected_member_id
            .clone();
        let selected = selected_member_id
            .as_deref()
            .and_then(|member_id| {
                members
                    .iter()
                    .position(|member| member.member_id == member_id)
            })
            .unwrap_or_default();
        let mut view = Self {
            root_thread_id,
            members,
            generation,
            sealed,
            operation_id,
            status,
            notice,
            selected,
            state,
            exit_on_cancel,
            completion: None,
            app_event_tx,
            keymap: keymap.list,
            agents_keymap: keymap.agents,
        };
        if view.members.is_empty() {
            view.selected = 0;
        } else if view.selected >= view.members.len() {
            view.selected = view.members.len() - 1;
        }
        view.remember_selection();
        view
    }

    fn state(&self) -> MutexGuard<'_, AgentsFleetViewState> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn remember_selection(&self) {
        if let Some(member) = self.members.get(self.selected) {
            self.state().selected_member_id = Some(member.member_id.clone());
        }
    }

    fn visible_indices(&self) -> Vec<usize> {
        let state = self.state().clone();
        let search = state.search.to_lowercase();
        let mut visible = self
            .members
            .iter()
            .enumerate()
            .filter_map(|(index, member)| {
                let searchable = format!(
                    "{} {} {}",
                    member.member_id,
                    member.thread_id.as_deref().unwrap_or_default(),
                    fleet_state_label(member.state),
                )
                .to_lowercase();
                (search.is_empty() || searchable.contains(&search)).then_some(index)
            })
            .collect::<Vec<_>>();
        if state.status_grouping {
            visible.sort_by_key(|index| {
                (
                    fleet_state_rank(self.members[*index].state),
                    self.members[*index].depth,
                    self.members[*index].order_index,
                )
            });
        } else {
            visible.sort_by_key(|index| {
                (self.members[*index].depth, self.members[*index].order_index)
            });
        }
        visible
    }

    fn selected_member(&self) -> Option<&FleetMember> {
        self.members
            .get(self.selected)
            .filter(|_| self.visible_indices().contains(&self.selected))
    }

    fn selected_thread_id(&self) -> Option<ThreadId> {
        self.selected_member().and_then(|member| {
            member
                .thread_id
                .as_deref()
                .or(Some(member.member_id.as_str()))
                .and_then(|id| ThreadId::from_string(id).ok())
        })
    }

    fn move_selection(&mut self, forward: bool) {
        if self.state().renaming {
            return;
        }
        let visible = self.visible_indices();
        if visible.is_empty() {
            return;
        }
        let current = visible
            .iter()
            .position(|index| *index == self.selected)
            .unwrap_or_default();
        self.selected = if forward {
            visible[(current + 1) % visible.len()]
        } else {
            visible[current.checked_sub(1).unwrap_or(visible.len() - 1)]
        };
        self.remember_selection();
    }

    fn activate(&mut self) {
        let state = self.state().clone();
        let input = state.input.clone();
        if !state.searching && !input.is_empty() && input.trim().is_empty() {
            return;
        }
        if !state.searching && !input.trim().is_empty() {
            if state.renaming {
                if let Some(thread_id) = self.selected_thread_id() {
                    self.app_event_tx
                        .send(AppEvent::RenameAgentsOverviewThread {
                            thread_id,
                            name: input.trim().to_string(),
                        });
                }
            } else {
                self.app_event_tx
                    .send(AppEvent::DispatchAgentsOverviewTask {
                        prompt: input,
                        cwd: None,
                    });
            }
            let mut state = self.state();
            state.input.clear();
            state.renaming = false;
        } else if let Some(thread_id) = self.selected_thread_id() {
            self.app_event_tx
                .send(AppEvent::SelectAgentsOverviewThread { thread_id });
            if state.searching {
                let mut state = self.state();
                state.search.clear();
                state.searching = false;
            }
            self.completion = Some(ViewCompletion::Accepted);
        }
    }

    fn edit_input(&mut self, edit: impl FnOnce(&mut String)) -> bool {
        let mut state = self.state();
        if state.searching {
            edit(&mut state.search);
        } else {
            edit(&mut state.input);
        }
        drop(state);
        if self.state().searching {
            self.selected = self.visible_indices().first().copied().unwrap_or_default();
            self.remember_selection();
        }
        true
    }

    fn open_actions(&self) {
        self.app_event_tx.send(AppEvent::OpenAgentsFleetActions {
            root_thread_id: self.root_thread_id,
            expected_generation: self.generation,
        });
    }

    fn stop_selected(&self) {
        let Some(member) = self.selected_member() else {
            return;
        };
        if matches!(
            member.state,
            FleetMemberState::Running
                | FleetMemberState::WaitingForTool
                | FleetMemberState::WaitingForApproval
                | FleetMemberState::WaitingForUser
        ) && let Some(thread_id) = self.selected_thread_id()
        {
            self.app_event_tx
                .send(AppEvent::StopAgentsOverviewThread { thread_id });
        }
    }

    fn begin_rename(&mut self) {
        if let Some(member_id) = self
            .selected_member()
            .map(|member| member.member_id.clone())
        {
            let mut state = self.state();
            if state.input.is_empty() {
                state.input = member_id;
                state.search.clear();
                state.searching = false;
                state.renaming = true;
            }
        }
    }

    fn render_rows(&self, area: Rect, buf: &mut Buffer) {
        let visible = self.visible_indices();
        if visible.is_empty() {
            let message = match self.status {
                FleetDashboardStatus::Loading => "Loading fleet status…",
                FleetDashboardStatus::Empty => "No fleet members found.",
                FleetDashboardStatus::Error => "Fleet status unavailable.",
                FleetDashboardStatus::Ready => "No members match the current filter.",
            };
            Line::from(message.dim()).render(area, buf);
            return;
        }
        for (offset, index) in visible.into_iter().enumerate() {
            if offset >= usize::from(area.height) {
                break;
            }
            let member = &self.members[index];
            let marker = if self.selected == index {
                "›".cyan().bold()
            } else {
                " ".into()
            };
            let (status, dot) = fleet_state_display(member.state);
            let indent = "  ".repeat(usize::try_from(member.depth.max(0)).unwrap_or_default());
            let thread_id = member
                .thread_id
                .as_deref()
                .unwrap_or(member.member_id.as_str());
            Line::from(vec![
                marker,
                " ".into(),
                dot,
                " ".into(),
                indent.into(),
                member.member_id.clone().into(),
                "  ".into(),
                status.dim(),
                "  ".into(),
                thread_id.dim(),
            ])
            .render(
                Rect::new(area.x, area.y + offset as u16, area.width, 1),
                buf,
            );
        }
    }

    fn render_summary(&self, area: Rect, buf: &mut Buffer) {
        let sealed = if self.sealed { "sealed" } else { "open" };
        let operation = self
            .operation_id
            .as_deref()
            .map_or_else(String::new, |id| format!("  operationId {id}"));
        Line::from(
            format!(
                "root {}  generation {}  {sealed}{operation}",
                self.root_thread_id, self.generation
            )
            .dim(),
        )
        .render(area, buf);
    }
}

pub(super) fn fleet_actions_params(
    root_thread_id: ThreadId,
    expected_generation: i64,
    sealed: bool,
    operation_id: Option<&str>,
    keymap: &RuntimeKeymap,
) -> SelectionViewParams {
    let operation_busy = operation_id.is_some();
    let busy_reason = operation_id.map(|id| format!("Operation {id} is still in progress."));
    let suspend_reason = if operation_busy {
        busy_reason.clone()
    } else if sealed {
        Some("Fleet is already sealed.".to_string())
    } else {
        None
    };
    let resume_reason = if operation_busy {
        busy_reason.clone()
    } else if !sealed {
        Some("Fleet is not suspended.".to_string())
    } else {
        None
    };
    let close_reason = if operation_busy { busy_reason } else { None };
    let action = |name: &'static str,
                  description: &'static str,
                  disabled_reason: Option<String>,
                  action: FleetAction| {
        let actions: Vec<SelectionAction> = if disabled_reason.is_none() {
            vec![Box::new(move |tx| {
                tx.send(match action {
                    FleetAction::Refresh => AppEvent::RefreshAgentsFleet,
                    FleetAction::Suspend => AppEvent::RequestAgentsFleetSuspend {
                        root_thread_id,
                        expected_generation,
                    },
                    FleetAction::Resume => AppEvent::RequestAgentsFleetResume {
                        root_thread_id,
                        expected_generation,
                    },
                    FleetAction::Close => AppEvent::OpenAgentsFleetCloseConfirmation {
                        root_thread_id,
                        expected_generation,
                    },
                })
            })]
        } else {
            Vec::new()
        };
        SelectionItem {
            name: name.to_string(),
            description: Some(description.to_string()),
            disabled_reason,
            actions,
            dismiss_on_select: true,
            ..Default::default()
        }
    };
    SelectionViewParams {
        title: Some("Manage agent fleet".to_string()),
        subtitle: Some(format!(
            "Root {root_thread_id} · generation {expected_generation}"
        )),
        footer_hint: Some(standard_popup_hint_line_for_keymap(&keymap.list)),
        items: vec![
            action(
                "Refresh status",
                "Read the current fleet generation and member states.",
                None,
                FleetAction::Refresh,
            ),
            action(
                "Suspend fleet",
                "Seal admissions and suspend leaves before the root.",
                suspend_reason,
                FleetAction::Suspend,
            ),
            action(
                "Resume fleet",
                "Recover the root and then its still-open descendants.",
                resume_reason,
                FleetAction::Resume,
            ),
            action(
                "Close fleet",
                "Close only when every member is idle or final. Confirmation required.",
                close_reason,
                FleetAction::Close,
            ),
        ],
        ..Default::default()
    }
}
#[derive(Clone, Copy)]
enum FleetAction {
    Refresh,
    Suspend,
    Resume,
    Close,
}

pub(super) fn fleet_close_confirmation_params(
    root_thread_id: ThreadId,
    expected_generation: i64,
    keymap: &RuntimeKeymap,
) -> SelectionViewParams {
    SelectionViewParams {
        title: Some("Close agent fleet?".to_string()),
        subtitle: Some(format!(
            "This closes idle/final members at generation {expected_generation}."
        )),
        footer_hint: Some(standard_popup_hint_line_for_keymap(&keymap.list)),
        items: vec![
            SelectionItem {
                name: "Close fleet".to_string(),
                description: Some(
                    "Preserve all rollouts while closing the fleet edges.".to_string(),
                ),
                actions: vec![Box::new(move |tx| {
                    tx.send(AppEvent::RequestAgentsFleetClose {
                        root_thread_id,
                        expected_generation,
                    });
                })],
                dismiss_on_select: true,
                ..Default::default()
            },
            SelectionItem {
                name: "Cancel".to_string(),
                dismiss_on_select: true,
                ..Default::default()
            },
        ],
        ..Default::default()
    }
}
