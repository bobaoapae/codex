//! Provider-side ownership admission for local Claude and MCP execution.

#[path = "provider_paths.rs"]
mod provider_paths;

use super::MutationAuthorizationRequest;
use super::MutationGuard;
use super::MutationOperation;
use super::OwnershipActor;
use super::OwnershipAuthority;
use super::OwnershipError;
use super::OwnershipOverrideAuthorization;
use super::WorkspaceOwnershipService;
use crate::session::session::Session;
use crate::session::turn_context::TurnContext;
use crate::session::turn_context::TurnEnvironment;
use codex_utils_absolute_path::AbsolutePathBuf;
use codex_utils_path_uri::PathUri;
use provider_paths::claude_input_paths;
use provider_paths::extract_mcp_paths;
use provider_paths::resolve_path;
use sha2::Digest;
use sha2::Sha256;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;

/// Scope retained by a provider while it executes a mutating tool.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ProviderMutationScope {
    /// An active write lease covers the complete configured checkout.
    FullCheckout,
    /// A durable write lease covers a verified linked worktree.
    IsolatedWorktree,
}

/// Provider-local guard retained for the duration of one external tool call or turn.
#[derive(Clone)]
pub(crate) struct ProviderMutationGuard {
    pub(crate) service: Arc<WorkspaceOwnershipService>,
    pub(crate) guard: MutationGuard,
    pub(crate) scope: ProviderMutationScope,
    pub(crate) environment_id: String,
}

impl std::fmt::Debug for ProviderMutationGuard {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ProviderMutationGuard")
            .field("scope", &self.scope)
            .field("environment_id", &self.environment_id)
            .finish_non_exhaustive()
    }
}

impl ProviderMutationGuard {
    pub(crate) async fn revalidate(&self) -> Result<(), String> {
        self.service
            .revalidate_guard(&self.guard)
            .await
            .map_err(format_ownership_error)
    }

    pub(crate) fn allows_full_checkout(&self) -> bool {
        self.scope == ProviderMutationScope::FullCheckout
    }

    pub(crate) fn covers_paths(&self, paths: &[PathBuf]) -> Result<bool, String> {
        let normalized = paths
            .iter()
            .map(|path| self.service.authorized_roots().normalize(path))
            .collect::<Result<Vec<_>, _>>()
            .map_err(|error| format_ownership_error(OwnershipError::Path(error)))?;
        Ok(normalized.iter().all(|path| {
            self.guard
                .paths()
                .iter()
                .any(|scope| scope.is_ancestor_or_equal(path))
        }))
    }
}

/// Access mode selected for a local Claude provider turn.
#[derive(Clone, Debug)]
pub(crate) enum ClaudeProviderAccess {
    Root,
    ReadOnly,
    Mutable(ProviderMutationGuard),
}

impl ClaudeProviderAccess {
    pub(crate) fn is_read_only(&self) -> bool {
        matches!(self, Self::ReadOnly)
    }

    /// Whether the Claude CLI must route tool calls through the Codex host.
    ///
    /// A writable subagent is admitted by a path lease, so the CLI's
    /// `bypassPermissions` mode cannot be used for it: that mode suppresses
    /// `can_use_tool` and would let the child skip the receiver-side lease and
    /// destructive-Git checks. Root access intentionally keeps its existing
    /// policy mapping; root is not a leased subagent.
    pub(crate) fn requires_tool_authorization(&self) -> bool {
        matches!(self, Self::Mutable(_))
    }

