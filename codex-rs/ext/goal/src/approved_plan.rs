//! Goal-slot claims for approved immutable plan snapshots.

use codex_protocol::ThreadId;
use codex_protocol::protocol::validate_thread_goal_objective;

use crate::api::GoalService;
use crate::api::GoalServiceError;
use crate::api::GoalSetOutcome;
use crate::runtime::PreviousGoalSnapshot;
use crate::tool::fill_empty_thread_preview_if_possible;
use crate::tool::protocol_goal_from_state;

const GOAL_CONFLICT_ERROR_CODE: &str = "goalConflict";
const APPROVED_PLAN_ID_MAX_BYTES: usize = 256;

impl GoalServiceError {
    /// Whether this error is the stable approved-plan goal conflict.
    pub fn is_goal_conflict(&self) -> bool {
        matches!(
            self,
            Self::InvalidRequest(message) if message.starts_with(GOAL_CONFLICT_ERROR_CODE)
        )
    }
}

impl GoalSetOutcome {
    /// Roll back an approved-plan claim only when its goal ID is still current.
    /// A concurrent mutation therefore wins and is never deleted or replaced.
    pub async fn rollback_claim_if_unchanged(
        &self,
        state_db: &codex_state::StateRuntime,
    ) -> Result<bool, GoalServiceError> {
        if !self.rollback_claim {
            return Ok(false);
        }
        let rolled_back = match self.previous_state_goal.as_ref() {
            Some(previous_goal) => state_db
                .thread_goals()
                .restore_thread_goal_if_id(self.goal.thread_id, self.goal_id(), previous_goal)
                .await
                .map_err(|err| {
                    GoalServiceError::Internal(format!(
                        "failed to roll back approved plan goal: {err}"
                    ))
                })?,
            None => state_db
                .thread_goals()
                .delete_thread_goal_if_id(self.goal.thread_id, self.goal_id())
                .await
                .map_err(|err| {
                    GoalServiceError::Internal(format!(
                        "failed to remove approved plan goal: {err}"
                    ))
                })?,
        };
        Ok(rolled_back)
    }
}

impl GoalService {
    /// Atomically claim the goal slot for an approved immutable plan.
    ///
    /// The objective contains only the plan title and pinned `{plan_id,
    /// revision}` reference. The plan body is deliberately excluded so a
    /// draft edit cannot silently alter the durable goal identity. Existing
    /// active, paused, blocked, usage-limited, and budget-limited goals are
    /// never overwritten.
    pub async fn claim_approved_plan_goal(
        &self,
        state_db: &codex_state::StateRuntime,
        thread_id: ThreadId,
        plan_id: &str,
        revision: u32,
        title: &str,
    ) -> Result<GoalSetOutcome, GoalServiceError> {
        let objective = approved_plan_goal_objective(plan_id, revision, title)?;
        let runtime = self.runtime_for_thread(thread_id);
        let _goal_state_permit = match runtime.as_ref() {
            Some(runtime) => Some(
                runtime
                    .goal_state_permit()
                    .await
                    .map_err(GoalServiceError::Internal)?,
            ),
            None => None,
        };

        let claim = state_db
            .thread_goals()
            .claim_approved_plan_goal(thread_id, &objective)
            .await
            .map_err(|err| {
                GoalServiceError::Internal(format!("failed to claim approved plan goal: {err}"))
            })?;
        let (goal, previous_state_goal) = match claim {
            codex_state::ApprovedPlanGoalClaim::Claimed {
                goal,
                previous_goal,
            } => (goal, previous_goal),
            codex_state::ApprovedPlanGoalClaim::Conflict(goal) => {
                return Err(goal_conflict(thread_id, goal.status));
            }
        };

        if let Some(runtime) = runtime.as_ref() {
            runtime.invalidate_turn_lineage().await;
        }
        fill_empty_thread_preview_if_possible(state_db, thread_id, &goal).await;
        let previous_goal = previous_state_goal.as_ref().map(PreviousGoalSnapshot::from);
        Ok(GoalSetOutcome {
            goal: protocol_goal_from_state(goal.clone()),
            goal_id: goal.goal_id.clone(),
            state_goal: goal,
            previous_goal,
            previous_state_goal,
            rollback_claim: true,
        })
    }
}

fn approved_plan_goal_objective(
    plan_id: &str,
    revision: u32,
    title: &str,
) -> Result<String, GoalServiceError> {
    let plan_id = plan_id.trim();
    if plan_id.is_empty() || plan_id.len() > APPROVED_PLAN_ID_MAX_BYTES || plan_id.contains('\0') {
        return Err(GoalServiceError::InvalidRequest(
            "approved plan id must be non-empty and bounded".to_string(),
        ));
    }
    let title = title.trim();
    if title.is_empty() || title.contains('\0') {
        return Err(GoalServiceError::InvalidRequest(
            "approved plan title must be non-empty".to_string(),
        ));
    }
    let objective = format!("Approved plan: {title} [planId={plan_id}, revision={revision}]");
    validate_thread_goal_objective(&objective).map_err(GoalServiceError::InvalidRequest)?;
    Ok(objective)
}

fn goal_conflict(thread_id: ThreadId, status: codex_state::ThreadGoalStatus) -> GoalServiceError {
    GoalServiceError::InvalidRequest(format!(
        "{GOAL_CONFLICT_ERROR_CODE}: thread {thread_id} already has an unfinished goal ({})",
        status.as_str()
    ))
}
