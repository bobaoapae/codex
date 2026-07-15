//! Isolated policy and rebuildable state for features maintained by the local fork.

mod config;
mod context;
mod goal;
mod store;

pub mod admission;
pub mod checkpoints;
pub mod evidence;
pub mod metrics;

pub use config::AgentAdmissionMode;
pub use config::AgentDelegationMode;
pub use config::ContextMode;
pub use config::EvidenceCacheMode;
pub use config::GoalSupervisionMode;
pub use config::LocalExtensionsConfig;
pub use config::MouseMode;
pub use config::OperationsDockMode;
pub use config::ResumeMode;
pub use context::AdaptiveContextPolicy;
pub use context::CompactionDecision;
pub use context::CompactionReason;
pub use goal::GoalErrorClass;
pub use goal::GoalSupervisorDecision;
pub use goal::GoalSupervisorState;
pub use store::LocalExtensionsStore;
