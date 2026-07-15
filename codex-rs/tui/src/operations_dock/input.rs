use crossterm::event::KeyCode;
use crossterm::event::KeyEvent;
use crossterm::event::KeyEventKind;

use super::state::DockTab;
use super::state::OperationsDockState;

pub(super) fn handle_key(state: &mut OperationsDockState, key: KeyEvent) -> bool {
    if !state.focused || !matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat) {
        return false;
    }
    match key.code {
        KeyCode::Esc => state.focused = false,
        KeyCode::Tab => {
            state.tab = match state.tab {
                DockTab::Tasks => DockTab::Agents,
                DockTab::Agents => DockTab::Tasks,
            };
        }
        KeyCode::Up => move_active_section(state, -1),
        KeyCode::Down => move_active_section(state, 1),
        KeyCode::PageUp => move_active_section(state, -10),
        KeyCode::PageDown => move_active_section(state, 10),
        _ => return false,
    }
    true
}

fn move_active_section(state: &mut OperationsDockState, delta: isize) {
    match state.tab {
        DockTab::Tasks => {
            state.task_scroll = state.task_scroll.saturating_add_signed(delta);
        }
        DockTab::Agents => state.move_agent_selection_by(delta),
    }
}
