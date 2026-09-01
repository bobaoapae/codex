use super::ApprovedPlanContext;
use super::TurnInputRequest;

#[test]
fn approved_plan_builder_keeps_context_internal_to_the_request() {
    let request =
        TurnInputRequest::user_input(Vec::new()).with_approved_plan("plan-7", 3, "approved body");

    assert_eq!(
        request.approved_plan,
        Some(ApprovedPlanContext {
            id: "plan-7".to_string(),
            revision: 3,
            body: "approved body".to_string(),
        })
    );
    assert_eq!(TurnInputRequest::user_input(Vec::new()).approved_plan, None);
    let debug = format!("{request:?}");
    assert!(!debug.contains("approved body"));
    assert!(debug.contains("<redacted>"));
}
