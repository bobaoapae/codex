use super::AgentMutationCapability;
use super::AgentRoleCapabilities;
use super::capabilities_for_canonical_role;
use super::capabilities_for_role;

#[test]
fn canonical_read_only_roles_fail_closed() {
    for role_name in ["explorer", "tester", "claude-fable", "chatgpt-pro"] {
        assert_eq!(
            capabilities_for_canonical_role(role_name),
            AgentRoleCapabilities {
                mutation: AgentMutationCapability::ReadOnly,
            }
        );
    }
}

#[test]
fn canonical_executor_roles_only_request_a_lease() {
    for role_name in [
        "executor_luna",
        "executor_sonnet",
        "claude-opus",
        "doc-writer",
        "worker",
        "luna",
    ] {
        let capabilities = capabilities_for_canonical_role(role_name);
        assert_eq!(
            capabilities,
            AgentRoleCapabilities {
                mutation: AgentMutationCapability::RequiresWorkspaceLease,
            }
        );
        assert!(!capabilities.effective_mutation_allowed(false));
        assert!(capabilities.effective_mutation_allowed(true));
    }
}

#[test]
fn unknown_custom_and_default_roles_are_read_only() {
    for role_name in [Some("default"), Some("custom"), Some("unknown"), None] {
        let capabilities = capabilities_for_role(role_name);
        assert_eq!(
            capabilities,
            AgentRoleCapabilities {
                mutation: AgentMutationCapability::ReadOnly,
            }
        );
        assert!(!capabilities.may_request_workspace_lease());
        assert!(!capabilities.effective_mutation_allowed(true));
    }
}
