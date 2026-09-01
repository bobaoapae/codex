use super::NormalizedLeasePath;
use codex_agent_roles::AgentRoleCapabilities;
use codex_agent_roles::capabilities_for_role;
use codex_protocol::ThreadId;
use codex_state::WorkflowLeaseMode;
use codex_state::WorkflowLeaseOverrideUse;
use codex_state::WorkflowPathLease;
use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

/// Authority used by ownership operations. A subagent's role capability is
/// retained separately so a lease never becomes an implicit write grant.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OwnershipAuthority {
    /// Root agent of the workflow tree.
    Root,
    /// Descendant agent whose role capability is checked.
    Subagent,
}

/// Runtime identity presented to the ownership service.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct OwnershipActor {
    run_id: ThreadId,
    authority: OwnershipAuthority,
    capabilities: AgentRoleCapabilities,
}

impl OwnershipActor {
    /// Construct the root actor for a workflow tree.
    pub fn root(run_id: ThreadId) -> Self {
        Self {
            run_id,
            authority: OwnershipAuthority::Root,
            capabilities: AgentRoleCapabilities::read_only(),
        }
    }

    /// Construct a subagent actor with an already resolved capability set.
    pub(crate) fn subagent(run_id: ThreadId, capabilities: AgentRoleCapabilities) -> Self {
        Self {
            run_id,
            authority: OwnershipAuthority::Subagent,
            capabilities,
        }
    }

    /// Construct a subagent identity from the closed role resolver.
    /// Construct a subagent actor from the closed role resolver.
    pub fn subagent_for_role(run_id: ThreadId, role_name: Option<&str>) -> Self {
        Self::subagent(run_id, capabilities_for_role(role_name))
    }

    /// Return the thread/run identity of this actor.
    pub fn run_id(self) -> ThreadId {
        self.run_id
    }

    /// Return whether this actor is the workflow root or a descendant.
    pub fn authority(self) -> OwnershipAuthority {
        self.authority
    }

    /// Return the typed role capability carried by this actor.
    pub fn capabilities(self) -> AgentRoleCapabilities {
        self.capabilities
    }
}

/// Environment binding attached to a path lease.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum OwnershipEnvironment {
    /// Use the default environment for the workspace.
    Default,
    /// Bind the lease to a named environment.
    Named(String),
}

impl OwnershipEnvironment {
    /// Convert the named binding to the state-store representation.
    pub fn as_option(&self) -> Option<String> {
        match self {
            Self::Default => None,
            Self::Named(environment_id) => Some(environment_id.clone()),
        }
    }
}

/// Root request to grant one or more paths to a target agent.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OwnershipGrantRequest {
    /// Root actor authorizing the grant.
    pub requester: OwnershipActor,
    /// Agent that will own the resulting leases.
    pub target: OwnershipActor,
    /// Absolute paths to normalize and lease atomically.
    pub paths: Vec<PathBuf>,
    /// Read or write access requested for every path.
    pub mode: WorkflowLeaseMode,
    /// Bounded lifetime of the new leases.
    pub lease_duration: Duration,
    /// Environment binding for the lease.
    pub environment: OwnershipEnvironment,
}

/// Owner or root request to release one fenced path lease.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OwnershipReleaseRequest {
    /// Owner or root actor requesting release.
    pub requester: OwnershipActor,
    /// Opaque lease identifier.
    pub lease_id: String,
    /// Current fencing token.
    pub token: String,
    /// Current fencing generation.
    pub generation: i64,
}

/// Bounded identity of a mutation operation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MutationOperation {
    /// Bounded caller-computed identity of the mutation.
    pub digest: String,
}

/// Optional authorization path for a root conflict override.
pub enum OwnershipOverrideAuthorization {
    /// No override was requested.
    NotRequested,
    /// Request a fresh receipt-backed one-shot override.
    Request(OwnershipOverrideRequest),
    /// Consume a previously issued exact proof.
    Use(WorkflowLeaseOverrideUse),
}

/// Request for a one-shot root override, including the canonical receipt sink.
pub struct OwnershipOverrideRequest {
    /// Human-readable reason retained in workflow state.
    pub reason: String,
    /// Canonical sink that must succeed before issuing the state token.
    pub receipt_sink: Arc<dyn OwnershipReceiptSink>,
}

/// Mutation admission request. Paths are normalized and revalidated by the
/// service before any authorization decision is returned.
pub struct MutationAuthorizationRequest {
    /// Runtime actor requesting mutation.
    pub actor: OwnershipActor,
    /// Absolute paths to normalize and revalidate.
    pub paths: Vec<PathBuf>,
    /// Bounded identity of the mutation operation.
    pub operation: MutationOperation,
    /// Optional root-only override proof or request.
    pub override_authorization: OwnershipOverrideAuthorization,
}

/// Safe metadata passed to the canonical root receipt before an override token
/// is issued. Raw path text and mutation content are never included in the
/// receipt metadata; paths are represented by bounded canonical state values.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OwnershipOverrideReceipt {
    /// Receipt identity shared with the workflow override row.
    pub receipt_id: String,
    /// Root thread that owns the override.
    pub root_run_id: ThreadId,
    /// Canonical lease paths involved in the override.
    pub paths: Vec<codex_state::WorkflowLeasePath>,
    /// Conflicting lease owners.
    pub conflict_owner_run_ids: Vec<String>,
    /// Exact mutation identity.
    pub operation_digest: String,
    /// Root-provided reason retained in state, not receipt metadata.
    pub reason: String,
}

/// Host-side sink used to append an ownership override receipt before state
/// issues a one-shot override token.
pub trait OwnershipReceiptSink: Send + Sync {
    /// Append a redacted canonical receipt before issuing an override token.
    fn append_ownership_override_receipt(
        &self,
        receipt: OwnershipOverrideReceipt,
    ) -> Pin<Box<dyn Future<Output = anyhow::Result<()>> + Send + '_>>;
}

/// Lease and path identity retained for the duration of an admitted mutation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MutationGuard {
    actor_run_id: ThreadId,
    operation_digest: String,
    paths: Vec<NormalizedLeasePath>,
    leases: Vec<WorkflowPathLease>,
}

impl MutationGuard {
    pub(crate) fn new(
        actor_run_id: ThreadId,
        operation_digest: String,
        paths: Vec<NormalizedLeasePath>,
        leases: Vec<WorkflowPathLease>,
    ) -> Self {
        Self {
            actor_run_id,
            operation_digest,
            paths,
            leases,
        }
    }

    /// Return the actor identity admitted by this guard.
    pub fn actor_run_id(&self) -> ThreadId {
        self.actor_run_id
    }

    /// Return the operation digest bound to this guard.
    pub fn operation_digest(&self) -> &str {
        &self.operation_digest
    }

    /// Return normalized, revalidation-capable paths held by this guard.
    pub fn paths(&self) -> &[NormalizedLeasePath] {
        &self.paths
    }

    /// Return durable lease fences retained by this guard.
    pub fn leases(&self) -> &[WorkflowPathLease] {
        &self.leases
    }

    /// Revalidate every path immediately before the guarded mutation starts.
    pub fn revalidate(&self) -> Result<(), super::OwnershipPathError> {
        for path in &self.paths {
            path.revalidate_before_mutation()?;
        }
        Ok(())
    }
}
