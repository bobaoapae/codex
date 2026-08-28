use super::*;
use pretty_assertions::assert_eq;

#[test]
fn render_wraps_body_in_markers() {
    let rendered = PlanModeReminder.render();
    assert!(rendered.starts_with("<plan_mode_reminder>"));
    assert!(rendered.ends_with("</plan_mode_reminder>"));
    assert!(rendered.contains("Plan mode is still active"));
    assert!(rendered.contains("request_user_input"));
}

#[test]
fn role_and_kind_are_developer_scoped() {
    assert_eq!(PlanModeReminder.role(), "developer");
    assert_eq!(
        PlanModeReminder.content_kind(),
        ContentItemKind("collaboration_mode.plan_reminder".to_string())
    );
}

#[test]
fn matches_text_recognizes_rendered_fragment() {
    assert!(PlanModeReminder::matches_text(&PlanModeReminder.render()));
    assert!(!PlanModeReminder::matches_text("plain user message"));
}
