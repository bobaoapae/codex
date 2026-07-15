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

pub(super) fn render(state: &mut OperationsDockState, area: Rect, buffer: &mut Buffer) {
    state.hit_regions.clear();
    if !state.visible() || area.is_empty() {
        return;
    }
    let focus = if state.focused { " • focused" } else { "" };
    let viewing_label = if state.viewing_label.is_empty() {
        "Main"
    } else {
        state.viewing_label.as_str()
    };
    let title = format!(
        " Operations {} | Agents | Viewing: {}{focus} ",
        tasks::summary(state.latest_plan()),
        viewing_label
    );
    let block = Block::default().borders(Borders::ALL).title(title.bold());
    let inner = block.inner(area);
    block.render(area, buffer);
    super::mouse::record_header_regions(state, area);
    if !state.expanded || inner.is_empty() {
        return;
    }

    if area.width >= 120 {
        let columns = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
            .split(inner);
        super::mouse::record_section(state, columns[0], DockTab::Tasks);
        super::mouse::record_section(state, columns[1], DockTab::Agents);
        Paragraph::new(tasks::lines(state.latest_plan(), state.scroll)).render(columns[0], buffer);
        Paragraph::new(agents::lines(
            &state.agents,
            state.scroll,
            state.focused,
            state.active_thread_id,
        ))
        .render(columns[1], buffer);
        super::mouse::record_agent_rows(state, columns[1], /*title_row*/ false);
    } else {
        let lines = match state.tab {
            DockTab::Tasks => tasks::lines(state.latest_plan(), state.scroll),
            DockTab::Agents => agents::lines(
                &state.agents,
                state.scroll,
                state.focused,
                state.active_thread_id,
            ),
        };
        let active_tab = state.tab;
        super::mouse::record_section(state, inner, active_tab);
        let tab = match state.tab {
            DockTab::Tasks => Line::from(" Tasks ").underlined(),
            DockTab::Agents => {
                Line::from(" Agents · Enter open · m main · I interrupt ").underlined()
            }
        };
        Paragraph::new(lines)
            .block(Block::default().title(tab))
            .render(inner, buffer);
        if state.tab == DockTab::Agents {
            super::mouse::record_agent_rows(state, inner, /*title_row*/ true);
        }
    }
}
