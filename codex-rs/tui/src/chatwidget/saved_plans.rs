//! FORK: loading plans persisted by Plan mode back into a session (`/plans`).
//!
//! The picker lists what `plan/list` returned; picking a plan opens a second popup that decides
//! what to do with it. The plan body itself is injected as hidden context ahead of the user's next
//! message, using the same `## My request for Codex:` delimiter the IDE context uses, so the
//! transcript, `/export` and the Desktop all show only the request.

use codex_app_server_protocol::PlanReadResponse;
use codex_app_server_protocol::PlanSummary;
use codex_app_server_protocol::UserInput;
use codex_protocol::config_types::ModeKind;
use ratatui::style::Stylize;
use ratatui::text::Line;
use uuid::Uuid;

use super::ChatWidget;
use super::PARENT_OWNED_INPUT_MESSAGE;
use super::plan_implementation::PLAN_IMPLEMENTATION_CODING_MESSAGE;
use super::plan_implementation::PLAN_IMPLEMENTATION_DEFAULT_UNAVAILABLE;
use crate::app_event::AppEvent;
use crate::app_event::SavedPlanAction;
use crate::bottom_pane::SelectionAction;
use crate::bottom_pane::SelectionItem;
use crate::bottom_pane::SelectionViewParams;
use crate::bottom_pane::popup_consts::standard_popup_hint_line;
use crate::collaboration_modes;
use crate::ide_context::PROMPT_REQUEST_BEGIN;
use crate::ide_context::prefixed_text_input;

pub(crate) const PLANS_PICKER_TITLE: &str = "Saved plans";
pub(crate) const SAVED_PLAN_REVISE_MESSAGE: &str = concat!(
    "Revise this plan: point out weaknesses, gaps and risks, then re-emit the full updated plan ",
    "as a complete <proposed_plan> block."
);
pub(crate) const SAVED_PLAN_PLAN_MODE_UNAVAILABLE: &str = "Plan mode unavailable";
pub(crate) const PLAN_SAVED_HINT: &str =
    "• Plan saved to ~/.codex/plans — use /plans to load it in another session.";

/// A plan the user loaded but has not sent yet.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct PendingPlanContext {
    pub(crate) id: String,
    pub(crate) title: String,
    pub(crate) path: String,
    pub(crate) markdown: String,
}

#[derive(Default)]
pub(crate) struct SavedPlansState {
    pub(crate) pending_context: Option<PendingPlanContext>,
    picker_request_id: Option<Uuid>,
    load_request_id: Option<Uuid>,
}

/// Hidden context text prepended to the next user message.
pub(crate) fn render_plan_context(context: &PendingPlanContext) -> String {
    let PendingPlanContext {
        title,
        path,
        markdown,
        ..
    } = context;
    format!(
        "# Saved plan: {title} ({path})\n\n{}\n",
        markdown.trim_end()
    )
}

/// Picker rows, newest first.
pub(crate) fn picker_params(plans: &[PlanSummary], current_cwd: &str) -> SelectionViewParams {
    let items = plans
        .iter()
        .map(|plan| {
            let id = plan.id.clone();
            let title = plan.title.clone();
            let action_title = title.clone();
            let actions: Vec<SelectionAction> = vec![Box::new(move |tx| {
                tx.send(AppEvent::OpenSavedPlanActions {
                    id: id.clone(),
                    title: action_title.clone(),
                });
            })];
            SelectionItem {
                name: title,
                description: Some(row_description(plan, current_cwd)),
                search_value: Some(format!(
                    "{} {} {}",
                    plan.title,
                    plan.cwd.clone().unwrap_or_default(),
                    plan.id
                )),
                actions,
                dismiss_on_select: false,
                dismiss_parent_on_child_accept: true,
                ..Default::default()
            }
        })
        .collect();

    SelectionViewParams {
        title: Some(PLANS_PICKER_TITLE.to_string()),
        subtitle: None,
        footer_hint: Some(standard_popup_hint_line()),
        items,
        is_searchable: true,
        ..Default::default()
    }
}

