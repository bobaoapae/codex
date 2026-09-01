//! Experimental app-server handlers for workspace path leases.
//!
//! Ownership and path normalization stay in `codex-core`; this boundary only
//! resolves the root/owner identities, applies list filters, and projects
//! lease metadata without leaking fencing tokens into list responses.

#[path = "workspace_lease_processor_support.rs"]
mod support;

use codex_app_server_protocol::ClientResponsePayload;
use codex_app_server_protocol::JSONRPCErrorError;
use codex_app_server_protocol::WorkspaceLeaseGrantParams;
use codex_app_server_protocol::WorkspaceLeaseGrantResponse;
use codex_app_server_protocol::WorkspaceLeaseListParams;
use codex_app_server_protocol::WorkspaceLeaseListResponse;
use codex_app_server_protocol::WorkspaceLeaseReleaseParams;
use codex_app_server_protocol::WorkspaceLeaseReleaseResponse;
use codex_core::ThreadManager;
use codex_core::ownership::AuthorizedWorkspaceRoots;
use codex_core::ownership::OwnershipActor;
use codex_core::ownership::OwnershipEnvironment;
use codex_core::ownership::OwnershipError;
use codex_core::ownership::OwnershipGrantRequest;
use codex_core::ownership::OwnershipReleaseRequest;
use codex_core::ownership::WorkspaceOwnershipService;
use codex_protocol::ThreadId;
use codex_rollout::StateDbHandle;
use codex_state::WorkflowStore;
use codex_thread_store::ReadThreadParams;
use codex_thread_store::ThreadStore;
use serde_json::json;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use crate::error_code::invalid_request;

use self::support::DEFAULT_TTL_SECONDS;
use self::support::MAX_TTL_SECONDS;
use self::support::api_grant;
use self::support::api_lease;
use self::support::filter_leases;
use self::support::paginate_leases;
use self::support::state_mode;

/// Handles the host-only workspace lease RPCs.
#[derive(Clone)]
pub(crate) struct WorkspaceLeaseRequestProcessor {
    thread_manager: Arc<ThreadManager>,
    thread_store: Arc<dyn ThreadStore>,
    config: Arc<codex_core::config::Config>,
    state_db: Option<StateDbHandle>,
}

impl WorkspaceLeaseRequestProcessor {
    pub(crate) fn new(
        thread_manager: Arc<ThreadManager>,
        thread_store: Arc<dyn ThreadStore>,
        config: Arc<codex_core::config::Config>,
        state_db: Option<StateDbHandle>,
    ) -> Self {
        Self {
            thread_manager,
            thread_store,
            config,
            state_db,
        }
    }

    pub(crate) async fn list(
        &self,
        params: WorkspaceLeaseListParams,
    ) -> Result<Option<ClientResponsePayload>, JSONRPCErrorError> {
        let root_thread_id = parse_thread_id(&params.root_thread_id, "root thread")?;
        validate_optional_filter(params.owner_thread_id.as_deref(), "ownerThreadId")?;
        validate_optional_filter(params.path.as_deref(), "path")?;
        if let Some(owner_thread_id) = params.owner_thread_id.as_deref() {
            parse_thread_id(owner_thread_id, "owner thread")?;
        }
        self.ensure_root_registered(root_thread_id).await?;
        let service = self.ownership_service(root_thread_id).await?;
        let leases = service
            .list_agent_ownership(OwnershipActor::root(root_thread_id))
            .await
            .map_err(|error| ownership_error("list", error))?;
        let leases = filter_leases(leases, &params);
        let (data, next_cursor) = paginate_leases(leases, &params)?;
        Ok(Some(
            WorkspaceLeaseListResponse { data, next_cursor }.into(),
        ))
    }

