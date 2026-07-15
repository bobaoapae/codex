use serde::Deserialize;
use serde::Serialize;

use codex_config::local_extensions as public;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OperationsDockMode {
    #[default]
    Hidden,
    Auto,
    Always,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MouseMode {
    #[default]
    Off,
    Dock,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ResumeMode {
    #[default]
    Legacy,
    Checkpointed,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ContextMode {
    #[default]
    Fixed,
    Adaptive,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EvidenceCacheMode {
    #[default]
    Off,
    ReadOnly,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentAdmissionMode {
    #[default]
    Direct,
    Adaptive,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentDelegationMode {
    #[default]
    ExplicitRequestOnly,
    Proactive,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GoalSupervisionMode {
    #[default]
    Off,
    InProcess,
}

/// Resolved local-extension settings used by the isolated policy crate.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct LocalExtensionsConfig {
    pub operations_dock: OperationsDockMode,
    pub mouse: MouseMode,
    pub resume: ResumeMode,
    pub context: ContextMode,
    pub evidence_cache: EvidenceCacheMode,
    pub agent_admission: AgentAdmissionMode,
    pub agent_delegation: AgentDelegationMode,
    pub goal_supervision: GoalSupervisionMode,
}

impl LocalExtensionsConfig {
    pub fn any_enabled(&self) -> bool {
        self.operations_dock != OperationsDockMode::Hidden
            || self.mouse != MouseMode::Off
            || self.resume != ResumeMode::Legacy
            || self.context != ContextMode::Fixed
            || self.evidence_cache != EvidenceCacheMode::Off
            || self.agent_admission != AgentAdmissionMode::Direct
            || self.agent_delegation != AgentDelegationMode::ExplicitRequestOnly
            || self.goal_supervision != GoalSupervisionMode::Off
    }
}

impl From<&public::LocalExtensionsToml> for LocalExtensionsConfig {
    fn from(value: &public::LocalExtensionsToml) -> Self {
        Self {
            operations_dock: match value.operations_dock {
                Some(public::OperationsDockModeToml::Auto) => OperationsDockMode::Auto,
                Some(public::OperationsDockModeToml::Always) => OperationsDockMode::Always,
                Some(public::OperationsDockModeToml::Hidden) | None => OperationsDockMode::Hidden,
            },
            mouse: match value.mouse {
                Some(public::MouseModeToml::Dock) => MouseMode::Dock,
                Some(public::MouseModeToml::Off) | None => MouseMode::Off,
            },
            resume: match value.resume {
                Some(public::ResumeModeToml::Checkpointed) => ResumeMode::Checkpointed,
                Some(public::ResumeModeToml::Legacy) | None => ResumeMode::Legacy,
            },
            context: match value.context {
                Some(public::ContextModeToml::Adaptive) => ContextMode::Adaptive,
                Some(public::ContextModeToml::Fixed) | None => ContextMode::Fixed,
            },
            evidence_cache: match value.evidence_cache {
                Some(public::EvidenceCacheModeToml::ReadOnly) => EvidenceCacheMode::ReadOnly,
                Some(public::EvidenceCacheModeToml::Off) | None => EvidenceCacheMode::Off,
            },
            agent_admission: match value.agent_admission {
                Some(public::AgentAdmissionModeToml::Adaptive) => AgentAdmissionMode::Adaptive,
                Some(public::AgentAdmissionModeToml::Direct) | None => AgentAdmissionMode::Direct,
            },
            agent_delegation: match value.agent_delegation {
                Some(public::AgentDelegationModeToml::Proactive) => AgentDelegationMode::Proactive,
                Some(public::AgentDelegationModeToml::ExplicitRequestOnly) | None => {
                    AgentDelegationMode::ExplicitRequestOnly
                }
            },
            goal_supervision: match value.goal_supervision {
                Some(public::GoalSupervisionModeToml::InProcess) => GoalSupervisionMode::InProcess,
                Some(public::GoalSupervisionModeToml::Off) | None => GoalSupervisionMode::Off,
            },
        }
    }
}

#[cfg(test)]
#[path = "config_tests.rs"]
mod tests;
