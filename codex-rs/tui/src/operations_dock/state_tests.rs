use super::*;
use codex_protocol::plan_tool::PlanItemArg;
use codex_protocol::plan_tool::StepStatus;
use crossterm::event::KeyCode;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;

fn plan(count: usize) -> UpdatePlanArgs {
    UpdatePlanArgs {
        explanation: Some("full update args".into()),
        plan: (0..count)
            .map(|index| PlanItemArg {
                step: format!("task {index}"),
                status: if index == 0 {
                    StepStatus::InProgress
                } else {
                    StepStatus::Pending
                },
            })
            .collect(),
    }
}

#[test]
fn dock_layout_snapshots_cover_responsive_boundary() {
    for width in [80, 119, 120, 160] {
        let mut state = OperationsDockState::new(OperationsDockMode::Auto);
        state.update_plan(plan(20));
        let area = Rect::new(0, 0, width, state.desired_height(30));
        let mut buffer = Buffer::empty(area);
        state.render(area, &mut buffer);
        insta::assert_snapshot!(
            format!("operations_dock_{width}"),
            buffer_to_string(&buffer)
        );
    }
}

#[test]
fn dock_keyboard_focus_and_scroll_are_self_contained() {
    let mut state = OperationsDockState::new(OperationsDockMode::Auto);
    state.update_plan(plan(20));
    assert!(state.focus());
    assert!(state.handle_key(KeyEvent::from(KeyCode::PageDown)));
    assert_eq!(state.scroll, 10);
    assert!(state.handle_key(KeyEvent::from(KeyCode::Tab)));
    assert_eq!(state.tab, DockTab::Agents);
    assert!(state.handle_key(KeyEvent::from(KeyCode::Esc)));
    assert!(!state.focused);
}

fn buffer_to_string(buffer: &Buffer) -> String {
    (0..buffer.area.height)
        .map(|y| {
            let line = (0..buffer.area.width)
                .map(|x| buffer[(x, y)].symbol())
                .collect::<String>();
            line.trim_end().to_string()
        })
        .collect::<Vec<_>>()
        .join("\n")
}
