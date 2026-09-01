mod agent_role_config;
mod capabilities;
mod discovery;
mod loader;

pub use agent_role_config::AgentRoleConfig;
pub use agent_role_config::ResolvedAgentRoleFile;
pub use agent_role_config::parse_agent_role_file_contents;
pub use capabilities::AgentMutationCapability;
pub use capabilities::AgentRoleCapabilities;
pub use capabilities::capabilities_for_canonical_role;
pub use capabilities::capabilities_for_role;
pub use loader::load_agent_roles;
