//! Fail-closed workspace path normalization primitives.

use crate::config::Config;

mod ensure;
mod lease_coordinator;
mod path;
mod provider;
mod service;
mod service_admission;
mod service_helpers;
mod service_types;

pub(crate) use ensure::EnsureLeaseRequest;
pub(crate) use ensure::EnsuredLeases;
pub(crate) use ensure::ensure_subagent_write_leases;
pub(crate) use lease_coordinator::LeaseCoordinator;
pub(crate) use lease_coordinator::LeaseHold;
pub use path::AuthorizedWorkspaceRoots;
pub use path::NormalizedLeasePath;
pub use path::OwnershipPathError;
pub(crate) use provider::ClaudeProviderAccess;
pub(crate) use provider::authorize_claude_provider;
pub(crate) use provider::authorize_mcp_mutation;
pub use service::OwnershipError;
pub use service::WorkspaceOwnershipService;
pub(crate) use service_helpers::describe_ownership_error;
pub(crate) use service_helpers::ownership_state_is_absent;
pub use service_types::MutationAuthorizationRequest;
pub use service_types::MutationGuard;
pub use service_types::MutationOperation;
pub use service_types::OwnershipActor;
pub use service_types::OwnershipAuthority;
pub use service_types::OwnershipEnvironment;
pub use service_types::OwnershipGrantRequest;
pub use service_types::OwnershipOverrideAuthorization;
pub use service_types::OwnershipOverrideReceipt;
pub use service_types::OwnershipOverrideRequest;
pub use service_types::OwnershipReceiptSink;
pub use service_types::OwnershipReleaseRequest;

/// FORK: the roots ownership admits for a config, plus the ones that need no
/// lease.
///
/// `<codex_home>/visualizations` is always admissible, even when the Desktop
/// did not hand this turn a scratch root under it: `AuthorizedWorkspaceRoots`
/// is only consulted by `normalize_paths`, so listing it grants admission to
/// ownership, never write permission in the sandbox — that is decided by
/// `effective_workspace_roots`, which is left alone.
pub fn authorized_roots_for_config(
    config: &Config,
) -> Result<AuthorizedWorkspaceRoots, OwnershipPathError> {
    let visualizations = config.visualizations_dir();
    // Created lazily by the Desktop; `new` rejects a root that does not exist.
    let _ = std::fs::create_dir_all(visualizations.as_path());
    let mut roots = config.effective_workspace_roots();
    if visualizations.as_path().is_dir() && !roots.contains(&visualizations) {
        roots.push(visualizations);
    }
    Ok(AuthorizedWorkspaceRoots::new(roots)?
        .with_lease_exempt_roots(config.lease_exempt_workspace_roots()))
}

#[cfg(test)]
#[path = "tests.rs"]
mod tests;

#[cfg(test)]
#[path = "service_tests.rs"]
mod service_tests;

#[cfg(test)]
#[path = "ensure_tests.rs"]
mod ensure_tests;
