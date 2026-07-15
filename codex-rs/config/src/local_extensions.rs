use schemars::JsonSchema;
use serde::Deserialize;
use serde::Serialize;

macro_rules! mode_enum {
    ($name:ident { $($variant:ident),+ $(,)? }) => {
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
        #[serde(rename_all = "snake_case")]
        pub enum $name {
            $($variant),+
        }
    };
}

mode_enum!(OperationsDockModeToml {
    Hidden,
    Auto,
    Always
});
mode_enum!(MouseModeToml { Off, Dock });
mode_enum!(ResumeModeToml {
    Legacy,
    Checkpointed
});
mode_enum!(ContextModeToml { Fixed, Adaptive });
mode_enum!(EvidenceCacheModeToml { Off, ReadOnly });
mode_enum!(AgentAdmissionModeToml { Direct, Adaptive });
mode_enum!(AgentDelegationModeToml {
    ExplicitRequestOnly,
    Proactive
});
mode_enum!(GoalSupervisionModeToml { Off, InProcess });

/// Opt-in settings for capabilities maintained only by the local fork.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[schemars(deny_unknown_fields)]
pub struct LocalExtensionsToml {
    pub operations_dock: Option<OperationsDockModeToml>,
    pub mouse: Option<MouseModeToml>,
    pub resume: Option<ResumeModeToml>,
    pub context: Option<ContextModeToml>,
    pub evidence_cache: Option<EvidenceCacheModeToml>,
    pub agent_admission: Option<AgentAdmissionModeToml>,
    pub agent_delegation: Option<AgentDelegationModeToml>,
    pub goal_supervision: Option<GoalSupervisionModeToml>,
}

#[cfg(test)]
#[path = "local_extensions_tests.rs"]
mod tests;
