//! Conversion and bounded pagination helpers for the experimental fleet RPCs.

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use codex_app_server_protocol::CommandExecutionStatus;
use codex_app_server_protocol::DynamicToolCallStatus;
use codex_app_server_protocol::FleetMember;
use codex_app_server_protocol::FleetMemberState;
use codex_app_server_protocol::FleetResult;
use codex_app_server_protocol::JSONRPCErrorError;
use codex_app_server_protocol::McpToolCallStatus;
use codex_app_server_protocol::PatchApplyStatus;
use codex_app_server_protocol::ThreadActiveFlag;
use codex_app_server_protocol::ThreadItem;
use codex_app_server_protocol::ThreadStatus;
use codex_core::FleetAgentState as CoreFleetAgentState;
use codex_core::FleetMemberOutcome;
use codex_core::FleetOperationResult;
use codex_core::FleetStatus;
use codex_protocol::ThreadId;
use codex_thread_store::ReadThreadParams;
use codex_thread_store::ThreadStore;
use serde::Deserialize;
use serde::Serialize;
use std::sync::Arc;

use crate::error_code::internal_error;
use crate::error_code::invalid_request;
use crate::thread_state::ThreadStateManager;
use crate::thread_status::ThreadWatchManager;

pub(super) const DEFAULT_LIMIT: u32 = 50;
pub(super) const MAX_LIMIT: u32 = 200;

#[derive(Debug, Deserialize, Serialize)]
struct FleetCursor {
    root_thread_id: String,
    generation: i64,
    order_index: i64,
    member_id: String,
}

pub(super) fn fleet_agent_state(state: CoreFleetAgentState) -> FleetMemberState {
    match state {
        CoreFleetAgentState::Running => FleetMemberState::Running,
        CoreFleetAgentState::WaitingForTool => FleetMemberState::WaitingForTool,
        CoreFleetAgentState::WaitingForApproval => FleetMemberState::WaitingForApproval,
        CoreFleetAgentState::WaitingForUser => FleetMemberState::WaitingForUser,
        CoreFleetAgentState::Idle => FleetMemberState::Idle,
        CoreFleetAgentState::Suspended => FleetMemberState::Suspended,
        CoreFleetAgentState::Closed => FleetMemberState::Closed,
        CoreFleetAgentState::Failed => FleetMemberState::Failed,
    }
}

fn fleet_member_state(value: &str) -> Result<FleetMemberState, JSONRPCErrorError> {
    match value {
        "running" => Ok(FleetMemberState::Running),
        "waitingForTool" => Ok(FleetMemberState::WaitingForTool),
        "waitingForApproval" => Ok(FleetMemberState::WaitingForApproval),
        "waitingForUser" => Ok(FleetMemberState::WaitingForUser),
        "idle" => Ok(FleetMemberState::Idle),
        "suspended" => Ok(FleetMemberState::Suspended),
        "closed" => Ok(FleetMemberState::Closed),
        "failed" => Ok(FleetMemberState::Failed),
        _ => Err(internal_error(format!(
            "core returned unknown fleet member state `{value}`"
        ))),
    }
}

pub(super) fn operation_results(
    operation: &FleetOperationResult,
) -> Result<Vec<FleetResult>, JSONRPCErrorError> {
    operation.members.iter().map(api_operation_result).collect()
}

fn api_operation_result(result: &FleetMemberOutcome) -> Result<FleetResult, JSONRPCErrorError> {
    Ok(FleetResult {
        operation_id: result.operation_id.clone(),
        member_id: result.member_id.clone(),
        thread_id: result.thread_id.clone(),
        run_id: result.run_id.clone(),
        requested_state: fleet_member_state(&result.requested_state)?,
        previous_state: result
            .previous_state
            .as_deref()
            .map(fleet_member_state)
            .transpose()?,
        final_state: result
            .final_state
            .as_deref()
            .map(fleet_member_state)
            .transpose()?,
        success: result.success,
        error: result.error.clone(),
        depth: result.depth,
        order_index: result.order_index,
        updated_at: result.updated_at_ms / 1_000,
    })
}

