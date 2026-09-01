//! Ownership admission for unified exec.
//!
//! The initial admission happens before the orchestrator is invoked. The runtime calls
//! [`revalidate_exec_authorization`] after approval and sandbox selection, immediately before a
//! process is spawned. A running process keeps the returned authorization; stdin continuation does
//! not perform a second caller-based authorization.

use crate::ownership::MutationAuthorizationRequest;
use crate::ownership::MutationOperation;
use crate::ownership::OwnershipActor;
use crate::ownership::OwnershipAuthority;
use crate::ownership::OwnershipError;
use crate::ownership::OwnershipOverrideAuthorization;
use crate::session::session::Session;
use crate::session::turn_context::TurnContext;
use crate::session::turn_context::TurnEnvironment;
use crate::tools::sandboxing::ToolError;
use crate::unified_exec::ExecMutationAuthorization;
use codex_shell_command::MutationIntent;
use codex_utils_absolute_path::AbsolutePathBuf;
use codex_utils_path_uri::PathUri;
use sha2::Digest;
use sha2::Sha256;
use std::path::Path;
use std::path::PathBuf;

/// Admit a command through the root-scoped ownership service before the orchestrator can spawn it.
pub(crate) async fn authorize_exec_command(
    session: &Session,
    turn: &TurnContext,
    command: &[String],
    cwd: &PathUri,
    tty: bool,
    environment: &TurnEnvironment,
    override_authorization: OwnershipOverrideAuthorization,
) -> Result<Option<ExecMutationAuthorization>, String> {
    let intent = codex_shell_command::classify_command(command);
    if matches!(&intent, MutationIntent::ReadOnly) && !tty {
        return Ok(None);
    }

    let actor = ownership_actor(session, turn);
    if actor.authority() == OwnershipAuthority::Subagent
        && matches!(intent, MutationIntent::DestructiveGit { .. })
    {
        return Err("subagents cannot execute destructive Git commands".to_string());
    }
    if environment.environment.is_remote() {
        return Err("workspace mutation admission is unavailable for remote executors".to_string());
    }

    let native_cwd = cwd
        .to_abs_path()
        .map_err(|error| format!("workspace mutation cwd is not local: {error}"))?;
    let config = session.get_config().await;
    let checkout_paths = config
        .effective_workspace_roots()
        .into_iter()
        .map(AbsolutePathBuf::into_path_buf)
        .collect::<Vec<_>>();
    let checkout_paths = if checkout_paths.is_empty() {
        vec![native_cwd.clone().into_path_buf()]
    } else {
        checkout_paths
    };
    let linked_worktree = if (tty || matches!(&intent, MutationIntent::RequiresCheckoutLease))
        && !environment.environment.is_remote()
    {
        detected_linked_worktree(environment, &native_cwd).await
    } else {
        None
    };
    let paths = match &intent {
        MutationIntent::WritesKnownPaths(paths) => paths
            .iter()
            .map(|path| resolve_command_path(&native_cwd, path))
            .collect(),
        MutationIntent::ReadOnly => linked_worktree
            .as_ref()
            .map(|path| vec![path.clone().into_path_buf()])
            .unwrap_or_else(|| checkout_paths.clone()),
        MutationIntent::DestructiveGit { .. } => checkout_paths.clone(),
        MutationIntent::RequiresCheckoutLease => linked_worktree
            .as_ref()
            .map(|path| vec![path.clone().into_path_buf()])
            .unwrap_or_else(|| checkout_paths.clone()),
    };
    if matches!(&intent, MutationIntent::DestructiveGit { .. })
        && actor.authority() == OwnershipAuthority::Root
        && matches!(
            &override_authorization,
            OwnershipOverrideAuthorization::NotRequested
        )
    {
        return Err("destructive Git requires an explicit one-shot root override".to_string());
    }

    let service = match session.ownership_service().await {
        Ok(service) => service,
        Err(OwnershipError::Unavailable)
            if actor.authority() == OwnershipAuthority::Root
                && !matches!(&intent, MutationIntent::DestructiveGit { .. }) =>
        {
            return Ok(None);
        }
        Err(error) => return Err(format!("workspace ownership check failed: {error}")),
    };
    let operation = MutationOperation {
        digest: operation_digest(command, &native_cwd, &paths),
    };
    let guard = service
        .authorize_mutation(MutationAuthorizationRequest {
            actor,
            paths: paths.clone(),
            operation,
            override_authorization,
        })
        .await
        .map_err(format_ownership_error)?;
    if actor.authority() == OwnershipAuthority::Subagent {
        service
            .require_full_environment_lease(&guard, &paths, &environment.selection.environment_id)
            .await
            .map_err(format_ownership_error)?;
    }
    Ok(Some(ExecMutationAuthorization { service, guard }))
}

