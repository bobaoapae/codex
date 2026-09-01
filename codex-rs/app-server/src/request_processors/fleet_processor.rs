//! Experimental app-server handlers for durable agent-fleet lifecycle control.
//!
//! The lifecycle engine remains in `codex-core::ThreadManager`. This module
//! only validates the RPC boundary, adds app-server-only waiting-state
//! enrichment, and applies bounded keyset pagination to the public projection.

#[path = "fleet_processor_support.rs"]
mod support;

use codex_app_server_protocol::AgentFleetCloseParams;
use codex_app_server_protocol::AgentFleetCloseResponse;
use codex_app_server_protocol::AgentFleetResumeParams;
use codex_app_server_protocol::AgentFleetResumeResponse;
use codex_app_server_protocol::AgentFleetStatusParams;
use codex_app_server_protocol::AgentFleetStatusResponse;
use codex_app_server_protocol::AgentFleetSuspendParams;
use codex_app_server_protocol::AgentFleetSuspendResponse;
use codex_app_server_protocol::ClientResponsePayload;
use codex_app_server_protocol::JSONRPCErrorError;
use codex_core::FleetOperationResult;
use codex_core::ThreadManager;
use codex_core::config::Config;
use codex_protocol::ThreadId;
use codex_protocol::error::CodexErr;
use codex_protocol::error::CodexErrorDetails;
use codex_thread_store::ThreadStore;
use serde_json::json;
use std::sync::Arc;

use crate::error_code::internal_error;
use crate::error_code::invalid_request;
use crate::thread_state::ThreadStateManager;
use crate::thread_status::ThreadWatchManager;

use self::support::api_members;
use self::support::operation_results;
use self::support::paginate_members;

/// Handles the host-only fleet RPCs. None of these methods are registered as
/// model tools; they are dispatched exclusively from `MessageProcessor`.
#[derive(Clone)]
pub(crate) struct FleetRequestProcessor {
    thread_manager: Arc<ThreadManager>,
    thread_store: Arc<dyn ThreadStore>,
    config: Arc<Config>,
    thread_watch_manager: ThreadWatchManager,
    thread_state_manager: ThreadStateManager,
}

impl FleetRequestProcessor {
    pub(crate) fn new(
        thread_manager: Arc<ThreadManager>,
        thread_store: Arc<dyn ThreadStore>,
        config: Arc<Config>,
        thread_watch_manager: ThreadWatchManager,
        thread_state_manager: ThreadStateManager,
    ) -> Self {
        Self {
            thread_manager,
            thread_store,
            config,
            thread_watch_manager,
            thread_state_manager,
        }
    }

    pub(crate) async fn fleet_status(
        &self,
        params: AgentFleetStatusParams,
    ) -> Result<Option<ClientResponsePayload>, JSONRPCErrorError> {
        let root_thread_id = parse_root_thread_id(&params.root_thread_id)?;
        let status = self
            .thread_manager
            .fleet_status(root_thread_id)
            .await
            .map_err(|error| fleet_error("status", error))?;
        let members = api_members(
            &status,
            &self.thread_store,
            &self.thread_watch_manager,
            &self.thread_state_manager,
        )
        .await?;
        let (data, next_cursor) = paginate_members(
            members,
            root_thread_id,
            status.generation,
            params.cursor.as_deref(),
            params.limit,
        )?;
        Ok(Some(
            AgentFleetStatusResponse {
                root_thread_id: root_thread_id.to_string(),
                generation: status.generation,
                sealed: status.admissions_sealed,
                operation_id: status.active_operation_id,
                data,
                next_cursor,
            }
            .into(),
        ))
    }

    pub(crate) async fn fleet_suspend(
        &self,
        params: AgentFleetSuspendParams,
    ) -> Result<Option<ClientResponsePayload>, JSONRPCErrorError> {
        let root_thread_id = parse_root_thread_id(&params.root_thread_id)?;
        let operation = self
            .thread_manager
            .suspend_fleet(root_thread_id, params.expected_generation)
            .await
            .map_err(|error| fleet_error("suspend", error))?;
        let response = self
            .mutation_state(root_thread_id, &operation, "suspend")
            .await?;
        Ok(Some(
            AgentFleetSuspendResponse::from_response(response).into(),
        ))
    }

