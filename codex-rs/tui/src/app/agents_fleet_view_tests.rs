use super::*;
use crate::app::test_support::make_test_app;
use crate::bottom_pane::BottomPaneView;
use crate::chatwidget::tests::helpers::render_bottom_popup;
use crate::keymap::RuntimeKeymap;
use codex_app_server_protocol::FleetMemberState;
use codex_protocol::ThreadId;
use crossterm::event::KeyCode;
use crossterm::event::KeyEvent;
use crossterm::event::KeyModifiers;
use std::sync::Arc;
use tokio::sync::mpsc::unbounded_channel;

fn root_thread_id() -> ThreadId {
    ThreadId::from_string("00000000-0000-0000-0000-000000000801").expect("root thread id")
}

fn member(id: &str, state: FleetMemberState, depth: i64, order_index: i64) -> FleetMember {
    FleetMember {
        member_id: id.to_string(),
        thread_id: Some(id.to_string()),
        run_id: Some(id.to_string()),
        parent_member_id: None,
        state,
        depth,
        order_index,
        updated_at: 1_756_000_000,
    }
}

fn view(
    root: ThreadId,
    members: Vec<FleetMember>,
    status: FleetDashboardStatus,
    generation: i64,
    sealed: bool,
    operation_id: Option<&str>,
    notice: Option<&str>,
) -> (
    AgentsFleetView,
    tokio::sync::mpsc::UnboundedReceiver<AppEvent>,
) {
    let (event_tx, event_rx) = unbounded_channel();
    (
        AgentsFleetView::new(
            root,
            members,
            generation,
            sealed,
            operation_id.map(str::to_string),
            status,
            notice.map(str::to_string),
            crate::app_event_sender::AppEventSender::new(event_tx),
            RuntimeKeymap::defaults(),
            Arc::default(),
            false,
        ),
        event_rx,
    )
}

async fn render_view_snapshot(name: &str, dashboard: AgentsFleetView) {
    let mut app = make_test_app().await;
    app.chat_widget.show_bottom_pane_view(Box::new(dashboard));
    let popup = render_bottom_popup(&app.chat_widget, /*width*/ 104);
    insta::with_settings!({snapshot_path => "../snapshots"}, {
        insta::assert_snapshot!(name, popup);
    });
}

#[tokio::test]
async fn dashboard_renders_every_durable_member_state() {
    let root = root_thread_id();
    let ids = [
        "00000000-0000-0000-0000-000000000801",
        "00000000-0000-0000-0000-000000000802",
        "00000000-0000-0000-0000-000000000803",
        "00000000-0000-0000-0000-000000000804",
        "00000000-0000-0000-0000-000000000805",
        "00000000-0000-0000-0000-000000000806",
        "00000000-0000-0000-0000-000000000807",
        "00000000-0000-0000-0000-000000000808",
    ];
    let states = [
        FleetMemberState::Running,
        FleetMemberState::WaitingForTool,
        FleetMemberState::WaitingForApproval,
        FleetMemberState::WaitingForUser,
        FleetMemberState::Idle,
        FleetMemberState::Suspended,
        FleetMemberState::Closed,
        FleetMemberState::Failed,
    ];
    let members = ids
        .into_iter()
        .zip(states)
        .enumerate()
        .map(|(index, (id, state))| member(id, state, if index == 0 { 0 } else { 1 }, index as i64))
        .collect();
    let (dashboard, _events) = view(
        root,
        members,
        FleetDashboardStatus::Ready,
        42,
        true,
        Some("operation-42"),
        None,
    );
    render_view_snapshot("agents_fleet_all_states", dashboard).await;
}

#[tokio::test]
async fn dashboard_loading_and_empty_state_is_deterministic() {
    let (dashboard, _events) = view(
        root_thread_id(),
        Vec::new(),
        FleetDashboardStatus::Loading,
        0,
        false,
        None,
        None,
    );
    render_view_snapshot("agents_fleet_loading", dashboard).await;

    let (dashboard, _events) = view(
        root_thread_id(),
        Vec::new(),
        FleetDashboardStatus::Empty,
        7,
        false,
        None,
        Some("No members were returned by the fleet store."),
    );
    render_view_snapshot("agents_fleet_empty", dashboard).await;
}

#[tokio::test]
async fn dashboard_shows_recoverable_partial_operation() {
    let (dashboard, _events) = view(
        root_thread_id(),
        vec![
            member(
                "00000000-0000-0000-0000-000000000801",
                FleetMemberState::Suspended,
                0,
                0,
            ),
            member(
                "00000000-0000-0000-0000-000000000802",
                FleetMemberState::Failed,
                1,
                1,
            ),
        ],
        FleetDashboardStatus::Ready,
        43,
        true,
        Some("operation-43"),
        Some("Fleet suspend completed partially: 1 member(s) are recoverable (operation-43)."),
    );
    render_view_snapshot("agents_fleet_partial_recoverable", dashboard).await;
}

#[tokio::test]
async fn dashboard_shows_stale_generation_notice() {
    let (dashboard, _events) = view(
        root_thread_id(),
        vec![member(
            "00000000-0000-0000-0000-000000000801",
            FleetMemberState::Idle,
            0,
            0,
        )],
        FleetDashboardStatus::Ready,
        43,
        false,
        None,
        Some("Fleet status changed; refresh before sending this action again."),
    );
    render_view_snapshot("agents_fleet_stale_generation", dashboard).await;
}

#[tokio::test]
async fn fleet_action_popup_and_close_confirmation_are_explicit() {
    let mut app = make_test_app().await;
    app.chat_widget.show_selection_view(fleet_actions_params(
        root_thread_id(),
        42,
        false,
        None,
        &app.keymap,
    ));
    let actions = render_bottom_popup(&app.chat_widget, /*width*/ 104);
    insta::with_settings!({snapshot_path => "../snapshots"}, {
        insta::assert_snapshot!("agents_fleet_actions", actions);
    });

    app.chat_widget
        .show_selection_view(fleet_close_confirmation_params(
            root_thread_id(),
            42,
            &app.keymap,
        ));
    let confirmation = render_bottom_popup(&app.chat_widget, /*width*/ 104);
    insta::with_settings!({snapshot_path => "../snapshots"}, {
        insta::assert_snapshot!("agents_fleet_close_confirmation", confirmation);
    });
}

#[test]
fn manage_shortcut_carries_the_current_generation() {
    let (mut dashboard, mut events) = view(
        root_thread_id(),
        vec![member(
            "00000000-0000-0000-0000-000000000801",
            FleetMemberState::Idle,
            0,
            0,
        )],
        FleetDashboardStatus::Ready,
        42,
        false,
        None,
        Some("Fleet status changed; refresh before sending this action again."),
    );
    dashboard.handle_key_event(KeyEvent::new(KeyCode::Right, KeyModifiers::NONE));
    assert!(matches!(
        events.try_recv(),
        Ok(AppEvent::OpenAgentsFleetActions {
            root_thread_id: root,
            expected_generation: 42,
        }) if root == root_thread_id()
    ));
}
