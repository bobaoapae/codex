use super::agents_fleet_view::FleetDashboardStatus;
use crate::app::test_support::make_test_app;
use crate::app_event::AgentFleetOperationResponse;
use codex_app_server_protocol::AgentFleetStatusResponse;
use codex_app_server_protocol::AgentFleetSuspendResponse;
use codex_protocol::ThreadId;
use pretty_assertions::assert_eq;
use uuid::Uuid;

fn root_thread_id() -> ThreadId {
    ThreadId::from_string("00000000-0000-0000-0000-000000000901").expect("root thread id")
}

#[tokio::test]
async fn stale_status_request_is_ignored() {
    let mut app = make_test_app().await;
    let root = root_thread_id();
    let request_id = Uuid::new_v4();
    app.agents_overview.fleet.root_thread_id = Some(root);
    app.agents_overview.fleet.status_request_id = Some(request_id);
    app.agents_overview.fleet.generation = 7;

    app.apply_agents_fleet_status(Uuid::new_v4(), root, Err("stale response".to_string()));

    assert_eq!(
        (
            app.agents_overview.fleet.status_request_id,
            app.agents_overview.fleet.generation,
            app.agents_overview.fleet.notice.clone(),
        ),
        (Some(request_id), 7, None)
    );
}

#[tokio::test]
async fn status_for_another_root_is_ignored() {
    let mut app = make_test_app().await;
    let root = root_thread_id();
    let request_id = Uuid::new_v4();
    app.agents_overview.fleet.root_thread_id = Some(root);
    app.agents_overview.fleet.status_request_id = Some(request_id);
    app.agents_overview.fleet.generation = 7;

    app.apply_agents_fleet_status(
        request_id,
        root,
        Ok(AgentFleetStatusResponse {
            root_thread_id: ThreadId::new().to_string(),
            generation: 8,
            sealed: true,
            operation_id: Some("wrong-root-operation".to_string()),
            data: Vec::new(),
            next_cursor: None,
        }),
    );

    assert_eq!(
        (
            app.agents_overview.fleet.generation,
            app.agents_overview.fleet.sealed,
            app.agents_overview.fleet.status,
        ),
        (7, false, FleetDashboardStatus::Error)
    );
}

#[tokio::test]
async fn stale_operation_generation_does_not_overwrite_newer_state() {
    let mut app = make_test_app().await;
    let root = root_thread_id();
    let request_id = Uuid::new_v4();
    app.agents_overview.fleet.root_thread_id = Some(root);
    app.agents_overview.fleet.operation_request_id = Some(request_id);
    app.agents_overview.fleet.generation = 7;
    app.agents_overview.fleet.sealed = false;

    app.apply_agents_fleet_operation(
        request_id,
        root,
        6,
        Ok(AgentFleetOperationResponse::Suspend(
            AgentFleetSuspendResponse {
                root_thread_id: root.to_string(),
                generation: 6,
                sealed: true,
                operation_id: Some("stale-operation".to_string()),
                results: Vec::new(),
                next_cursor: None,
            },
        )),
    );

    assert_eq!(
        (
            app.agents_overview.fleet.generation,
            app.agents_overview.fleet.sealed,
            app.agents_overview.fleet.operation_id.clone(),
        ),
        (7, false, None)
    );
}
