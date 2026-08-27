//! FORK: the `update_plan` checklist, carried across a compaction.
//!
//! `update_plan` emitted an event and stored nothing, so the plan lived only in
//! the transcript. Compaction rewrites the transcript, and the model came back
//! on the other side with no idea which step it was on — the observed shape is
//! an agent that had been on "step 4 of 6" restarting the list, or asking the
//! user where it was. Re-emitting the checklist as a small fragment costs a few
//! hundred tokens and removes that entirely.

use super::ContextualUserFragment;
use codex_protocol::models::ContentItemKind;
use codex_protocol::plan_tool::StepStatus;
use codex_protocol::plan_tool::UpdatePlanArgs;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CarriedPlan {
    rendered: String,
}

impl CarriedPlan {
    /// Renders a plan for re-injection, or `None` when there is nothing to say.
    pub(crate) fn new(plan: &UpdatePlanArgs) -> Option<Self> {
        if plan.plan.is_empty() {
            return None;
        }
        let mut rendered = String::from(
            "Current plan (carried across compaction; keep working from it, do not restart it):\n",
        );
        for item in &plan.plan {
            // Every step that was ever in the plan stays in it, including the
            // finished ones: a list that only shows what is left reads as a
            // shorter plan and invites the model to declare victory early.
            let marker = match item.status {
                StepStatus::Completed => "[x]",
                StepStatus::InProgress => "[>]",
                StepStatus::Pending => "[ ]",
            };
            rendered.push_str(&format!("{marker} {}\n", item.step));
        }
        if let Some(explanation) = plan
            .explanation
            .as_deref()
            .map(str::trim)
            .filter(|explanation| !explanation.is_empty())
        {
            rendered.push_str(&format!("\n{explanation}\n"));
        }
        Some(Self { rendered })
    }
}

impl ContextualUserFragment for CarriedPlan {
    fn content_kind(&self) -> ContentItemKind {
        ContentItemKind("plan.carried_across_compaction".to_string())
    }

    fn role(&self) -> &'static str {
        "user"
    }

    fn requires_separate_message(&self) -> bool {
        true
    }

    fn markers(&self) -> (&'static str, &'static str) {
        Self::type_markers()
    }

    fn type_markers() -> (&'static str, &'static str) {
        ("<current_plan>", "</current_plan>")
    }

    fn body(&self) -> String {
        self.rendered.clone()
    }
}
