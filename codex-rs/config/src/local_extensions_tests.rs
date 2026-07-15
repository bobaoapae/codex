use super::*;

#[test]
fn local_extension_modes_deserialize_from_documented_values() {
    let config: LocalExtensionsToml = toml::from_str(
        r#"
operations_dock = "auto"
mouse = "dock"
resume = "checkpointed"
context = "adaptive"
evidence_cache = "read_only"
agent_admission = "adaptive"
agent_delegation = "explicit_request_only"
goal_supervision = "in_process"
"#,
    )
    .expect("documented local extension config should parse");

    assert_eq!(
        config,
        LocalExtensionsToml {
            operations_dock: Some(OperationsDockModeToml::Auto),
            mouse: Some(MouseModeToml::Dock),
            resume: Some(ResumeModeToml::Checkpointed),
            context: Some(ContextModeToml::Adaptive),
            evidence_cache: Some(EvidenceCacheModeToml::ReadOnly),
            agent_admission: Some(AgentAdmissionModeToml::Adaptive),
            agent_delegation: Some(AgentDelegationModeToml::ExplicitRequestOnly),
            goal_supervision: Some(GoalSupervisionModeToml::InProcess),
        }
    );
}
