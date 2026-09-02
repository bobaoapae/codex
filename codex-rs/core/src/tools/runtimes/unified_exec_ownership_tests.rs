use super::authorize_exec_command;
use super::operation_digest;
use super::resolve_command_path;
use crate::ownership::OwnershipOverrideAuthorization;
use codex_utils_absolute_path::AbsolutePathBuf;
use pretty_assertions::assert_eq;
use std::path::Path;

#[tokio::test]
async fn fork_invariant_destructive_git_requires_explicit_override() {
    let (session, turn, _) = crate::session::tests::make_session_and_context_with_rx().await;
    let environment = turn
        .environments
        .primary()
        .cloned()
        .expect("test turn has a primary environment");
    let error = authorize_exec_command(
        session.as_ref(),
        turn.as_ref(),
        &["git".to_string(), "reset".to_string()],
        environment.cwd(),
        false,
        &environment,
        OwnershipOverrideAuthorization::NotRequested,
        None,
    )
    .await
    .expect_err("root destructive Git must require a one-shot override");
    assert!(error.contains("explicit one-shot root override"));
}

#[tokio::test]
async fn read_only_commands_do_not_require_workflow_state() {
    let (session, turn, _) = crate::session::tests::make_session_and_context_with_rx().await;
    let environment = turn
        .environments
        .primary()
        .cloned()
        .expect("test turn has a primary environment");
    assert!(
        authorize_exec_command(
            session.as_ref(),
            turn.as_ref(),
            &["git".to_string(), "status".to_string()],
            environment.cwd(),
            false,
            &environment,
            OwnershipOverrideAuthorization::NotRequested,
            None,
        )
        .await
        .expect("read-only admission should succeed")
        .is_none()
    );
}

#[test]
fn operation_digest_binds_command_and_checkout_paths() {
    let cwd = AbsolutePathBuf::try_from(std::env::current_dir().expect("current directory"))
        .expect("current directory is absolute");
    let paths = vec![cwd.join("src").into_path_buf()];
    let first = operation_digest(&["git".to_string(), "status".to_string()], &cwd, &paths);
    let changed = operation_digest(&["git".to_string(), "diff".to_string()], &cwd, &paths);
    assert_ne!(first, changed);
}

#[test]
fn relative_mutation_paths_resolve_against_command_cwd() {
    let cwd = AbsolutePathBuf::try_from(std::env::current_dir().expect("current directory"))
        .expect("current directory is absolute");
    assert_eq!(
        resolve_command_path(&cwd, Path::new("src/lib.rs")),
        cwd.join("src/lib.rs").into_path_buf()
    );
    let absolute = cwd.join("other/file.rs").into_path_buf();
    assert_eq!(resolve_command_path(&cwd, &absolute), absolute);
}

/// FORK: the kill-switch must not re-open what never depended on a lease.
///
/// `[features.workspace_ownership] enabled = false` turns lease coordination
/// off. It is placed *after* the destructive-Git denials for exactly this
/// reason: those are not lease decisions, and losing them with the leases would
/// hand a shared, dirty working tree to `git reset`.
#[tokio::test]
async fn fork_invariant_destructive_git_survives_disabled_lease_enforcement() {
    let (session, turn, _) =
        crate::session::tests::make_session_and_context_with_auth_and_config_and_rx(
            codex_login::CodexAuth::from_api_key("Test API Key"),
            Vec::new(),
            |config| config.workspace_ownership.enforce = false,
        )
        .await;
    let environment = turn
        .environments
        .primary()
        .cloned()
        .expect("test turn has a primary environment");
    let error = authorize_exec_command(
        session.as_ref(),
        turn.as_ref(),
        &["git".to_string(), "reset".to_string()],
        environment.cwd(),
        false,
        &environment,
        OwnershipOverrideAuthorization::NotRequested,
        None,
    )
    .await
    .expect_err("destructive Git stays gated with lease enforcement disabled");
    assert!(error.contains("explicit one-shot root override"), "{error}");
}

#[tokio::test]
async fn disabled_lease_enforcement_restores_legacy_admission() {
    let (session, turn, _) =
        crate::session::tests::make_session_and_context_with_auth_and_config_and_rx(
            codex_login::CodexAuth::from_api_key("Test API Key"),
            Vec::new(),
            |config| config.workspace_ownership.enforce = false,
        )
        .await;
    let environment = turn
        .environments
        .primary()
        .cloned()
        .expect("test turn has a primary environment");
    assert!(
        authorize_exec_command(
            session.as_ref(),
            turn.as_ref(),
            &["touch".to_string(), "output.txt".to_string()],
            environment.cwd(),
            false,
            &environment,
            OwnershipOverrideAuthorization::NotRequested,
            None,
        )
        .await
        .expect("a mutating command is admitted without ownership state")
        .is_none()
    );
}
