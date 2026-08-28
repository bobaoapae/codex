use codex_protocol::config_types::CollaborationModeMask;

use crate::app_event::AppEvent;
use crate::bottom_pane::SelectionAction;
use crate::bottom_pane::SelectionItem;
use crate::bottom_pane::SelectionViewParams;
use crate::bottom_pane::popup_consts::standard_popup_hint_line;

pub(super) const PLAN_IMPLEMENTATION_TITLE: &str = "Implement this plan?";
const PLAN_IMPLEMENTATION_YES: &str = "Yes, implement this plan";
const PLAN_IMPLEMENTATION_CLEAR_CONTEXT: &str = "Yes, clear context and implement";
const PLAN_IMPLEMENTATION_NO: &str = "No, stay in Plan mode";
pub(super) const PLAN_IMPLEMENTATION_CODING_MESSAGE: &str = "Implement the plan.";
pub(super) const PLAN_IMPLEMENTATION_CLEAR_CONTEXT_PREFIX: &str = concat!(
    "A previous agent produced the plan below to accomplish the user's task. ",
    "Implement the plan in a fresh context. Treat the plan as the source of ",
    "user intent, re-read files as needed, and carry the work through ",
    "implementation and verification."
);
pub(super) const PLAN_IMPLEMENTATION_DEFAULT_UNAVAILABLE: &str = "Default mode unavailable";
pub(super) const PLAN_IMPLEMENTATION_NO_APPROVED_PLAN: &str = "No approved plan available";
/// FORK: revision path out of the approval popup.
const PLAN_IMPLEMENTATION_DECISION_AUDIT: &str = "Ask me the open decisions first";
const PLAN_IMPLEMENTATION_REVISE: &str = "Revise the plan…";
pub(super) const PLAN_IMPLEMENTATION_PLAN_UNAVAILABLE: &str = "Plan mode unavailable";
pub(super) const PLAN_REVISE_COMPOSER_PREFIX: &str = "Revise the plan: ";
pub(super) const PLAN_DECISION_AUDIT_MESSAGE: &str = concat!(
    "Before I approve: list every design decision you resolved by assumption; for each, the ",
    "alternative you rejected and whether it should have been a question. Ask me the ones that ",
    "are mine to make via request_user_input, then re-emit the complete <proposed_plan>."
);

/// Builds the confirmation prompt shown after a plan is approved in Plan mode.
///
/// The optional usage label is already phrased for display, such as `89% used`
/// or `123K used`. This module only decides where that label belongs in the
/// decision copy so action wiring stays separate from token accounting.
pub(super) fn selection_view_params(
    default_mask: Option<CollaborationModeMask>,
    plan_markdown: Option<&str>,
    clear_context_usage_label: Option<&str>,
    plan_mask: Option<CollaborationModeMask>,
) -> SelectionViewParams {
    let (implement_actions, implement_disabled_reason) = match default_mask.clone() {
        Some(mask) => {
            let user_text = PLAN_IMPLEMENTATION_CODING_MESSAGE.to_string();
            let actions: Vec<SelectionAction> = vec![Box::new(move |tx| {
                tx.send(AppEvent::SubmitUserMessageWithMode {
                    text: user_text.clone(),
                    collaboration_mode: mask.clone(),
                });
            })];
            (actions, None)
        }
        None => (
            Vec::new(),
            Some(PLAN_IMPLEMENTATION_DEFAULT_UNAVAILABLE.to_string()),
        ),
    };

    let (clear_context_actions, clear_context_disabled_reason) = match (default_mask, plan_markdown)
    {
        (None, _) => (
            Vec::new(),
            Some(PLAN_IMPLEMENTATION_DEFAULT_UNAVAILABLE.to_string()),
        ),
        (Some(_), Some(plan_markdown)) if !plan_markdown.trim().is_empty() => {
            let user_text =
                format!("{PLAN_IMPLEMENTATION_CLEAR_CONTEXT_PREFIX}\n\n{plan_markdown}");
            let actions: Vec<SelectionAction> = vec![Box::new(move |tx| {
                tx.send(AppEvent::ClearUiAndSubmitUserMessage {
                    text: user_text.clone(),
                });
            })];
            (actions, None)
        }
        (Some(_), _) => (
            Vec::new(),
            Some(PLAN_IMPLEMENTATION_NO_APPROVED_PLAN.to_string()),
        ),
    };

    // FORK: staying in Plan mode with a concrete next step, instead of only yes/no.
    let (decision_audit_actions, plan_disabled_reason) = match plan_mask {
        Some(mask) => {
            let actions: Vec<SelectionAction> = vec![Box::new(move |tx| {
                tx.send(AppEvent::SubmitUserMessageWithMode {
                    text: PLAN_DECISION_AUDIT_MESSAGE.to_string(),
                    collaboration_mode: mask.clone(),
                });
            })];
            (actions, None)
        }
        None => (
            Vec::new(),
            Some(PLAN_IMPLEMENTATION_PLAN_UNAVAILABLE.to_string()),
        ),
    };
    let revise_actions: Vec<SelectionAction> = if plan_disabled_reason.is_none() {
        vec![Box::new(|tx| {
            tx.send(AppEvent::SetComposerText {
                text: PLAN_REVISE_COMPOSER_PREFIX.to_string(),
            });
        })]
    } else {
        Vec::new()
    };

    let clear_context_description = clear_context_usage_label.map_or_else(
        || "Fresh thread with this plan.".to_string(),
        |label| format!("Fresh thread. Context: {label}."),
    );

    SelectionViewParams {
        title: Some(PLAN_IMPLEMENTATION_TITLE.to_string()),
        subtitle: None,
        footer_hint: Some(standard_popup_hint_line()),
        items: vec![
            SelectionItem {
                name: PLAN_IMPLEMENTATION_YES.to_string(),
                description: Some("Switch to Default and start coding.".to_string()),
                selected_description: None,
                is_current: false,
                actions: implement_actions,
                disabled_reason: implement_disabled_reason,
                dismiss_on_select: true,
                ..Default::default()
            },
            SelectionItem {
                name: PLAN_IMPLEMENTATION_CLEAR_CONTEXT.to_string(),
                description: Some(clear_context_description),
                selected_description: None,
                is_current: false,
                actions: clear_context_actions,
                disabled_reason: clear_context_disabled_reason,
                dismiss_on_select: true,
                ..Default::default()
            },
            SelectionItem {
                name: PLAN_IMPLEMENTATION_NO.to_string(),
                description: Some("Continue planning with the model.".to_string()),
                selected_description: None,
                is_current: false,
                actions: Vec::new(),
                dismiss_on_select: true,
                ..Default::default()
            },
            SelectionItem {
                name: PLAN_IMPLEMENTATION_DECISION_AUDIT.to_string(),
                description: Some(
                    "The model lists what it assumed and asks you the product decisions before you approve."
                        .to_string(),
                ),
                selected_description: None,
                is_current: false,
                actions: decision_audit_actions,
                disabled_reason: plan_disabled_reason.clone(),
                dismiss_on_select: true,
                ..Default::default()
            },
            SelectionItem {
                name: PLAN_IMPLEMENTATION_REVISE.to_string(),
                description: Some("Stay in Plan mode and tell the model what to change.".to_string()),
                selected_description: None,
                is_current: false,
                actions: revise_actions,
                disabled_reason: plan_disabled_reason,
                dismiss_on_select: true,
                ..Default::default()
            },
        ],
        ..Default::default()
    }
}
