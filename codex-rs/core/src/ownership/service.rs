use super::AuthorizedWorkspaceRoots;
use super::NormalizedLeasePath;
use super::service_helpers::*;
use super::service_types::*;
use codex_protocol::ThreadId;
use codex_state::WorkflowLeaseAcquireRequest;
use codex_state::WorkflowLeaseAuthority;
use codex_state::WorkflowLeaseConflict;
use codex_state::WorkflowLeasePath;
use codex_state::WorkflowLeaseReleaseRequest;
use codex_state::WorkflowPathLease;
use codex_state::WorkflowStore;
use std::path::PathBuf;

/// Errors returned by the ownership admission service.
#[derive(Debug, PartialEq, Eq, thiserror::Error)]
pub enum OwnershipError {
    /// No durable workflow state is available for ownership coordination.
    #[error("workspace ownership state is unavailable")]
    Unavailable,
    /// The operation requires the root actor.
    #[error("only the root agent may perform this ownership operation")]
    RootRequired,
    /// The actor belongs to a different workflow root.
    #[error("ownership actor is not the root of this agent tree")]
    WrongRoot,
    /// The resolved role is not allowed to mutate, even with a lease.
    #[error("read-only roles cannot mutate the workspace")]
    ReadOnlyRole,
    /// The actor has no covering active write lease.
    #[error("the actor does not hold an active write lease for {path}")]
    LeaseRequired { path: PathBuf },
    /// One or more active leases overlap the requested mutation.
    #[error("ownership lease conflicts with another agent (operationDigest={operation_digest})")]
    Conflict {
        conflicts: Vec<WorkflowLeaseConflict>,
        operation_digest: String,
        paths: Vec<WorkflowLeasePath>,
    },
    /// A caller requested an override where no conflict exists.
    #[error("root override is not needed for this path set")]
    OverrideNotNeeded,
    /// A subagent attempted to use root override authority.
    #[error("root override is restricted to the root agent")]
    OverrideRootOnly,
    /// The supplied override proof is stale or mismatched.
    #[error("root override proof does not match this operation")]
    OverrideMismatch,
    /// The canonical receipt sink failed before state-token issuance.
    #[error("ownership receipt could not be persisted before the override: {message}")]
    Receipt { message: String },
    /// The workflow store rejected or could not complete an operation.
    #[error("ownership state error: {message}")]
    State { message: String },
    /// Filesystem normalization or revalidation failed.
    #[error("ownership path error: {0}")]
    Path(#[from] super::OwnershipPathError),
    /// A bounded ownership request was malformed.
    #[error("ownership request is invalid: {message}")]
    InvalidRequest { message: String },
}

/// Root-scoped service that binds filesystem paths to durable workflow leases.
#[derive(Clone)]
pub struct WorkspaceOwnershipService {
    pub(super) workflow: WorkflowStore,
    pub(super) root_run_id: ThreadId,
    pub(super) authorized_roots: AuthorizedWorkspaceRoots,
}

impl WorkspaceOwnershipService {
    pub fn new(
        workflow: WorkflowStore,
        root_run_id: ThreadId,
        authorized_roots: AuthorizedWorkspaceRoots,
    ) -> Self {
        Self {
            workflow,
            root_run_id,
            authorized_roots,
        }
    }

    pub fn root_run_id(&self) -> ThreadId {
        self.root_run_id
    }

    pub fn authorized_roots(&self) -> &AuthorizedWorkspaceRoots {
        &self.authorized_roots
    }

    /// Grant normalized paths to a target agent. Only the root identity may
    /// issue this operation; role capabilities are enforced at mutation time.
    pub async fn grant_agent_ownership(
        &self,
        request: OwnershipGrantRequest,
    ) -> Result<Vec<WorkflowPathLease>, OwnershipError> {
        self.require_root(request.requester)?;
        if request.mode == codex_state::WorkflowLeaseMode::Write
            && !request.target.capabilities().may_request_workspace_lease()
        {
            return Err(OwnershipError::ReadOnlyRole);
        }
        let (_, paths) = self.normalize_paths(&request.paths)?;
        let lease_duration_ms = duration_millis(request.lease_duration)?;
        let state_request = WorkflowLeaseAcquireRequest {
            root_run_id: self.root_run_id.to_string(),
            owner_run_id: request.target.run_id().to_string(),
            environment_id: request.environment.as_option(),
            paths,
            mode: request.mode,
            lease_duration_ms,
            authority: WorkflowLeaseAuthority::Owner,
        };
        self.workflow
            .acquire_path_leases(&state_request)
            .await
            .map_err(map_state_error)
    }