fn row_description(plan: &PlanSummary, current_cwd: &str) -> String {
    let updated = format_updated_at(plan.updated_at);
    let cwd = plan.cwd.as_deref().unwrap_or_default();
    let location = if !cwd.is_empty() && cwd == current_cwd {
        "this project".to_string()
    } else {
        basename(cwd)
    };
    if location.is_empty() {
        format!("{updated} · rev {}", plan.revision)
    } else {
        format!("{updated} · {location} · rev {}", plan.revision)
    }
}

fn basename(path: &str) -> String {
    path.rsplit(['/', '\\'])
        .find(|segment| !segment.is_empty())
        .unwrap_or_default()
        .to_string()
}

fn format_updated_at(updated_at: i64) -> String {
    chrono::DateTime::from_timestamp(updated_at, /*nsecs*/ 0).map_or_else(
        || "unknown".to_string(),
        |value| {
            value
                .with_timezone(&chrono::Local)
                .format("%Y-%m-%d %H:%M")
                .to_string()
        },
    )
}

/// Second popup: what to do with the plan the user picked.
pub(crate) fn load_plan_params(
    id: &str,
    title: &str,
    plan_mode_available: bool,
) -> SelectionViewParams {
    let plan_disabled_reason =
        (!plan_mode_available).then(|| SAVED_PLAN_PLAN_MODE_UNAVAILABLE.to_string());
    let rows: Vec<(&str, &str, SavedPlanAction, Option<String>)> = vec![
        (
            "Implement this plan",
            "Switch to Default mode and start coding from it.",
            SavedPlanAction::Implement,
            None,
        ),
        (
            "Attach to my next message",
            "Include the plan as hidden context with whatever you type next.",
            SavedPlanAction::AttachToNextMessage,
            None,
        ),
        (
            "Revise in Plan mode",
            "Ask the model to critique and re-emit the plan.",
            SavedPlanAction::Revise,
            plan_disabled_reason,
        ),
    ];

    let items = rows
        .into_iter()
        .map(|(name, description, action, disabled_reason)| {
            let plan_id = id.to_string();
            let actions: Vec<SelectionAction> = if disabled_reason.is_none() {
                vec![Box::new(move |tx| {
                    tx.send(AppEvent::LoadSavedPlan {
                        id: plan_id.clone(),
                        action,
                    });
                })]
            } else {
                Vec::new()
            };
            SelectionItem {
                name: name.to_string(),
                description: Some(description.to_string()),
                actions,
                disabled_reason,
                dismiss_on_select: true,
                ..Default::default()
            }
        })
        .collect();

    SelectionViewParams {
        title: Some(format!("Load plan «{title}»?")),
        subtitle: None,
        footer_hint: Some(standard_popup_hint_line()),
        items,
        ..Default::default()
    }
}

impl ChatWidget {
    pub(crate) fn begin_plans_picker_request(&mut self) -> Uuid {
        let request_id = Uuid::new_v4();
        self.saved_plans.picker_request_id = Some(request_id);
        request_id
    }

    /// Returns `false` for a stale response the user already superseded.
    pub(crate) fn finish_plans_picker_request(&mut self, request_id: Uuid) -> bool {
        if self.saved_plans.picker_request_id != Some(request_id) {
            return false;
        }
        self.saved_plans.picker_request_id = None;
        true
    }

    pub(crate) fn begin_saved_plan_load(&mut self) -> Uuid {
        let request_id = Uuid::new_v4();
        self.saved_plans.load_request_id = Some(request_id);
        request_id
    }

    pub(crate) fn finish_saved_plan_load(&mut self, request_id: Uuid) -> bool {
        if self.saved_plans.load_request_id != Some(request_id) {
            return false;
        }
        self.saved_plans.load_request_id = None;
        true
    }

    #[cfg(test)]
    pub(crate) fn pending_saved_plan_title(&self) -> Option<String> {
        self.saved_plans
            .pending_context
            .as_ref()
            .map(|context| context.title.clone())
    }