    pub(crate) async fn grant(
        &self,
        params: WorkspaceLeaseGrantParams,
    ) -> Result<Option<ClientResponsePayload>, JSONRPCErrorError> {
        let root_thread_id = parse_thread_id(&params.root_thread_id, "root thread")?;
        let owner_thread_id = parse_thread_id(&params.owner_thread_id, "owner thread")?;
        self.ensure_root_registered(root_thread_id).await?;
        self.ensure_owner_descendant(root_thread_id, owner_thread_id)
            .await?;
        let role = self.owner_role(owner_thread_id).await?;
        let target = if owner_thread_id == root_thread_id {
            OwnershipActor::root(owner_thread_id)
        } else {
            let target = OwnershipActor::subagent_for_role(owner_thread_id, role.as_deref());
            if !target.capabilities().may_request_workspace_lease() {
                return Err(ownership_error("grant", OwnershipError::ReadOnlyRole));
            }
            target
        };
        let ttl_seconds = params.ttl_seconds.unwrap_or(DEFAULT_TTL_SECONDS);
        if !(1..=MAX_TTL_SECONDS).contains(&ttl_seconds) {
            return Err(invalid_request(format!(
                "ttlSeconds must be between 1 and {MAX_TTL_SECONDS}"
            )));
        }
        if params.paths.is_empty() {
            return Err(invalid_request("paths must contain at least one path"));
        }
        let environment = params
            .environment_id
            .map_or(OwnershipEnvironment::Default, OwnershipEnvironment::Named);
        let request = OwnershipGrantRequest {
            requester: OwnershipActor::root(root_thread_id),
            target,
            paths: params.paths.into_iter().map(PathBuf::from).collect(),
            mode: state_mode(params.mode),
            lease_duration: Duration::from_secs(ttl_seconds),
            environment,
        };
        let service = self.ownership_service(root_thread_id).await?;
        let leases = service
            .grant_agent_ownership(request)
            .await
            .map_err(|error| ownership_error("grant", error))?;
        Ok(Some(
            WorkspaceLeaseGrantResponse {
                leases: leases.iter().map(api_grant).collect(),
            }
            .into(),
        ))
    }

    pub(crate) async fn release(
        &self,
        params: WorkspaceLeaseReleaseParams,
    ) -> Result<Option<ClientResponsePayload>, JSONRPCErrorError> {
        let root_thread_id = parse_thread_id(&params.root_thread_id, "root thread")?;
        self.ensure_root_registered(root_thread_id).await?;
        let service = self.ownership_service(root_thread_id).await?;
        let lease = service
            .release_agent_ownership(OwnershipReleaseRequest {
                requester: OwnershipActor::root(root_thread_id),
                lease_id: params.lease_id,
                token: params.token,
                generation: params.generation,
            })
            .await
            .map_err(|error| ownership_error("release", error))?;
        Ok(Some(
            WorkspaceLeaseReleaseResponse {
                lease: api_lease(&lease),
            }
            .into(),
        ))
    }

    async fn ownership_service(
        &self,
        root_thread_id: ThreadId,
    ) -> Result<Arc<WorkspaceOwnershipService>, JSONRPCErrorError> {
        match self.thread_manager.ownership_service(root_thread_id).await {
            Ok(service) => return Ok(service),
            Err(OwnershipError::Unavailable) => {}
            Err(error) => return Err(ownership_error("service", error)),
        }
        let workflow = self.workflow_store()?;
        let roots = AuthorizedWorkspaceRoots::new(self.config.effective_workspace_roots())
            .map_err(|error| ownership_error("service", OwnershipError::Path(error)))?;
        Ok(Arc::new(WorkspaceOwnershipService::new(
            workflow,
            root_thread_id,
            roots,
        )))
    }

    fn workflow_store(&self) -> Result<WorkflowStore, JSONRPCErrorError> {
        self.state_db
            .as_ref()
            .map(|state_db| state_db.workflow_store().clone())
            .ok_or_else(|| invalid_request("workspace lease state requires sqlite state"))
    }