    pub(crate) async fn authorize_claude_tool(
        &self,
        tool_name: &str,
        input: &serde_json::Value,
        cwd: &AbsolutePathBuf,
    ) -> Result<(), String> {
        let Self::Mutable(guard) = self else {
            return if self.is_read_only() && is_mutating_claude_tool(tool_name) {
                Err("this Claude agent role is read-only".to_string())
            } else {
                Ok(())
            };
        };
        guard.revalidate().await?;
        match tool_name {
            "Bash" => {
                let command = input
                    .get("command")
                    .and_then(serde_json::Value::as_str)
                    .ok_or_else(|| "Claude Bash request has no literal command".to_string())?;
                let words = vec!["bash".to_string(), "-lc".to_string(), command.to_string()];
                match codex_shell_command::classify_command(&words) {
                    codex_shell_command::MutationIntent::ReadOnly => Ok(()),
                    codex_shell_command::MutationIntent::DestructiveGit { .. } => {
                        Err("subagents cannot execute destructive Git commands".to_string())
                    }
                    codex_shell_command::MutationIntent::WritesKnownPaths(paths) => {
                        let paths = paths
                            .iter()
                            .map(|path| resolve_path(cwd, path))
                            .collect::<Vec<_>>();
                        if guard.covers_paths(&paths)? {
                            Ok(())
                        } else {
                            Err("Claude Bash path is outside the admitted ownership scope"
                                .to_string())
                        }
                    }
                    codex_shell_command::MutationIntent::RequiresCheckoutLease => {
                        if guard.allows_full_checkout()
                            || guard.scope == ProviderMutationScope::IsolatedWorktree
                        {
                            Ok(())
                        } else {
                            Err("complex Claude Bash requires a full checkout lease".to_string())
                        }
                    }
                }
            }
            "Edit" | "MultiEdit" | "Write" | "NotebookEdit" => {
                let paths = claude_input_paths(input, cwd);
                if paths.is_empty() {
                    return Err("Claude edit request has no provable target path".to_string());
                }
                if guard.covers_paths(&paths)? {
                    Ok(())
                } else {
                    Err("Claude edit path is outside the admitted ownership scope".to_string())
                }
            }
            name if name.starts_with("mcp__") => {
                if guard.allows_full_checkout()
                    || guard.scope == ProviderMutationScope::IsolatedWorktree
                {
                    Ok(())
                } else {
                    Err("Claude MCP mutation requires a full checkout lease".to_string())
                }
            }
            _ => Ok(()),
        }
    }
}

/// Prepare local Claude provider access before its process is launched.
pub(crate) async fn authorize_claude_provider(
    session: &Session,
    turn: &TurnContext,
    environment: &TurnEnvironment,
) -> Result<ClaudeProviderAccess, String> {
    if environment.environment.is_remote() {
        return Err("local Claude cannot reuse ownership from a remote executor".to_string());
    }
    let actor = ownership_actor(session, turn);
    if actor.authority() == OwnershipAuthority::Root {
        return Ok(ClaudeProviderAccess::Root);
    }
    if !actor.capabilities().may_request_workspace_lease() {
        return Ok(ClaudeProviderAccess::ReadOnly);
    }
    let service = session
        .ownership_service()
        .await
        .map_err(|error| format!("Claude ownership check failed: {error}"))?;
    let environment_id = environment.selection.environment_id.clone();
    let (paths, scope) = if let Some(worktree_root) =
        detected_linked_worktree(environment, environment.cwd()).await
    {
        (
            vec![worktree_root.into_path_buf()],
            ProviderMutationScope::IsolatedWorktree,
        )
    } else {
        (
            checkout_paths(&service, turn)?,
            ProviderMutationScope::FullCheckout,
        )
    };

    let operation = MutationOperation {
        digest: operation_digest(b"claude.provider", &paths, &environment_id),
    };
    let guard = service
        .authorize_mutation(MutationAuthorizationRequest {
            actor,
            paths: paths.clone(),
            operation,
            override_authorization: OwnershipOverrideAuthorization::NotRequested,
        })
        .await
        .map_err(format_ownership_error)?;
    service
        .require_full_environment_lease(&guard, &paths, &environment_id)
        .await
        .map_err(format_ownership_error)?;
    Ok(ClaudeProviderAccess::Mutable(ProviderMutationGuard {
        service,
        guard,
        scope,
        environment_id,
    }))
}

