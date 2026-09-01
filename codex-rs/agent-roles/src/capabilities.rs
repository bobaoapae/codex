//! Typed mutation capabilities for canonical agent roles.
//!
//! Role names are user-facing strings and are therefore not an authority
//! boundary by themselves. This module provides the closed mapping used by
//! core: only the canonical executor/editor roles may request a workspace
//! lease, and even those roles remain non-mutating until a lease is granted by
//! the ownership layer.

/// Mutation authority requested by an agent role.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentMutationCapability {
    /// The role cannot request a workspace lease or mutate files.
    ReadOnly,
    /// The role may request a workspace lease; it does not grant write access.
    RequiresWorkspaceLease,
}

/// Capabilities resolved from a canonical role name.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AgentRoleCapabilities {
    /// Mutation capability requested by the role.
    pub mutation: AgentMutationCapability,
}

impl AgentRoleCapabilities {
    /// Construct a read-only capability set.
    pub const fn read_only() -> Self {
        Self {
            mutation: AgentMutationCapability::ReadOnly,
        }
    }

    /// Construct a capability set that may request a workspace lease.
    pub const fn requires_workspace_lease() -> Self {
        Self {
            mutation: AgentMutationCapability::RequiresWorkspaceLease,
        }
    }

    /// Capabilities for a canonical role. Unknown names fail closed.
    pub fn for_canonical_role(role_name: &str) -> Self {
        let mutation = match role_name {
            "executor_luna" | "executor_sonnet" | "claude-opus" | "doc-writer" | "worker"
            | "luna" => AgentMutationCapability::RequiresWorkspaceLease,
            _ => AgentMutationCapability::ReadOnly,
        };
        Self { mutation }
    }

    /// Resolve an optional runtime role name, treating omitted/default and
    /// custom names as untrusted read-only roles.
    pub fn for_role(role_name: Option<&str>) -> Self {
        match role_name {
            Some(role_name) => Self::for_canonical_role(role_name),
            None => Self::read_only(),
        }
    }

    /// Whether the role is allowed to ask the ownership layer for a lease.
    pub const fn may_request_workspace_lease(self) -> bool {
        matches!(
            self.mutation,
            AgentMutationCapability::RequiresWorkspaceLease
        )
    }

    /// Whether mutation is effective after applying the lease state.
    ///
    /// A role capability never grants mutation on its own. Read-only roles
    /// remain read-only even if a caller accidentally reports a lease.
    pub const fn effective_mutation_allowed(self, workspace_lease_held: bool) -> bool {
        self.may_request_workspace_lease() && workspace_lease_held
    }
}

impl Default for AgentRoleCapabilities {
    fn default() -> Self {
        Self::read_only()
    }
}

/// Resolve mutation capabilities for a canonical role name.
pub fn capabilities_for_canonical_role(role_name: &str) -> AgentRoleCapabilities {
    AgentRoleCapabilities::for_canonical_role(role_name)
}

/// Resolve mutation capabilities from an optional runtime role name.
pub fn capabilities_for_role(role_name: Option<&str>) -> AgentRoleCapabilities {
    AgentRoleCapabilities::for_role(role_name)
}

#[cfg(test)]
#[path = "capabilities_tests.rs"]
mod tests;
