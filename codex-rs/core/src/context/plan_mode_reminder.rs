//! FORK: per-turn reminder that Plan mode is still active.
//!
//! The full Plan-mode instructions enter the conversation once, when the mode is selected. On a
//! long thread they drift far behind the recent tool output, so each Plan-mode turn also gets a
//! short reminder of the rules that matter most for turn shape.

use super::ContextualUserFragment;
use codex_protocol::models::ContentItemKind;

pub(crate) struct PlanModeReminder;

impl ContextualUserFragment for PlanModeReminder {
    fn content_kind(&self) -> ContentItemKind {
        ContentItemKind("collaboration_mode.plan_reminder".to_string())
    }

    fn role(&self) -> &'static str {
        "developer"
    }

    fn markers(&self) -> (&'static str, &'static str) {
        Self::type_markers()
    }

    fn type_markers() -> (&'static str, &'static str) {
        ("<plan_mode_reminder>", "</plan_mode_reminder>")
    }

    fn body(&self) -> String {
        "Plan mode is still active (full instructions earlier in this conversation): read-only \
         except exploration; end this turn only with request_user_input, a <proposed_plan>, or a \
         direct answer; unresolved preferences/tradeoffs must be asked, never assumed."
            .to_string()
    }
}

#[cfg(test)]
#[path = "plan_mode_reminder_tests.rs"]
mod tests;