/// Admit a mutating MCP call. Read-only annotations are handled by the caller and do not enter
/// this function. `None` means a root call retained normal policy-only behavior without state.
pub(crate) async fn authorize_mcp_mutation(
    session: &Session,
    turn: &TurnContext,
    environment: &TurnEnvironment,
    arguments: &serde_json::Value,
) -> Result<Option<ProviderMutationGuard>, String> {
    if environment.environment.is_remote() {
        return Err("local ownership cannot authorize a remote MCP executor".to_string());
    }
    let actor = ownership_actor(session, turn);
    let service = match session.ownership_service().await {
        Ok(service) => service,
        Err(OwnershipError::Unavailable) if actor.authority() == OwnershipAuthority::Root => {
            return Ok(None);
        }
        Err(error) => return Err(format_ownership_error(error)),
    };
    let cwd = environment
        .cwd()
        .to_abs_path()
        .map_err(|error| format!("MCP environment cwd is not local: {error}"))?;
    let known_paths = extract_mcp_paths(arguments)
        .into_iter()
        .map(|path| resolve_path(&cwd, &path))
        .collect::<Vec<_>>();
    let full_scope = known_paths.is_empty();
    let paths = if full_scope {
        checkout_paths(&service, turn)?
    } else {
        known_paths.clone()
    };
    let environment_id = environment.selection.environment_id.clone();
    if actor.authority() == OwnershipAuthority::Subagent
        && let Some(worktree_root) = detected_linked_worktree(environment, environment.cwd()).await
    {
        let scope_paths = vec![worktree_root.into_path_buf()];
        let guard = service
            .authorize_mutation(MutationAuthorizationRequest {
                actor,
                paths: scope_paths.clone(),
                operation: MutationOperation {
                    digest: operation_digest(b"mcp", &scope_paths, &environment_id),
                },
                override_authorization: OwnershipOverrideAuthorization::NotRequested,
            })
            .await
            .map_err(format_ownership_error)?;
        service
            .require_full_environment_lease(&guard, &scope_paths, &environment_id)
            .await
            .map_err(format_ownership_error)?;
        let provider_guard = ProviderMutationGuard {
            guard,
            service,
            scope: ProviderMutationScope::IsolatedWorktree,
            environment_id,
        };
        if !known_paths.is_empty() && !provider_guard.covers_paths(&known_paths)? {
            return Err("MCP path is outside the isolated worktree".to_string());
        }
        return Ok(Some(provider_guard));
    }
    let guard = service
        .authorize_mutation(MutationAuthorizationRequest {
            actor,
            paths: paths.clone(),
            operation: MutationOperation {
                digest: operation_digest(b"mcp", &paths, &environment_id),
            },
            override_authorization: OwnershipOverrideAuthorization::NotRequested,
        })
        .await
        .map_err(format_ownership_error)?;
    if actor.authority() == OwnershipAuthority::Subagent {
        service
            .require_full_environment_lease(&guard, &paths, &environment_id)
            .await
            .map_err(format_ownership_error)?;
    }
    Ok(Some(ProviderMutationGuard {
        service,
        guard,
        scope: ProviderMutationScope::FullCheckout,
        environment_id,
    }))
}

pub(crate) fn ownership_actor(session: &Session, turn: &TurnContext) -> OwnershipActor {
    if turn.session_source.is_non_root_agent() {
        OwnershipActor::subagent_for_role(
            session.thread_id(),
            turn.session_source.get_agent_role().as_deref(),
        )
    } else {
        OwnershipActor::root(session.thread_id())
    }
}

fn checkout_paths(
    service: &WorkspaceOwnershipService,
    turn: &TurnContext,
) -> Result<Vec<PathBuf>, String> {
    let paths = service
        .authorized_roots()
        .roots()
        .map(Path::to_path_buf)
        .collect::<Vec<_>>();
    if paths.is_empty() {
        return Err(format!(
            "{} has no authorized workspace roots",
            turn.session_source
        ));
    }
    Ok(paths)
}

/// Detect a trusted linked-worktree root for narrowing provider scope.
/// The result is never an ownership grant; the durable lease check remains
/// mandatory before a provider guard is constructed.
async fn detected_linked_worktree(
    environment: &TurnEnvironment,
    cwd: &PathUri,
) -> Option<AbsolutePathBuf> {
    let cwd = cwd.to_abs_path().ok()?;
    let filesystem = environment.environment.get_filesystem();
    let mut current = cwd.as_path().to_path_buf();
    loop {
        let dot_git = AbsolutePathBuf::try_from(current.join(".git")).ok()?;
        let metadata = filesystem
            .get_metadata(
                &PathUri::from_abs_path(&dot_git),
                Default::default(),
                /*sandbox*/ None,
            )
            .await
            .ok()?;
        if metadata.is_file && !metadata.is_symlink {
            return codex_git_utils::resolve_root_git_project_for_trust(filesystem.as_ref(), &cwd)
                .await
                .and_then(|_| AbsolutePathBuf::try_from(current).ok());
        }
        if !current.pop() {
            return None;
        }
    }
}

fn operation_digest(prefix: &[u8], paths: &[PathBuf], environment_id: &str) -> String {
    let mut digest = Sha256::new();
    digest.update(b"codex.provider.ownership.v1\0");
    digest.update(prefix);
    digest.update([0]);
    digest.update(environment_id.as_bytes());
    let mut paths = paths
        .iter()
        .map(|path| path.to_string_lossy().replace('\\', "/"))
        .collect::<Vec<_>>();
    paths.sort();
    paths.dedup();
    for path in paths {
        digest.update((path.len() as u64).to_be_bytes());
        digest.update(path.as_bytes());
    }
    format!("{:x}", digest.finalize())
}

fn is_mutating_claude_tool(name: &str) -> bool {
    matches!(
        name,
        "Bash" | "Edit" | "MultiEdit" | "Write" | "NotebookEdit"
    ) || name.starts_with("mcp__")
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
#[path = "provider_tests.rs"]
mod tests;
