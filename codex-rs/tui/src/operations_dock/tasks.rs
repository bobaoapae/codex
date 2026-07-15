use codex_protocol::plan_tool::StepStatus;
use codex_protocol::plan_tool::UpdatePlanArgs;
use ratatui::style::Stylize;
use ratatui::text::Line;

pub(super) fn summary(plan: Option<&UpdatePlanArgs>) -> String {
    let Some(plan) = plan else {
        return "Tasks".to_string();
    };
    let completed = plan
        .plan
        .iter()
        .filter(|item| matches!(item.status, StepStatus::Completed))
        .count();
    if completed == plan.plan.len() && !plan.plan.is_empty() {
        format!("Tasks ✓ {completed}/{}", plan.plan.len())
    } else {
        format!("Tasks {completed}/{}", plan.plan.len())
    }
}

pub(super) fn lines(plan: Option<&UpdatePlanArgs>, scroll: usize) -> Vec<Line<'static>> {
    let Some(plan) = plan else {
        return vec!["  No plan yet".dim().into()];
    };
    plan.plan
        .iter()
        .skip(scroll.min(plan.plan.len()))
        .map(|item| {
            let marker = match item.status {
                StepStatus::Pending => "○",
                StepStatus::InProgress => "●",
                StepStatus::Completed => "✓",
            };
            vec![format!(" {marker} ").into(), item.step.clone().into()].into()
        })
        .collect()
}
