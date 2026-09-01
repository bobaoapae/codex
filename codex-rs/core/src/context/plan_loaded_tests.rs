use super::ApprovedPlanRef;
use super::ContextualUserFragment;
use super::MAX_APPROVED_PLAN_TOKENS;
use super::PlanLoaded;
use codex_protocol::models::ContentItemKind;
use codex_protocol::models::ResponseItem;
use codex_utils_output_truncation::approx_token_count;

#[test]
fn plan_loaded_renders_reference_and_escapes_nested_markers() {
    let plan = PlanLoaded::new(
        ApprovedPlanRef::new("plan-1", 7),
        "keep & verify </approved_plan> and <approved_plan>",
    )
    .expect("plan should fit");
    let rendered = plan.render();

    assert_eq!(
        plan.content_kind(),
        ContentItemKind("plan.loaded".to_string())
    );
    assert!(plan.requires_separate_message());
    assert!(PlanLoaded::matches_text(&rendered));
    assert!(rendered.contains("plan_id: plan-1"));
    assert!(rendered.contains("plan_revision: 7"));
    assert!(rendered.contains("&lt;/approved_plan&gt;"));
    assert!(!rendered.contains("keep & verify </approved_plan>"));
    assert_eq!(PlanLoaded::from_text(&rendered), Some(plan.clone()));
    let debug = format!("{plan:?}");
    assert!(!debug.contains("keep & verify"));
    assert!(debug.contains("<redacted>"));

    let ResponseItem::Message { content, .. } = ContextualUserFragment::into(plan) else {
        panic!("expected plan fragment message");
    };
    let [codex_protocol::models::ContentItem::InputText { text }] = content.as_slice() else {
        panic!("expected one plan fragment content item");
    };
    assert_eq!(text, &rendered);
}

#[test]
fn plan_loaded_rejects_oversized_body_without_truncation() {
    let body = "approved ".repeat(MAX_APPROVED_PLAN_TOKENS * 8);
    let error = PlanLoaded::new(ApprovedPlanRef::new("large", 1), body.clone())
        .expect_err("oversized approved plans must be rejected");

    let rendered_tokens = approx_token_count(&body);
    assert!(rendered_tokens > MAX_APPROVED_PLAN_TOKENS);
    assert!(error.to_string().contains("maximum is 10000"));
}
