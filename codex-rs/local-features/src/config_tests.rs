use super::*;

#[test]
fn public_config_resolves_all_local_modes() {
    let config = public::LocalExtensionsToml {
        operations_dock: Some(public::OperationsDockModeToml::Auto),
        mouse: Some(public::MouseModeToml::Dock),
        resume: Some(public::ResumeModeToml::Checkpointed),
        context: Some(public::ContextModeToml::Adaptive),
        evidence_cache: Some(public::EvidenceCacheModeToml::ReadOnly),
        agent_admission: Some(public::AgentAdmissionModeToml::Adaptive),
        agent_delegation: Some(public::AgentDelegationModeToml::Proactive),
        goal_supervision: Some(public::GoalSupervisionModeToml::InProcess),
    };

    assert_eq!(
        LocalExtensionsConfig::from(&config),
        LocalExtensionsConfig {
            operations_dock: OperationsDockMode::Auto,
            mouse: MouseMode::Dock,
            resume: ResumeMode::Checkpointed,
            context: ContextMode::Adaptive,
            evidence_cache: EvidenceCacheMode::ReadOnly,
            agent_admission: AgentAdmissionMode::Adaptive,
            agent_delegation: AgentDelegationMode::Proactive,
            goal_supervision: GoalSupervisionMode::InProcess,
        }
    );
}