    pub(crate) async fn fleet_resume(
        &self,
        params: AgentFleetResumeParams,
    ) -> Result<Option<ClientResponsePayload>, JSONRPCErrorError> {
        let root_thread_id = parse_root_thread_id(&params.root_thread_id)?;
        let operation = self
            .thread_manager
            .resume_fleet(
                root_thread_id,
                params.expected_generation,
                self.config.as_ref().clone(),
            )
            .await
            .map_err(|error| fleet_error("resume", error))?;
        let response = self
            .mutation_state(root_thread_id, &operation, "resume")
            .await?;
        Ok(Some(
            AgentFleetResumeResponse::from_response(response).into(),
        ))
    }

    pub(crate) async fn fleet_close(
        &self,
        params: AgentFleetCloseParams,
    ) -> Result<Option<ClientResponsePayload>, JSONRPCErrorError> {
        let root_thread_id = parse_root_thread_id(&params.root_thread_id)?;
        let operation = self
            .thread_manager
            .close_fleet(root_thread_id, params.expected_generation)
            .await
            .map_err(|error| fleet_error("close", error))?;
        let response = self
            .mutation_state(root_thread_id, &operation, "close")
            .await?;
        Ok(Some(
            AgentFleetCloseResponse::from_response(response).into(),
        ))
    }

    async fn mutation_state(
        &self,
        root_thread_id: ThreadId,
        operation: &FleetOperationResult,
        operation_name: &'static str,
    ) -> Result<MutationResponse, JSONRPCErrorError> {
        let status = self
            .thread_manager
            .fleet_status(root_thread_id)
            .await
            .map_err(|error| {
                internal_error(format!(
                    "fleet {operation_name} completed but its state could not be read: {error}"
                ))
            })?;
        Ok(MutationResponse {
            root_thread_id: root_thread_id.to_string(),
            generation: status.generation,
            sealed: status.admissions_sealed,
            operation_id: Some(operation.operation_id.clone()),
            results: operation_results(operation)?,
        })
    }
}

struct MutationResponse {
    root_thread_id: String,
    generation: i64,
    sealed: bool,
    operation_id: Option<String>,
    results: Vec<codex_app_server_protocol::FleetResult>,
}

trait FleetMutationResponse: Sized {
    fn from_response(response: MutationResponse) -> Self;
}

impl FleetMutationResponse for AgentFleetSuspendResponse {
    fn from_response(response: MutationResponse) -> Self {
        Self {
            root_thread_id: response.root_thread_id,
            generation: response.generation,
            sealed: response.sealed,
            operation_id: response.operation_id,
            results: response.results,
            next_cursor: None,
        }
    }
}

impl FleetMutationResponse for AgentFleetResumeResponse {
    fn from_response(response: MutationResponse) -> Self {
        Self {
            root_thread_id: response.root_thread_id,
            generation: response.generation,
            sealed: response.sealed,
            operation_id: response.operation_id,
            results: response.results,
            next_cursor: None,
        }
    }
}

impl FleetMutationResponse for AgentFleetCloseResponse {
    fn from_response(response: MutationResponse) -> Self {
        Self {
            root_thread_id: response.root_thread_id,
            generation: response.generation,
            sealed: response.sealed,
            operation_id: response.operation_id,
            results: response.results,
            next_cursor: None,
        }
    }
}

fn parse_root_thread_id(value: &str) -> Result<ThreadId, JSONRPCErrorError> {
    ThreadId::from_string(value)
        .map_err(|error| invalid_request(format!("invalid fleet root thread id: {error}")))
}

fn fleet_error(operation: &str, error: CodexErr) -> JSONRPCErrorError {
    let stale_generation = matches!(
        error.details(),
        CodexErrorDetails::InvalidRequest(message)
            if message.contains("stale fleet generation")
    );
    let message = match error.details() {
        CodexErrorDetails::InvalidRequest(message)
            if message.contains("stale fleet generation") =>
        {
            format!("staleGeneration: {message}")
        }
        CodexErrorDetails::UnsupportedOperation(message) => {
            format!("fleet {operation} unavailable: {message}")
        }
        CodexErrorDetails::ThreadNotFound(thread_id) => {
            format!("fleet {operation} root not found: {thread_id}")
        }
        CodexErrorDetails::InvalidRequest(message) => {
            format!("fleet {operation} rejected: {message}")
        }
        _ => format!("fleet {operation} failed: {error}"),
    };
    let mut mapped = invalid_request(message);
    if stale_generation {
        mapped.data = Some(json!({
            "kind": "staleGeneration",
            "operation": operation,
            "retry": false,
        }));
    }
    mapped
}

#[cfg(test)]
#[path = "fleet_processor_tests.rs"]
mod tests;
