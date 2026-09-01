use super::*;
use crate::agent::AgentStatus;
use crate::agent::control::SpawnAgentOptions;
use crate::config::Config;
use crate::config::ConfigBuilder;
use crate::init_state_db;
use crate::thread_manager::StartThreadOptions;
use crate::thread_manager::ThreadManager;
use codex_agent_graph_store::ThreadSpawnEdgeStatus;
use codex_exec_server::EnvironmentManager;
use codex_login::CodexAuth;
use codex_protocol::protocol::SessionSource;
use codex_protocol::protocol::SubAgentSource;
use codex_protocol::user_input::UserInput;
use codex_state::SqliteConfig;
use codex_state::WorkflowStore;
use codex_utils_absolute_path::test_support::PathExt;
use pretty_assertions::assert_eq;
use tempfile::TempDir;

#[test]
fn fleet_agent_status_mapping_is_explicit_and_terminal_safe() {
    assert_eq!(
        fleet_agent_state(AgentStatus::PendingInit),
        FleetAgentState::WaitingForUser
    );
    assert_eq!(
        fleet_agent_state(AgentStatus::Running),
        FleetAgentState::Running
    );
    assert_eq!(
        fleet_agent_state(AgentStatus::Completed(Some("done".to_string()))),
        FleetAgentState::Idle
    );
    assert_eq!(
        fleet_agent_state(AgentStatus::Errored("failed".to_string())),
        FleetAgentState::Failed
    );
    assert_eq!(
        fleet_agent_state(AgentStatus::Interrupted),
        FleetAgentState::Suspended
    );
    assert_eq!(
        fleet_agent_state(AgentStatus::Shutdown),
        FleetAgentState::Closed
    );
    assert!(is_close_ready(AgentStatus::Completed(Some(
        "done".to_string()
    ))));
    assert!(is_close_ready(AgentStatus::Errored("failed".to_string())));
    assert!(!is_close_ready(AgentStatus::Running));
}

#[test]
fn unloaded_member_status_uses_root_and_edge_lifecycle() {
    assert_eq!(
        fleet_member_state(
            FleetRootState::Suspended,
            Some(ThreadSpawnEdgeStatus::Open),
            AgentStatus::NotFound,
        ),
        FleetAgentState::Suspended
    );
    assert_eq!(
        fleet_member_state(
            FleetRootState::Active,
            Some(ThreadSpawnEdgeStatus::Open),
            AgentStatus::NotFound,
        ),
        FleetAgentState::Idle
    );
    assert_eq!(
        fleet_member_state(
            FleetRootState::Active,
            Some(ThreadSpawnEdgeStatus::Closed),
            AgentStatus::NotFound,
        ),
        FleetAgentState::Closed
    );
    assert_eq!(
        fleet_member_state(FleetRootState::Closed, None, AgentStatus::NotFound),
        FleetAgentState::Closed
    );
}

#[tokio::test]
async fn sealed_root_rejects_spawn_admission() {
    let home = TempDir::new().expect("temporary codex home");
    let store = WorkflowStore::open(&SqliteConfig::new_for_testing(home.path().abs()))
        .await
        .expect("workflow store");
    let control = AgentControl::default();
    let root = ThreadId::new();
    control.register_session_root(root, None);
    assert!(control.workflow_store.set(store.clone()).is_ok());

    control
        .ensure_fleet_data_admission(root)
        .await
        .expect("unsealed root admits data and children");
    store
        .seal_fleet_admissions(&root.to_string(), 0)
        .await
        .expect("seal admissions");
    assert!(control.ensure_fleet_data_admission(root).await.is_err());
    store.close().await;
}
#[test]
fn fleet_root_validation_rejects_unregistered_or_non_root_targets() {
    let control = AgentControl::default();
    let root = ThreadId::new();
    assert!(control.validate_fleet_root(root).is_err());
    control.register_session_root(root, None);
    assert!(control.validate_fleet_root(root).is_ok());
    assert!(control.validate_fleet_root(ThreadId::new()).is_err());
}

async fn fleet_test_config(home: &TempDir) -> Config {
    ConfigBuilder::without_managed_config_for_tests()
        .codex_home(home.path().to_path_buf())
        .cli_overrides(vec![(
            "model".to_string(),
            toml::Value::String("gpt-5.5".to_string()),
        )])
        .build()
        .await
        .expect("fleet test config")
}

fn text_input(text: &str) -> Vec<UserInput> {
    vec![UserInput::Text {
        text: text.to_string(),
        text_elements: Vec::new(),
    }]
}

#[tokio::test]
async fn cold_resume_uses_explicit_config_and_restores_root_before_open_children() {
    let home = TempDir::new().expect("temporary codex home");
    let config = fleet_test_config(&home).await;
    let state_db = init_state_db(&config).await.expect("state db");
    let manager = ThreadManager::with_models_provider_home_and_state_for_tests(
        CodexAuth::from_api_key("dummy"),
        config.model_provider.clone(),
        config.codex_home.to_path_buf(),
        std::sync::Arc::new(EnvironmentManager::default_for_tests()),
        Some(state_db.clone()),
    );
    let control = manager.agent_control();
    let root_thread = manager
        .start_thread(StartThreadOptions::new(config.clone()))
        .await
        .expect("root thread");
    let root_id = root_thread.thread_id;
    control.register_session_root(root_id, None);
    let child = control
        .spawn_agent_with_metadata(
            config.clone(),
            text_input("child task"),
            Some(SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
                parent_thread_id: root_id,
                depth: 1,
                agent_path: None,
                agent_nickname: None,
                agent_role: None,
            })),
            SpawnAgentOptions::default(),
        )
        .await
        .expect("child thread");
    let child_id = child.thread_id;
    let edges = state_db
        .list_thread_spawn_edge_records(
            root_id,
            Some(codex_state::DirectionalThreadSpawnEdgeStatus::Open),
        )
        .await
        .expect("open child edge");
    assert_eq!(edges.len(), 1);
    let suspended = control
        .suspend_fleet(root_id, 0)
        .await
        .expect("suspend fleet");
    assert_eq!(
        suspended.operation.status,
        codex_state::FleetOperationStatus::Complete
    );
    assert_eq!(
        state_db
            .workflow_store()
            .get_fleet_state(&root_id.to_string())
            .await
            .unwrap()
            .unwrap()
            .state,
        codex_state::FleetRootState::Suspended
    );
    drop(root_thread);
    drop(control);
    drop(manager);

    let cold_manager = ThreadManager::with_models_provider_home_and_state_for_tests(
        CodexAuth::from_api_key("dummy"),
        config.model_provider.clone(),
        config.codex_home.to_path_buf(),
        std::sync::Arc::new(EnvironmentManager::default_for_tests()),
        Some(state_db.clone()),
    );
    let generation = state_db
        .workflow_store()
        .get_fleet_state(&root_id.to_string())
        .await
        .unwrap()
        .unwrap()
        .generation;
    let resumed = cold_manager
        .resume_fleet(root_id, generation, config.clone())
        .await
        .expect("cold resume with explicit config");
    assert_eq!(resumed.status, "complete");
    assert!(cold_manager.get_thread(root_id).await.is_ok());
    assert!(cold_manager.get_thread(child_id).await.is_ok());
    let root = state_db
        .workflow_store()
        .get_fleet_state(&root_id.to_string())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(root.state, codex_state::FleetRootState::Active);
    assert!(!root.admissions_sealed);
    cold_manager
        .shutdown_all_threads_bounded(std::time::Duration::from_secs(5))
        .await;
}
