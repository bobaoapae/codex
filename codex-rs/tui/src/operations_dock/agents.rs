use ratatui::style::Stylize;
use ratatui::text::Line;

use super::state::DockAgentRow;

pub(super) fn lines(agents: &[DockAgentRow], selected: usize, focused: bool) -> Vec<Line<'static>> {
    if agents.is_empty() {
        return vec!["  No subagents yet".dim().into()];
    }
    agents
        .iter()
        .enumerate()
        .map(|(index, agent)| {
            let marker = if focused && index == selected {
                ">"
            } else {
                " "
            };
            vec![
                format!("{marker} ").into(),
                agent.label.clone().into(),
                "  ".into(),
                agent.status.clone().dim(),
            ]
            .into()
        })
        .collect()
}
