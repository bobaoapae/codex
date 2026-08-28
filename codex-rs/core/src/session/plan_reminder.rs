//! FORK: records the per-turn Plan-mode reminder.

use super::session::Session;
use super::step_context::StepContext;
use super::turn_context::TurnContext;
use crate::context::ContextualUserFragment;
use crate::context::PlanModeReminder;
use codex_protocol::config_types::ModeKind;

/// Record one `<plan_mode_reminder>` per Plan-mode turn.
///
/// `recorded` is owned by the `run_turn` loop so the reminder is emitted once per turn rather
/// than once per model step / tool call.
pub(super) async fn maybe_record_plan_mode_reminder(
    sess: &Session,
    turn_context: &TurnContext,
    step_context: &StepContext,
    recorded: &mut bool,
) {
    if *recorded {
        return;
    }
    if step_context.settings.selected_collaboration_mode().mode != ModeKind::Plan {
        return;
    }
    *recorded = true;
    let response_item = ContextualUserFragment::into(PlanModeReminder);
    sess.record_conversation_items(turn_context, std::slice::from_ref(&response_item))
        .await;
}