    async fn ensure_root_registered(
        &self,
        root_thread_id: ThreadId,
    ) -> Result<(), JSONRPCErrorError> {
        if let Ok(thread) = self.thread_manager.get_thread(root_thread_id).await {
            if thread.config_snapshot().await.parent_thread_id.is_some() {
                return Err(invalid_request(
                    "workspace lease rootThreadId must identify a root thread",
                ));
            }
            return Ok(());
        }
        let stored = self
            .thread_store
            .read_thread(ReadThreadParams {
                thread_id: root_thread_id,
                include_archived: true,
                include_history: false,
            })
            .await
            .map_err(|error| {
                invalid_request(format!(
                    "workspace lease root thread is unavailable: {error}"
                ))
            })?;
        if stored.parent_thread_id.is_some() {
            return Err(invalid_request(
                "workspace lease rootThreadId must identify a root thread",
            ));
        }
        Ok(())
    }

    async fn ensure_owner_descendant(
        &self,
        root_thread_id: ThreadId,
        owner_thread_id: ThreadId,
    ) -> Result<(), JSONRPCErrorError> {
        let members = self
            .thread_manager
            .list_agent_subtree_thread_ids(root_thread_id)
            .await
            .map_err(|error| {
                invalid_request(format!(
                    "workspace lease agent graph is unavailable: {error}"
                ))
            })?;
        if !members.contains(&owner_thread_id) {
            return Err(invalid_request(
                "ownerThreadId must identify the root or a descendant agent",
            ));
        }
        Ok(())
    }

    async fn owner_role(
        &self,
        owner_thread_id: ThreadId,
    ) -> Result<Option<String>, JSONRPCErrorError> {
        if let Ok(thread) = self.thread_manager.get_thread(owner_thread_id).await {
            return Ok(thread
                .config_snapshot()
                .await
                .session_source
                .get_agent_role());
        }
        self.thread_store
            .read_thread(ReadThreadParams {
                thread_id: owner_thread_id,
                include_archived: true,
                include_history: false,
            })
            .await
            .map(|thread| thread.agent_role)
            .map_err(|error| {
                invalid_request(format!("workspace lease owner is unavailable: {error}"))
            })
    }
}

fn parse_thread_id(value: &str, label: &str) -> Result<ThreadId, JSONRPCErrorError> {
    ThreadId::from_string(value)
        .map_err(|error| invalid_request(format!("invalid {label} id: {error}")))
}

fn validate_optional_filter(value: Option<&str>, label: &str) -> Result<(), JSONRPCErrorError> {
    if value.is_some_and(str::is_empty) {
        return Err(invalid_request(format!("{label} must not be empty")));
    }
    Ok(())
}

fn ownership_error(operation: &str, error: OwnershipError) -> JSONRPCErrorError {
    let kind = match &error {
        OwnershipError::Conflict { .. } => "leaseConflict",
        OwnershipError::ReadOnlyRole => "readOnlyRole",
        OwnershipError::Path(_) => "pathRejected",
        OwnershipError::State { message } if message.contains("stale") => "staleLease",
        OwnershipError::Unavailable => "stateUnavailable",
        OwnershipError::RootRequired | OwnershipError::WrongRoot => "ownershipDenied",
        OwnershipError::OverrideNotNeeded
        | OwnershipError::OverrideRootOnly
        | OwnershipError::OverrideMismatch => "overrideDenied",
        OwnershipError::Receipt { .. } => "receiptFailed",
        OwnershipError::State { .. } => "stateError",
        OwnershipError::InvalidRequest { .. } => "invalidRequest",
        OwnershipError::LeaseRequired { .. } => "leaseRequired",
    };
    let mut mapped = invalid_request(format!("workspace lease {operation} failed: {error}"));
    mapped.data = Some(json!({
        "kind": kind,
        "operation": operation,
        "retry": false,
    }));
    mapped
}

#[cfg(test)]
#[path = "workspace_lease_processor_tests.rs"]
mod tests;