pub(super) async fn api_members(
    status: &FleetStatus,
    thread_store: &Arc<dyn ThreadStore>,
    thread_watch_manager: &ThreadWatchManager,
    thread_state_manager: &ThreadStateManager,
) -> Result<Vec<FleetMember>, JSONRPCErrorError> {
    let root_thread_id = status.root_thread_id;
    let mut members = Vec::with_capacity(status.members.len());
    for member in &status.members {
        let thread_id = member.thread_id;
        let base_state = if status.state == "closed" {
            // A closed root is terminal. Do not let a stale loaded runtime
            // report itself as running after the graph has been closed.
            FleetMemberState::Closed
        } else {
            fleet_agent_state(member.state)
        };
        let state = enrich_running_state(
            thread_id,
            base_state,
            thread_watch_manager,
            thread_state_manager,
        )
        .await;
        let parent_member_id = if thread_id == root_thread_id {
            None
        } else {
            parent_member_id(thread_id, thread_store).await
        };
        let depth = i64::from(member.depth);
        let order_index = i64::try_from(member.order).map_err(|_| {
            internal_error(format!("fleet member order is too large for {thread_id}"))
        })?;
        let id = thread_id.to_string();
        members.push(FleetMember {
            member_id: id.clone(),
            thread_id: Some(id.clone()),
            run_id: Some(id),
            parent_member_id,
            state,
            depth,
            order_index,
            updated_at: unix_timestamp_seconds(),
        });
    }
    members.sort_by(|left, right| {
        left.order_index
            .cmp(&right.order_index)
            .then_with(|| left.member_id.cmp(&right.member_id))
    });
    Ok(members)
}

async fn parent_member_id(
    thread_id: ThreadId,
    thread_store: &Arc<dyn ThreadStore>,
) -> Option<String> {
    thread_store
        .read_thread(ReadThreadParams {
            thread_id,
            include_archived: true,
            include_history: false,
        })
        .await
        .ok()
        .and_then(|thread| thread.parent_thread_id.map(|parent| parent.to_string()))
}

async fn enrich_running_state(
    thread_id: ThreadId,
    state: FleetMemberState,
    thread_watch_manager: &ThreadWatchManager,
    thread_state_manager: &ThreadStateManager,
) -> FleetMemberState {
    if state != FleetMemberState::Running {
        return state;
    }

    match thread_watch_manager
        .loaded_status_for_thread(&thread_id.to_string())
        .await
    {
        ThreadStatus::Active { active_flags }
            if active_flags.contains(&ThreadActiveFlag::WaitingOnApproval) =>
        {
            return FleetMemberState::WaitingForApproval;
        }
        ThreadStatus::Active { active_flags }
            if active_flags.contains(&ThreadActiveFlag::WaitingOnUserInput) =>
        {
            return FleetMemberState::WaitingForUser;
        }
        ThreadStatus::SystemError => return FleetMemberState::Failed,
        ThreadStatus::NotLoaded | ThreadStatus::Idle | ThreadStatus::Active { .. } => {}
    }

    if has_pending_tool(thread_id, thread_state_manager).await {
        FleetMemberState::WaitingForTool
    } else {
        FleetMemberState::Running
    }
}

async fn has_pending_tool(thread_id: ThreadId, thread_state_manager: &ThreadStateManager) -> bool {
    let state = thread_state_manager.thread_state(thread_id).await;
    let active_turn = state.lock().await.active_turn_snapshot();
    active_turn.is_some_and(|turn| turn.items.iter().any(is_pending_tool))
}

