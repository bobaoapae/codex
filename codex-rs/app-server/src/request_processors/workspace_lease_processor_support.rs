//! Safe wire projections and keyset pagination for workspace leases.

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use codex_app_server_protocol::JSONRPCErrorError;
use codex_app_server_protocol::WorkspaceLease;
use codex_app_server_protocol::WorkspaceLeaseGrant;
use codex_app_server_protocol::WorkspaceLeaseListParams;
use codex_app_server_protocol::WorkspaceLeaseMode;
use codex_app_server_protocol::WorkspaceLeaseState;
use codex_state::WorkflowLeaseMode;
use codex_state::WorkflowLeaseState;
use codex_state::WorkflowPathLease;
use serde::Deserialize;
use serde::Serialize;

use crate::error_code::internal_error;
use crate::error_code::invalid_request;

pub(super) const DEFAULT_LIMIT: u32 = 50;
pub(super) const MAX_LIMIT: u32 = 200;
pub(super) const DEFAULT_TTL_SECONDS: u64 = 15 * 60;
pub(super) const MAX_TTL_SECONDS: u64 = 24 * 60 * 60;

#[derive(Debug, Deserialize, Serialize)]
struct LeaseCursor {
    root_thread_id: String,
    owner_thread_id: Option<String>,
    path_filter: Option<String>,
    path_key: String,
    lease_id: String,
}

pub(super) fn state_mode(mode: WorkspaceLeaseMode) -> WorkflowLeaseMode {
    match mode {
        WorkspaceLeaseMode::Read => WorkflowLeaseMode::Read,
        WorkspaceLeaseMode::Write => WorkflowLeaseMode::Write,
    }
}

pub(super) fn api_mode(mode: WorkflowLeaseMode) -> WorkspaceLeaseMode {
    match mode {
        WorkflowLeaseMode::Read => WorkspaceLeaseMode::Read,
        WorkflowLeaseMode::Write => WorkspaceLeaseMode::Write,
    }
}

pub(super) fn api_state(state: WorkflowLeaseState) -> WorkspaceLeaseState {
    match state {
        WorkflowLeaseState::Active => WorkspaceLeaseState::Active,
        WorkflowLeaseState::Released => WorkspaceLeaseState::Released,
        WorkflowLeaseState::Expired => WorkspaceLeaseState::Expired,
        WorkflowLeaseState::Recoverable => WorkspaceLeaseState::Recoverable,
    }
}

pub(super) fn api_lease(lease: &WorkflowPathLease) -> WorkspaceLease {
    WorkspaceLease {
        lease_id: lease.lease_id.clone(),
        root_thread_id: lease.root_run_id.clone(),
        owner_thread_id: lease.owner_run_id.clone(),
        normalized_paths: vec![lease.path.display.clone()],
        mode: api_mode(lease.mode),
        state: api_state(lease.state),
        generation: lease.generation,
        environment_id: lease.environment_id.clone(),
        issued_at: milliseconds_to_seconds(lease.issued_at_ms),
        expires_at: lease.expires_at_ms.map(milliseconds_to_seconds),
        released_at: lease.released_at_ms.map(milliseconds_to_seconds),
    }
}

pub(super) fn api_grant(lease: &WorkflowPathLease) -> WorkspaceLeaseGrant {
    WorkspaceLeaseGrant {
        lease: api_lease(lease),
        token: lease.token.clone(),
    }
}

pub(super) fn filter_leases(
    leases: Vec<WorkflowPathLease>,
    params: &WorkspaceLeaseListParams,
) -> Vec<WorkflowPathLease> {
    leases
        .into_iter()
        .filter(|lease| {
            params
                .owner_thread_id
                .as_deref()
                .is_none_or(|owner| lease.owner_run_id == owner)
        })
        .filter(|lease| {
            params
                .path
                .as_deref()
                .is_none_or(|path| display_path_matches(&lease.path.display, path))
        })
        .collect()
}

pub(super) fn paginate_leases(
    leases: Vec<WorkflowPathLease>,
    params: &WorkspaceLeaseListParams,
) -> Result<(Vec<WorkspaceLease>, Option<String>), JSONRPCErrorError> {
    let limit = params.limit.unwrap_or(DEFAULT_LIMIT).clamp(1, MAX_LIMIT) as usize;
    let cursor = params
        .cursor
        .as_deref()
        .map(|cursor| decode_cursor(cursor, params))
        .transpose()?;
    let start = cursor
        .as_ref()
        .map(|cursor| {
            leases
                .iter()
                .position(|lease| {
                    (lease.path.comparison_key.as_str(), lease.lease_id.as_str())
                        > (cursor.path_key.as_str(), cursor.lease_id.as_str())
                })
                .unwrap_or(leases.len())
        })
        .unwrap_or_default();
    let end = start.saturating_add(limit).min(leases.len());
    let page = leases[start..end].iter().map(api_lease).collect::<Vec<_>>();
    let next_cursor = (end < leases.len())
        .then(|| leases.get(end.saturating_sub(1)))
        .flatten()
        .map(|lease| encode_cursor(lease, params))
        .transpose()?;
    Ok((page, next_cursor))
}

fn display_path_matches(display: &str, filter: &str) -> bool {
    if filter.is_empty() {
        return false;
    }
    let display = display.replace('\\', "/");
    let filter = filter.replace('\\', "/");
    let filter = filter.trim_end_matches('/');
    display == filter || display.starts_with(&format!("{filter}/"))
}

fn decode_cursor(
    encoded: &str,
    params: &WorkspaceLeaseListParams,
) -> Result<LeaseCursor, JSONRPCErrorError> {
    let bytes = URL_SAFE_NO_PAD
        .decode(encoded)
        .map_err(|_| invalid_request("invalid workspace lease pagination cursor"))?;
    let cursor: LeaseCursor = serde_json::from_slice(&bytes)
        .map_err(|_| invalid_request("invalid workspace lease pagination cursor"))?;
    if cursor.root_thread_id != params.root_thread_id
        || cursor.owner_thread_id != params.owner_thread_id
        || cursor.path_filter != params.path
        || cursor.path_key.is_empty()
        || cursor.lease_id.is_empty()
    {
        return Err(invalid_request(
            "workspace lease pagination cursor does not match the query",
        ));
    }
    Ok(cursor)
}

fn encode_cursor(
    lease: &WorkflowPathLease,
    params: &WorkspaceLeaseListParams,
) -> Result<String, JSONRPCErrorError> {
    let bytes = serde_json::to_vec(&LeaseCursor {
        root_thread_id: params.root_thread_id.clone(),
        owner_thread_id: params.owner_thread_id.clone(),
        path_filter: params.path.clone(),
        path_key: lease.path.comparison_key.clone(),
        lease_id: lease.lease_id.clone(),
    })
    .map_err(|error| internal_error(format!("failed to encode workspace lease cursor: {error}")))?;
    Ok(URL_SAFE_NO_PAD.encode(bytes))
}

fn milliseconds_to_seconds(value: i64) -> i64 {
    value.div_euclid(1_000)
}

#[cfg(test)]
#[path = "workspace_lease_processor_support_tests.rs"]
mod tests;
