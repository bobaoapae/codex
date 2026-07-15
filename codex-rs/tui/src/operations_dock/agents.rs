use ratatui::style::Stylize;
use ratatui::text::Line;

use super::state::DockAgentRow;

pub(super) fn lines(
    agents: &[DockAgentRow],
    selected: usize,
    focused: bool,
    active_thread_id: Option<codex_protocol::ThreadId>,
) -> Vec<Line<'static>> {
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
            let active = if Some(agent.thread_id) == active_thread_id {
                "●"
            } else {
                " "
            };
            vec![
                format!("{marker}{active} ").into(),
                agent.label.clone().into(),
                "  ".into(),
                agent.status.clone().dim(),
            ]
            .into()
        })
        .collect()
}
