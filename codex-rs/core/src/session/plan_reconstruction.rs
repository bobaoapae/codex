use crate::context::CarriedPlan;
use crate::context::PlanLoaded;
use codex_history::ResponseItemEnvelope;
use codex_protocol::plan_tool::UpdatePlanArgs;

/// Restore plan state from history that has already applied compaction and rollback.
pub(super) fn restore_plans(
    history: &[ResponseItemEnvelope],
    last_plan: Option<UpdatePlanArgs>,
) -> (Option<UpdatePlanArgs>, Option<PlanLoaded>) {
    (
        last_plan.or_else(|| CarriedPlan::from_history(history)),
        PlanLoaded::from_history(history),
    )
}
