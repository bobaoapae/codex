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
            state.scroll = 0;
        }
        KeyCode::Up => state.scroll = state.scroll.saturating_sub(1),
        KeyCode::Down => state.scroll = state.scroll.saturating_add(1),
        KeyCode::PageUp => state.scroll = state.scroll.saturating_sub(10),
        KeyCode::PageDown => state.scroll = state.scroll.saturating_add(10),
        _ => return false,
    }
    true
}
