use super::*;

use codex_core::FleetAgentState as CoreFleetAgentState;
use codex_protocol::ThreadId;
use pretty_assertions::assert_eq;

fn member(order_index: i64) -> FleetMember {
    let id = format!("member-{order_index}");
    FleetMember {
        member_id: id.clone(),
        thread_id: Some(id.clone()),
        run_id: Some(id),
        parent_member_id: None,
        state: FleetMemberState::Idle,
        depth: 0,
        order_index,
        updated_at: 1_700_000_000,
    }
}

#[test]
fn maps_every_core_fleet_state_to_the_wire_vocabulary() {
    let states = [
        (CoreFleetAgentState::Running, FleetMemberState::Running),
        (
            CoreFleetAgentState::WaitingForTool,
            FleetMemberState::WaitingForTool,
        ),
        (
            CoreFleetAgentState::WaitingForApproval,
            FleetMemberState::WaitingForApproval,
        ),
        (
            CoreFleetAgentState::WaitingForUser,
            FleetMemberState::WaitingForUser,
        ),
        (CoreFleetAgentState::Idle, FleetMemberState::Idle),
        (CoreFleetAgentState::Suspended, FleetMemberState::Suspended),
        (CoreFleetAgentState::Closed, FleetMemberState::Closed),
        (CoreFleetAgentState::Failed, FleetMemberState::Failed),
    ];

    for (core, expected) in states {
        assert_eq!(fleet_agent_state(core), expected);
    }
}

#[tokio::test]
async fn running_state_is_enriched_from_pending_app_server_requests() {
    let thread_id = ThreadId::from_u128(1);
    let watch = ThreadWatchManager::new();
    let state = ThreadStateManager::new();
    watch.upsert_thread(&thread_id.to_string()).await;
    watch.note_turn_started(&thread_id.to_string()).await;
    let permission_guard = watch
        .note_permission_requested(&thread_id.to_string())
        .await;

    assert_eq!(
        enrich_running_state(thread_id, FleetMemberState::Running, &watch, &state).await,
        FleetMemberState::WaitingForApproval
    );
    drop(permission_guard);
}

#[test]
fn fleet_member_pagination_is_keyset_bound_to_root_and_generation() {
    let root_thread_id = ThreadId::from_u128(42);
    let members = vec![member(0), member(1), member(2)];
    let (first, next_cursor) = paginate_members(members.clone(), root_thread_id, 7, None, Some(2))
        .expect("first fleet page");
    assert_eq!(
        first
            .iter()
            .map(|member| member.member_id.as_str())
            .collect::<Vec<_>>(),
        vec!["member-0", "member-1"]
    );
    let next_cursor = next_cursor.expect("second fleet page cursor");
    let (second, no_cursor) =
        paginate_members(members, root_thread_id, 7, Some(&next_cursor), Some(2))
            .expect("second fleet page");
    assert_eq!(
        second
            .iter()
            .map(|member| member.member_id.as_str())
            .collect::<Vec<_>>(),
        vec!["member-2"]
    );
    assert_eq!(no_cursor, None);

    assert!(
        paginate_members(
            vec![member(0)],
            root_thread_id,
            8,
            Some(&next_cursor),
            Some(2),
        )
        .is_err()
    );
}