/// Revalidate a guard after tool approval and sandbox selection, immediately before spawn.
pub(crate) async fn revalidate_exec_authorization(
    authorization: &ExecMutationAuthorization,
) -> Result<(), ToolError> {
    authorization
        .service
        .revalidate_guard(&authorization.guard)
        .await
        .map_err(|error| ToolError::Rejected(format_ownership_error(error)))?;
    Ok(())
}

fn ownership_actor(session: &Session, turn: &TurnContext) -> OwnershipActor {
    if turn.session_source.is_non_root_agent() {
        OwnershipActor::subagent_for_role(
            session.thread_id(),
            turn.session_source.get_agent_role().as_deref(),
        )
    } else {
        OwnershipActor::root(session.thread_id())
    }
}

fn resolve_command_path(cwd: &AbsolutePathBuf, path: &Path) -> PathBuf {
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        cwd.join(path).into_path_buf()
    }
}

/// Detect a trusted linked-worktree root for narrowing the requested scope.
/// Git metadata proves only the repository shape; the caller must still
/// obtain an actor- and environment-bound durable write lease.
async fn detected_linked_worktree(
    environment: &TurnEnvironment,
    cwd: &AbsolutePathBuf,
) -> Option<AbsolutePathBuf> {
    let filesystem = environment.environment.get_filesystem();
    let mut current = cwd.as_path().to_path_buf();
    loop {
        let dot_git = AbsolutePathBuf::try_from(current.join(".git")).ok()?;
        let dot_git_uri = PathUri::from_abs_path(&dot_git);
        if let Ok(metadata) = filesystem
            .get_metadata(&dot_git_uri, Default::default(), /*sandbox*/ None)
            .await
            && metadata.is_file
            && !metadata.is_symlink
        {
            return codex_git_utils::resolve_root_git_project_for_trust(filesystem.as_ref(), cwd)
                .await
                .and_then(|_| AbsolutePathBuf::try_from(current).ok());
        }
        if !current.pop() {
            break;
        }
    }
    None
}

fn operation_digest(command: &[String], cwd: &AbsolutePathBuf, paths: &[PathBuf]) -> String {
    let mut digest = Sha256::new();
    digest.update(b"codex.unified_exec.ownership.v1\0");
    update_digest_part(&mut digest, cwd.to_string_lossy().as_bytes());
    for word in command {
        update_digest_part(&mut digest, word.as_bytes());
    }
    let mut path_keys = paths
        .iter()
        .map(|path| path.to_string_lossy().replace('\\', "/"))
        .collect::<Vec<_>>();
    path_keys.sort();
    path_keys.dedup();
    for path in path_keys {
        update_digest_part(&mut digest, path.as_bytes());
    }
    format!("{:x}", digest.finalize())
}

fn update_digest_part(digest: &mut Sha256, value: &[u8]) {
    digest.update((value.len() as u64).to_be_bytes());
    digest.update(value);
}

fn format_ownership_error(error: OwnershipError) -> String {
    match error {
        OwnershipError::LeaseRequired { path } => {
            format!("write lease required for {}", path.display())
        }
        OwnershipError::Conflict { .. } => {
            "workspace ownership conflicts with another agent".to_string()
        }
        OwnershipError::ReadOnlyRole => "the agent role is read-only".to_string(),
        OwnershipError::Unavailable => "workspace ownership state is unavailable".to_string(),
        other => other.to_string(),
    }
}

#[cfg(test)]
#[path = "unified_exec_ownership_tests.rs"]
mod tests;