    /// Release a lease using its exact token/generation fence. Owners may
    /// release their own leases; the root may release any lease in its tree.
    pub async fn release_agent_ownership(
        &self,
        request: OwnershipReleaseRequest,
    ) -> Result<WorkflowPathLease, OwnershipError> {
        let Some(lease) = self
            .workflow
            .get_path_lease(&request.lease_id)
            .await
            .map_err(map_state_error)?
        else {
            return Err(OwnershipError::State {
                message: "ownership lease was not found".to_string(),
            });
        };
        if lease.root_run_id != self.root_run_id.to_string() {
            return Err(OwnershipError::WrongRoot);
        }
        let may_release = match request.requester.authority() {
            OwnershipAuthority::Root => request.requester.run_id() == self.root_run_id,
            OwnershipAuthority::Subagent => {
                request.requester.run_id().to_string() == lease.owner_run_id
            }
        };
        if !may_release {
            return Err(OwnershipError::RootRequired);
        }
        self.workflow
            .release_path_lease(&WorkflowLeaseReleaseRequest {
                lease_id: request.lease_id,
                token: request.token,
                generation: request.generation,
            })
            .await
            .map_err(map_state_error)
    }

    /// List all durable leases in this root's tree in state-defined order.
    pub async fn list_agent_ownership(
        &self,
        requester: OwnershipActor,
    ) -> Result<Vec<WorkflowPathLease>, OwnershipError> {
        self.require_root(requester)?;
        self.workflow
            .list_path_leases(&self.root_run_id.to_string())
            .await
            .map_err(map_state_error)
    }

    /// Read one lease after checking that the caller is its owner or root.
    pub async fn read_agent_ownership(
        &self,
        requester: OwnershipActor,
        lease_id: &str,
    ) -> Result<Option<WorkflowPathLease>, OwnershipError> {
        let Some(lease) = self
            .workflow
            .get_path_lease(lease_id)
            .await
            .map_err(map_state_error)?
        else {
            return Ok(None);
        };
        if lease.root_run_id != self.root_run_id.to_string() {
            return Err(OwnershipError::WrongRoot);
        }
        let authorized = match requester.authority() {
            OwnershipAuthority::Root => requester.run_id() == self.root_run_id,
            OwnershipAuthority::Subagent => requester.run_id().to_string() == lease.owner_run_id,
        };
        if !authorized {
            return Err(OwnershipError::RootRequired);
        }
        Ok(Some(lease))
    }

    pub(super) fn require_root(&self, actor: OwnershipActor) -> Result<(), OwnershipError> {
        if actor.authority() != OwnershipAuthority::Root {
            return Err(OwnershipError::RootRequired);
        }
        if actor.run_id() != self.root_run_id {
            return Err(OwnershipError::WrongRoot);
        }
        Ok(())
    }

    pub(super) fn normalize_paths(
        &self,
        paths: &[PathBuf],
    ) -> Result<(Vec<NormalizedLeasePath>, Vec<WorkflowLeasePath>), OwnershipError> {
        if paths.is_empty() {
            return Err(OwnershipError::InvalidRequest {
                message: "at least one path is required".to_string(),
            });
        }
        if paths.len() > 128 {
            return Err(OwnershipError::InvalidRequest {
                message: "at most 128 paths may be admitted at once".to_string(),
            });
        }
        let normalized = paths
            .iter()
            .map(|path| self.authorized_roots.normalize(path))
            .collect::<Result<Vec<_>, _>>()?;
        let mut normalized_pairs = normalized
            .iter()
            .map(state_path)
            .collect::<Result<Vec<_>, _>>()?
            .into_iter()
            .zip(normalized)
            .map(|(state_path, normalized)| (normalized, state_path))
            .collect::<Vec<_>>();
        normalized_pairs.sort_by(|(_, left), (_, right)| {
            left.comparison_key
                .cmp(&right.comparison_key)
                .then_with(|| left.display.cmp(&right.display))
        });
        normalized_pairs
            .dedup_by(|(_, left), (_, right)| left.comparison_key == right.comparison_key);
        let (normalized, state_paths) = normalized_pairs.into_iter().unzip();
        Ok((normalized, state_paths))
    }
}
