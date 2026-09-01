use anyhow::Result;
use codex_goal_extension::GoalService;
use codex_goal_extension::GoalServiceError;
use codex_state::SqliteConfig;
use codex_state::StateRuntime;
use codex_state::ThreadGoalStatus;
use codex_utils_absolute_path::AbsolutePathBuf;
use pretty_assertions::assert_eq;
use std::sync::Arc;
use std::time::SystemTime;
use std::time::UNIX_EPOCH;

fn test_home() -> AbsolutePathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system clock")
        .as_nanos();
    AbsolutePathBuf::from_absolute_path(
        std::env::temp_dir().join(format!("codex-goal-approved-plan-{nanos}")),
    )
    .expect("temporary path is absolute")
}

async fn test_runtime() -> Arc<StateRuntime> {
    StateRuntime::init(
        SqliteConfig::new_for_testing(test_home()),
        "test-provider".to_string(),
    )
    .await
    .expect("state runtime should initialize")
}

fn test_thread_id(value: u128) -> codex_protocol::ThreadId {
    codex_protocol::ThreadId::from_u128(value)
}

#[tokio::test]
async fn fork_invariant_approved_plan_claims_are_exclusive() -> Result<()> {
    let runtime = test_runtime().await;
    let service = Arc::new(GoalService::new());
    let thread_id = test_thread_id(1);

    let (first, second) = tokio::join!(
        service.claim_approved_plan_goal(&runtime, thread_id, "plan-1", 4, "Ship plan",),
        service.claim_approved_plan_goal(&runtime, thread_id, "plan-2", 9, "Other plan",),
    );
    let claimed = [first.as_ref(), second.as_ref()]
        .into_iter()
        .filter(|result| result.is_ok())
        .count();
    assert_eq!(claimed, 1);
    let conflict = [first, second]
        .into_iter()
        .find_map(|result| match result {
            Err(error) if error.is_goal_conflict() => Some(error),
            _ => None,
        })
        .expect("one claim should report goalConflict");
    assert!(
        matches!(conflict, GoalServiceError::InvalidRequest(message) if message.starts_with("goalConflict"))
    );
    let goal = runtime
        .thread_goals()
        .get_thread_goal(thread_id)
        .await?
        .expect("winning goal");
    assert_eq!(goal.status, ThreadGoalStatus::Active);
    assert!(goal.objective.contains("planId="));
    runtime.close().await;
    Ok(())
}

#[tokio::test]
async fn fork_invariant_complete_goal_can_be_replaced_but_unfinished_conflicts() -> Result<()> {
    let runtime = test_runtime().await;
    let service = GoalService::new();
    let complete_thread = test_thread_id(2);
    let complete = runtime
        .thread_goals()
        .replace_thread_goal(
            complete_thread,
            "finished plan",
            ThreadGoalStatus::Complete,
            None,
        )
        .await?;
    let replacement = service
        .claim_approved_plan_goal(&runtime, complete_thread, "plan-complete", 2, "New plan")
        .await?;
    assert_eq!(
        replacement.goal.objective,
        "Approved plan: New plan [planId=plan-complete, revision=2]"
    );
    assert_ne!(replacement.goal_id(), complete.goal_id);

    let budget_thread = test_thread_id(3);
    let budget = runtime
        .thread_goals()
        .replace_thread_goal(
            budget_thread,
            "budget limited",
            ThreadGoalStatus::Active,
            Some(0),
        )
        .await?;
    assert_eq!(budget.status, ThreadGoalStatus::BudgetLimited);
    let error = service
        .claim_approved_plan_goal(&runtime, budget_thread, "plan-budget", 1, "Blocked plan")
        .await
        .expect_err("budget-limited goal must conflict");
    assert!(error.is_goal_conflict());
    assert_eq!(
        runtime
            .thread_goals()
            .get_thread_goal(budget_thread)
            .await?
            .expect("budget goal")
            .objective,
        "budget limited"
    );
    runtime.close().await;
    Ok(())
}

#[tokio::test]
async fn fork_invariant_approved_plan_rollback_uses_compare_and_swap() -> Result<()> {
    let runtime = test_runtime().await;
    let service = GoalService::new();
    let thread_id = test_thread_id(4);
    let claimed = service
        .claim_approved_plan_goal(&runtime, thread_id, "plan-rollback", 1, "Rollback plan")
        .await?;
    let changed = runtime
        .thread_goals()
        .replace_thread_goal(thread_id, "changed by user", ThreadGoalStatus::Active, None)
        .await?;
    assert!(!claimed.rollback_claim_if_unchanged(&runtime).await?);
    assert_eq!(
        runtime
            .thread_goals()
            .get_thread_goal(thread_id)
            .await?
            .expect("changed goal"),
        changed
    );

    let clean_thread = test_thread_id(5);
    let clean_claim = service
        .claim_approved_plan_goal(&runtime, clean_thread, "plan-clean", 3, "Clean plan")
        .await?;
    assert!(clean_claim.rollback_claim_if_unchanged(&runtime).await?);
    assert!(
        runtime
            .thread_goals()
            .get_thread_goal(clean_thread)
            .await?
            .is_none()
    );
    runtime.close().await;
    Ok(())
}
