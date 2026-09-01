use super::*;

use codex_app_server_protocol::WorkspaceLeaseListParams;
use codex_protocol::ThreadId;
use codex_state::WorkflowLeaseMode;
use codex_state::WorkflowLeasePath;
use codex_state::WorkflowLeaseState;
use codex_state::WorkflowPathLease;
use pretty_assertions::assert_eq;

fn lease(id: &str, path: &str) -> WorkflowPathLease {
    WorkflowPathLease {
        lease_id: id.to_string(),
        token: format!("token-{id}"),
        root_run_id: ThreadId::from_u128(1).to_string(),
        owner_run_id: ThreadId::from_u128(2).to_string(),
        environment_id: Some("local".to_string()),
        path: WorkflowLeasePath::new(path, path).expect("absolute lease path"),
        mode: WorkflowLeaseMode::Write,
        generation: 1,
        expires_at_ms: Some(1_700_003_600_000),
        state: WorkflowLeaseState::Active,
        issued_at_ms: 1_700_000_000_000,
        released_at_ms: None,
        override_receipt_id: None,
    }
}

#[test]
fn lease_summary_is_token_free_and_grant_projection_keeps_token() {
    let lease = lease("lease-1", "/workspace/repo");
    let summary = serde_json::to_value(api_lease(&lease)).expect("lease summary JSON");
    assert!(!summary.to_string().contains("token"));
    assert_eq!(
        serde_json::to_value(api_grant(&lease)).expect("lease grant JSON")["token"],
        "token-lease-1"
    );
}

#[test]
fn lease_pagination_is_keyset_bound_to_filters() {
    let params = WorkspaceLeaseListParams {
        root_thread_id: ThreadId::from_u128(1).to_string(),
        owner_thread_id: Some(ThreadId::from_u128(2).to_string()),
        path: Some("/workspace".to_string()),
        cursor: None,
        limit: Some(1),
    };
    let leases = filter_leases(
        vec![
            lease("lease-1", "/workspace/a"),
            lease("lease-2", "/workspace/b"),
        ],
        &params,
    );
    assert_eq!(leases.len(), 2);
    let (page, next) = paginate_leases(leases, &params).expect("lease page");
    assert_eq!(page.len(), 1);
    let next = next.expect("second lease page cursor");

    let invalid = WorkspaceLeaseListParams {
        root_thread_id: ThreadId::from_u128(3).to_string(),
        cursor: Some(next),
        ..params
    };
    assert!(paginate_leases(vec![lease("lease-1", "/workspace/a")], &invalid).is_err());
}
