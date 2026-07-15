use ratatui::buffer::Buffer;
use ratatui::layout::Constraint;
use ratatui::layout::Direction;
use ratatui::layout::Layout;
use ratatui::layout::Rect;
use ratatui::style::Stylize;
use ratatui::text::Line;
use ratatui::widgets::Block;
use ratatui::widgets::Borders;
use ratatui::widgets::Paragraph;
use ratatui::widgets::Widget;

use super::agents;
use super::state::DockTab;
use super::state::OperationsDockState;
use super::tasks;

pub(super) fn desired_height(state: &OperationsDockState, terminal_height: u16) -> u16 {
    if !state.visible() {
        return 0;
    }
    if !state.expanded {
        return 3;
    }
    let rows = state
        .latest_plan()
        .map_or(1, |plan| plan.plan.len())
        .saturating_add(2);
    u16::try_from(rows)
        .unwrap_or(u16::MAX)
        .clamp(3, (terminal_height / 3).max(3))
}

pub(super) fn render(state: &OperationsDockState, area: Rect, buffer: &mut Buffer) {
    if !state.visible() || area.is_empty() {
        return;
    }
    let focus = if state.focused { " • focused" } else { "" };
    let title = format!(
        " Operations {} | Agents{focus} ",
        tasks::summary(state.latest_plan())
    );
    let block = Block::default().borders(Borders::ALL).title(title.bold());
    let inner = block.inner(area);
    block.render(area, buffer);
    if !state.expanded || inner.is_empty() {
        return;
    }

    if area.width >= 120 {
        let columns = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
            .split(inner);
        Paragraph::new(tasks::lines(state.latest_plan(), state.scroll)).render(columns[0], buffer);
        Paragraph::new(agents::lines(&state.agents, state.scroll, state.focused))
            .render(columns[1], buffer);
    } else {
        let lines = match state.tab {
            DockTab::Tasks => tasks::lines(state.latest_plan(), state.scroll),
            DockTab::Agents => agents::lines(&state.agents, state.scroll, state.focused),
        };
        let tab = match state.tab {
            DockTab::Tasks => Line::from(" Tasks ").underlined(),
            DockTab::Agents => Line::from(" Agents ").underlined(),
        };
        Paragraph::new(lines)
            .block(Block::default().title(tab))
            .render(inner, buffer);
    }
}
