use codex_protocol::ThreadId;
use crossterm::event::MouseButton;
use crossterm::event::MouseEvent;
use crossterm::event::MouseEventKind;
use ratatui::layout::Position;
use ratatui::layout::Rect;

use super::state::DockTab;
use super::state::OperationsDockState;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DockMouseAction {
    Ignored,
    Consumed,
    OpenAgent(ThreadId),
}

#[derive(Debug, Clone, Copy)]
pub(super) struct HitRegion {
    area: Rect,
    target: HitTarget,
}

#[derive(Debug, Clone, Copy)]
enum HitTarget {
    Header(DockTab),
    Section(DockTab),
    Agent(ThreadId),
}

pub(super) fn record_section(state: &mut OperationsDockState, area: Rect, tab: DockTab) {
    state.hit_regions.push(HitRegion {
        area,
        target: HitTarget::Section(tab),
    });
}

pub(super) fn record_header_regions(state: &mut OperationsDockState, area: Rect) {
    let left_width = area.width / 2;
    state.hit_regions.push(HitRegion {
        area: Rect::new(area.x, area.y, left_width, 1),
        target: HitTarget::Header(DockTab::Tasks),
    });
    state.hit_regions.push(HitRegion {
        area: Rect::new(
            area.x.saturating_add(left_width),
            area.y,
            area.width.saturating_sub(left_width),
            1,
        ),
        target: HitTarget::Header(DockTab::Agents),
    });
}

pub(super) fn record_agent_rows(state: &mut OperationsDockState, area: Rect, title_row: bool) {
    let title_height = u16::from(title_row);
    let start_y = area.y.saturating_add(title_height);
    let available = usize::from(area.height.saturating_sub(title_height));
    let rows: Vec<_> = state
        .agents
        .iter()
        .skip(state.agent_scroll)
        .take(available)
        .enumerate()
        .map(|(visible_index, agent)| HitRegion {
            area: Rect::new(
                area.x,
                start_y.saturating_add(u16::try_from(visible_index).unwrap_or(u16::MAX)),
                area.width,
                1,
            ),
            target: HitTarget::Agent(agent.thread_id),
        })
        .collect();
    state.hit_regions.extend(rows);
}

pub(super) fn handle_mouse(state: &mut OperationsDockState, event: MouseEvent) -> DockMouseAction {
    let position = Position::new(event.column, event.row);
    match event.kind {
        MouseEventKind::Down(MouseButton::Left) => {
            let Some(target) = state
                .hit_regions
                .iter()
                .rev()
                .find(|region| region.area.contains(position))
                .map(|region| region.target)
            else {
                return DockMouseAction::Ignored;
            };
            state.focused = true;
            state.expanded = true;
            match target {
                HitTarget::Header(tab) => {
                    state.tab = tab;
                    DockMouseAction::Consumed
                }
                HitTarget::Section(tab) => {
                    state.tab = tab;
                    DockMouseAction::Consumed
                }
                HitTarget::Agent(thread_id) => DockMouseAction::OpenAgent(thread_id),
            }
        }
        MouseEventKind::ScrollUp | MouseEventKind::ScrollDown => {
            let target = state
                .hit_regions
                .iter()
                .rev()
                .find(|region| region.area.contains(position))
                .map(|region| region.target);
            let Some(target) = target else {
                return DockMouseAction::Ignored;
            };
            state.focused = true;
            state.tab = match target {
                HitTarget::Header(tab) | HitTarget::Section(tab) => tab,
                HitTarget::Agent(_) => DockTab::Agents,
            };
            let delta = match event.kind {
                MouseEventKind::ScrollUp => -3,
                MouseEventKind::ScrollDown => 3,
                _ => unreachable!(),
            };
            match state.tab {
                DockTab::Tasks => {
                    state.task_scroll = state.task_scroll.saturating_add_signed(delta);
                }
                DockTab::Agents => state.move_agent_selection_by(delta),
            }
            DockMouseAction::Consumed
        }
        _ => DockMouseAction::Ignored,
    }
}
