//! Fail-closed workspace path normalization primitives.

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

#[cfg(test)]
#[path = "tests.rs"]
mod tests;

#[cfg(test)]
#[path = "service_tests.rs"]
mod service_tests;

#[cfg(test)]
#[path = "ensure_tests.rs"]
mod ensure_tests;