fn is_pending_tool(item: &ThreadItem) -> bool {
    match item {
        ThreadItem::CommandExecution { status, .. } => {
            *status == CommandExecutionStatus::InProgress
        }
        ThreadItem::FileChange { status, .. } => *status == PatchApplyStatus::InProgress,
        ThreadItem::McpToolCall { status, .. } => *status == McpToolCallStatus::InProgress,
        ThreadItem::DynamicToolCall { status, .. } => *status == DynamicToolCallStatus::InProgress,
        ThreadItem::CollabAgentToolCall { status, .. } => {
            *status == codex_app_server_protocol::CollabAgentToolCallStatus::InProgress
        }
        ThreadItem::UserMessage { .. }
        | ThreadItem::HookPrompt { .. }
        | ThreadItem::AgentMessage { .. }
        | ThreadItem::FunctionCallOutput { .. }
        | ThreadItem::Plan { .. }
        | ThreadItem::Reasoning { .. }
        | ThreadItem::SubAgentActivity { .. }
        | ThreadItem::WebSearch { .. }
        | ThreadItem::ImageView { .. }
        | ThreadItem::Sleep { .. }
        | ThreadItem::ImageGeneration { .. }
        | ThreadItem::EnteredReviewMode { .. }
        | ThreadItem::ExitedReviewMode { .. }
        | ThreadItem::ContextCompaction { .. } => false,
    }
}

pub(super) fn paginate_members(
    members: Vec<FleetMember>,
    root_thread_id: ThreadId,
    generation: i64,
    cursor: Option<&str>,
    requested_limit: Option<u32>,
) -> Result<(Vec<FleetMember>, Option<String>), JSONRPCErrorError> {
    let limit = requested_limit.unwrap_or(DEFAULT_LIMIT).clamp(1, MAX_LIMIT) as usize;
    let cursor = cursor
        .map(|cursor| decode_cursor(cursor, root_thread_id, generation))
        .transpose()?;
    let start = cursor
        .as_ref()
        .map(|cursor| {
            members
                .iter()
                .position(|member| {
                    (member.order_index, &member.member_id)
                        > (cursor.order_index, &cursor.member_id)
                })
                .unwrap_or(members.len())
        })
        .unwrap_or_default();
    let end = start.saturating_add(limit).min(members.len());
    let page = members[start..end].to_vec();
    let next_cursor = (end < members.len())
        .then(|| page.last())
        .flatten()
        .map(|member| encode_cursor(root_thread_id, generation, member))
        .transpose()?;
    Ok((page, next_cursor))
}

fn decode_cursor(
    encoded: &str,
    root_thread_id: ThreadId,
    generation: i64,
) -> Result<FleetCursor, JSONRPCErrorError> {
    let bytes = URL_SAFE_NO_PAD
        .decode(encoded)
        .map_err(|_| invalid_request("invalid fleet pagination cursor"))?;
    let cursor: FleetCursor = serde_json::from_slice(&bytes)
        .map_err(|_| invalid_request("invalid fleet pagination cursor"))?;
    if cursor.root_thread_id != root_thread_id.to_string() || cursor.generation != generation {
        return Err(invalid_request(
            "fleet pagination cursor does not match root or generation",
        ));
    }
    if cursor.order_index < 0 || cursor.member_id.is_empty() {
        return Err(invalid_request("invalid fleet pagination cursor"));
    }
    Ok(cursor)
}

fn encode_cursor(
    root_thread_id: ThreadId,
    generation: i64,
    member: &FleetMember,
) -> Result<String, JSONRPCErrorError> {
    let cursor = serde_json::to_vec(&FleetCursor {
        root_thread_id: root_thread_id.to_string(),
        generation,
        order_index: member.order_index,
        member_id: member.member_id.clone(),
    })
    .map_err(|error| internal_error(format!("failed to encode fleet cursor: {error}")))?;
    Ok(URL_SAFE_NO_PAD.encode(cursor))
}

fn unix_timestamp_seconds() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| i64::try_from(duration.as_secs()).unwrap_or(i64::MAX))
        .unwrap_or_default()
}

#[cfg(test)]
#[path = "fleet_processor_support_tests.rs"]
mod tests;
