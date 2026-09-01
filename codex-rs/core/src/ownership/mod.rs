//! Fail-closed workspace path normalization primitives.

mod path;
mod provider;
mod service;
mod service_admission;
mod service_helpers;
mod service_types;

pub use path::AuthorizedWorkspaceRoots;
pub use path::NormalizedLeasePath;
pub use path::OwnershipPathError;
pub(crate) use provider::ClaudeProviderAccess;
pub(crate) use provider::authorize_claude_provider;
pub(crate) use provider::authorize_mcp_mutation;
pub use service::OwnershipError;
pub use service::WorkspaceOwnershipService;
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
