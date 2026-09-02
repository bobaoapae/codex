use super::super::service_helpers::lease_environment_matches;
use super::ClaudeProviderAccess;
use super::ProviderMutationGuard;
use super::ProviderMutationScope;
use super::WorkspaceOwnershipService;
use super::extract_mcp_paths;
use super::is_mutating_claude_tool;
use crate::ownership::AuthorizedWorkspaceRoots;
use crate::ownership::MutationAuthorizationRequest;
use crate::ownership::MutationOperation;
use crate::ownership::OwnershipActor;
use crate::ownership::OwnershipEnvironment;
use crate::ownership::OwnershipGrantRequest;
use crate::ownership::OwnershipOverrideAuthorization;
use codex_exec_server::LOCAL_ENVIRONMENT_ID;
use codex_protocol::ThreadId;
use codex_state::SqliteConfig;
use codex_state::WorkflowLeaseMode;
use codex_state::WorkflowStore;
use codex_utils_absolute_path::AbsolutePathBuf;
use pretty_assertions::assert_eq;
use serde_json::json;
use std::sync::Arc;
use std::time::Duration;

#[test]
fn provider_path_extraction_only_keeps_static_path_values() {
    let mut paths = extract_mcp_paths(&json!({
        "path": "src/lib.rs",
        "paths": ["README.md", "$DYNAMIC"],
        "nested": {"destination": "build/out"}
    }));
    paths.sort();
    let mut expected = vec![
        std::path::PathBuf::from("src/lib.rs"),
        std::path::PathBuf::from("README.md"),
        std::path::PathBuf::from("build/out"),
    ];
    expected.sort();
    assert_eq!(paths, expected);
}

#[test]
fn provider_lease_environment_defaults_to_local_and_rejects_mismatch() {
    assert!(lease_environment_matches(None, "local"));
    assert!(lease_environment_matches(Some("local"), "local"));
    assert!(!lease_environment_matches(Some("remote"), "local"));
}

#[test]
fn claude_mutation_tool_allowlist_excludes_process_observers() {
    assert!(is_mutating_claude_tool("Bash"));
    assert!(is_mutating_claude_tool("Write"));
    assert!(is_mutating_claude_tool("mcp__server__tool"));
    assert!(!is_mutating_claude_tool("BashOutput"));
    assert!(!is_mutating_claude_tool("Read"));
}

#[tokio::test]
async fn fork_invariant_read_only_provider_denies_mutating_tools() {
    let cwd = AbsolutePathBuf::try_from(std::env::current_dir().expect("current directory"))
        .expect("current directory is absolute");
    let access = ClaudeProviderAccess::ReadOnly { notice: None };
    assert!(
        access
            .authorize_claude_tool("Bash", &json!({"command": "touch output.txt"}), &cwd)
            .await
            .is_err()
    );
    assert_eq!(
        access
            .authorize_claude_tool("Read", &json!({"file_path": "README.md"}), &cwd)
            .await,
        Ok(())
    );
}

#[tokio::test]
async fn never_policy_writable_editor_still_guards_destructive_git() {
    let home = tempfile::tempdir().expect("temporary home");
    let root = home.path().join("workspace");
    let state = home.path().join("state");
    std::fs::create_dir_all(&root).expect("workspace root");
    std::fs::create_dir_all(&state).expect("state directory");
    let workflow = WorkflowStore::open(&SqliteConfig::new_for_testing(
        AbsolutePathBuf::from_absolute_path(&state).expect("absolute state home"),
    ))
    .await
    .expect("workflow store");
    let root_run_id = ThreadId::new();
    let service = Arc::new(WorkspaceOwnershipService::new(
        workflow,
        root_run_id,
        AuthorizedWorkspaceRoots::new([root.clone()]).expect("authorized root"),
    ));
    let actor = OwnershipActor::subagent_for_role(ThreadId::new(), Some("executor_luna"));
    service
        .grant_agent_ownership(OwnershipGrantRequest {
            requester: OwnershipActor::root(root_run_id),
            target: actor,
            paths: vec![root.clone()],
            mode: WorkflowLeaseMode::Write,
            lease_duration: Duration::from_secs(60),
            environment: OwnershipEnvironment::Default,
        })
        .await
        .expect("write lease");
    let guard = service
        .authorize_mutation(MutationAuthorizationRequest {
            actor,
            paths: vec![root.clone()],
            operation: MutationOperation {
                digest: "test-operation".to_string(),
            },
            override_authorization: OwnershipOverrideAuthorization::NotRequested,
        })
        .await
        .expect("lease admission");
    let access = ClaudeProviderAccess::Mutable(ProviderMutationGuard {
        service,
        guard,
        scope: ProviderMutationScope::FullCheckout,
        environment_id: LOCAL_ENVIRONMENT_ID.to_string(),
        _lease_hold: None,
    });
    let cwd = AbsolutePathBuf::try_from(root).expect("absolute workspace");

    // `Never` is represented by the forced `auto` mode at the Claude command
    // boundary; the provider guard still runs before the host auto-approves.
    assert!(access.requires_tool_authorization());
    for command in ["git reset --hard", "git clean -fd"] {
        let error = access
            .authorize_claude_tool("Bash", &json!({ "command": command }), &cwd)
            .await
            .expect_err("destructive Git must be denied for a subagent");
        assert!(error.contains("destructive Git"), "{error}");
    }
}

/// FORK: the second half of the kill-switch invariant, on the Claude side.
///
/// Turning lease coordination off must not turn a subagent loose on the shared
/// working tree's history.
#[tokio::test]
async fn fork_invariant_unmanaged_provider_still_denies_destructive_git() {
    let cwd = AbsolutePathBuf::try_from(std::env::current_dir().expect("current directory"))
        .expect("current directory is absolute");
    let access = ClaudeProviderAccess::Unmanaged;
    // The host still sees every tool call; `bypassPermissions` would skip it.
    assert!(access.requires_tool_authorization());
    for command in ["git reset --hard", "git clean -fd"] {
        let error = access
            .authorize_claude_tool("Bash", &json!({ "command": command }), &cwd)
            .await
            .expect_err("destructive Git must be denied with enforcement disabled");
        assert!(error.contains("destructive Git"), "{error}");
    }
    assert_eq!(
        access
            .authorize_claude_tool("Write", &json!({"file_path": "notes.md"}), &cwd)
            .await,
        Ok(()),
        "ordinary writes are exactly what disabling enforcement restores"
    );
}

/// FORK: a missing lease pauses writing; it never kills the turn.
///
/// This runs on every sampling request, so a hard error here ended the whole
/// turn with "This agent's turn failed" and nothing else. Degrading instead
/// lets the agent report, and the next request re-runs this check.
#[test]
fn a_degraded_provider_turn_is_read_only_and_says_why() {
    let access = super::degraded_access("another agent is writing C:/repo/src".to_string());
    assert!(access.is_read_only());
    let notice = access
        .ownership_notice()
        .expect("a degraded turn must explain itself");
    assert!(notice.contains("another agent is writing"), "{notice}");
    assert!(
        notice.contains("retries automatically"),
        "the agent must be told the pause is temporary: {notice}"
    );
    // A role that simply cannot write carries no notice to explain away.
    assert_eq!(
        ClaudeProviderAccess::ReadOnly { notice: None }.ownership_notice(),
        None
    );
}