    pub(crate) fn show_plans_picker(&mut self, plans: Vec<PlanSummary>) {
        let current_cwd = self.config.cwd.as_path().to_string_lossy().to_string();
        self.bottom_pane
            .show_selection_view(picker_params(&plans, &current_cwd));
    }

    pub(crate) fn show_saved_plan_actions(&mut self, id: String, title: String) {
        let plan_mode_available = self.collaboration_modes_enabled()
            && collaboration_modes::plan_mask(self.model_catalog.as_ref()).is_some();
        self.bottom_pane
            .show_selection_view(load_plan_params(&id, &title, plan_mode_available));
    }

    pub(crate) fn apply_loaded_plan(&mut self, plan: PlanReadResponse, action: SavedPlanAction) {
        if self.blocks_direct_input {
            self.add_error_message(PARENT_OWNED_INPUT_MESSAGE.to_string());
            return;
        }
        let context = PendingPlanContext {
            id: plan.plan.id,
            title: plan.plan.title,
            path: plan.plan.path,
            markdown: plan.markdown,
        };
        let title = context.title.clone();
        let path = context.path.clone();
        self.saved_plans.pending_context = Some(context);
        self.add_info_message(format!("Loaded plan «{title}»"), Some(path));

        // A running or queued turn cannot switch collaboration mode, so fall back to attaching.
        let turn_busy =
            self.turn_lifecycle.agent_turn_running || self.has_queued_follow_up_messages();
        let action = match action {
            SavedPlanAction::Implement | SavedPlanAction::Revise if turn_busy => {
                SavedPlanAction::AttachToNextMessage
            }
            other => other,
        };

        match action {
            SavedPlanAction::Implement => {
                let Some(mask) =
                    collaboration_modes::default_mode_mask(self.model_catalog.as_ref())
                else {
                    self.add_error_message(PLAN_IMPLEMENTATION_DEFAULT_UNAVAILABLE.to_string());
                    return;
                };
                self.submit_user_message_with_mode(
                    PLAN_IMPLEMENTATION_CODING_MESSAGE.to_string(),
                    mask,
                );
            }
            SavedPlanAction::Revise => {
                let Some(mask) = collaboration_modes::plan_mask(self.model_catalog.as_ref()) else {
                    self.add_error_message(SAVED_PLAN_PLAN_MODE_UNAVAILABLE.to_string());
                    return;
                };
                self.submit_user_message_with_mode(SAVED_PLAN_REVISE_MESSAGE.to_string(), mask);
            }
            SavedPlanAction::AttachToNextMessage => {
                self.add_info_message(
                    format!(
                        "Plan «{title}» attached — it will be included with your next message."
                    ),
                    None,
                );
            }
        }
    }

    /// Prepend the pending plan to the outgoing message, ahead of any IDE context.
    pub(super) fn maybe_apply_pending_plan_context(&mut self, items: &mut Vec<UserInput>) {
        let Some(context) = self.saved_plans.pending_context.take() else {
            return;
        };
        let prefix = format!(
            "{}\n{PROMPT_REQUEST_BEGIN}\n",
            render_plan_context(&context)
        );
        match items
            .iter()
            .position(|item| matches!(item, UserInput::Text { .. }))
        {
            Some(text_index) => {
                let item = std::mem::replace(
                    &mut items[text_index],
                    UserInput::Text {
                        text: String::new(),
                        text_elements: Vec::new(),
                    },
                );
                let UserInput::Text {
                    text,
                    text_elements,
                } = item
                else {
                    unreachable!("position matched a text item");
                };
                items[text_index] = prefixed_text_input(prefix, text, text_elements);
            }
            None => items.insert(
                0,
                UserInput::Text {
                    text: prefix,
                    text_elements: Vec::new(),
                },
            ),
        }
    }

    /// One-line hint after Plan mode persists a plan.
    pub(super) fn on_plan_item_saved(&mut self) {
        if self.active_mode_kind() != ModeKind::Plan {
            return;
        }
        self.add_plain_history_lines(vec![Line::from(PLAN_SAVED_HINT.to_string().dim())]);
    }
}
