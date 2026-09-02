//! Provider-side ownership admission for local Claude and MCP execution.

#[path = "provider_paths.rs"]
mod provider_paths;

use super::EnsureLeaseRequest;
use super::LeaseHold;
use super::MutationAuthorizationRequest;
use super::MutationGuard;
use super::MutationOperation;
use super::OwnershipActor;
use super::OwnershipAuthority;
use super::OwnershipError;
use super::OwnershipOverrideAuthorization;
use super::WorkspaceOwnershipService;
use super::describe_ownership_error;
use super::ensure_subagent_write_leases;
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
use std::time::Duration;

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
    /// FORK: custody of a lease the runtime acquired for this provider turn.
    pub(crate) _lease_hold: Option<LeaseHold>,
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
        let roots = self.service.authorized_roots();
        Ok(normalized.iter().all(|path| {
            // A lease-exempt root is authorized but never leased, so no guard
            // scope covers it; without this a Claude edit into the thread's own
            // scratch directory would be refused as out of scope.
            roots.is_lease_exempt(path)
                || self
                    .guard
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
    /// FORK: no write lease is in force for this request.
    ///
    /// `notice` explains a *temporary* degradation — a sibling holds the paths —
    /// so the agent reports instead of failing, and the next sampling request
    /// tries again. A role that simply cannot write carries no notice.
    ReadOnly {
        notice: Option<String>,
    },
    /// FORK: lease coordination is switched off by config. Tools run, but the
    /// checks that never depended on a lease still apply.
    Unmanaged,
    Mutable(ProviderMutationGuard),
}

impl ClaudeProviderAccess {
    pub(crate) fn is_read_only(&self) -> bool {
        matches!(self, Self::ReadOnly { .. })
    }

    /// FORK: what to tell the agent about a degraded turn, if anything.
    pub(crate) fn ownership_notice(&self) -> Option<&str> {
        match self {
            Self::ReadOnly { notice } => notice.as_deref(),
            Self::Root | Self::Unmanaged | Self::Mutable(_) => None,
        }
    }

    /// Whether the Claude CLI must route tool calls through the Codex host.
    ///
    /// A writable subagent is admitted by a path lease, so the CLI's
    /// `bypassPermissions` mode cannot be used for it: that mode suppresses
    /// `can_use_tool` and would let the child skip the receiver-side lease and
    /// destructive-Git checks. Root access intentionally keeps its existing
    /// policy mapping; root is not a leased subagent.
    pub(crate) fn requires_tool_authorization(&self) -> bool {
        matches!(self, Self::Mutable(_) | Self::Unmanaged)
    }

    pub(crate) async fn authorize_claude_tool(
        &self,
        tool_name: &str,
        input: &serde_json::Value,
        cwd: &AbsolutePathBuf,
    ) -> Result<(), String> {
        let Self::Mutable(guard) = self else {
            return match self {
                Self::ReadOnly { notice } if is_mutating_claude_tool(tool_name) => {
                    Err(match notice {
                        Some(_) => "write access is paused for this request".to_string(),
                        None => "this Claude agent role is read-only".to_string(),
                    })
                }
                Self::Unmanaged => authorize_unmanaged_claude_tool(tool_name, input),
                _ => Ok(()),
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
///
/// FORK: this runs on *every* sampling request, so it must not be able to kill
/// the turn. A missing lease degrades the request to read-only and says so;
/// because the next request re-runs this, the agent recovers write access on
/// its own as soon as the sibling holding the paths lets go. Only a remote
/// executor is still an error, because ownership cannot be proven there at all.
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
    let policy = session.get_config().await.workspace_ownership.clone();
    if !policy.enforce {
        return Ok(ClaudeProviderAccess::Unmanaged);
    }
    if !actor.capabilities().may_request_workspace_lease() {
        return Ok(ClaudeProviderAccess::ReadOnly { notice: None });
    }
    let service = match session.ownership_service().await {
        Ok(service) => service,
        Err(error) => return Ok(degraded_access(describe_ownership_error(error))),
    };
    let environment_id = environment.selection.environment_id.clone();
    let (paths, scope) = if let Some(worktree_root) =
        detected_linked_worktree(environment, environment.cwd()).await
    {
        (
            vec![worktree_root.into_path_buf()],
            ProviderMutationScope::IsolatedWorktree,
        )
    } else {
        match checkout_paths(&service, turn) {
            Ok(paths) => (paths, ProviderMutationScope::FullCheckout),
            // Every writable root is lease-exempt scratch: there is nothing to
            // coordinate, so do not withhold write access over it.
            Err(CheckoutScope::AllExempt) => return Ok(ClaudeProviderAccess::Unmanaged),
            Err(CheckoutScope::NoRoots) => {
                return Ok(degraded_access(format!(
                    "{} has no authorized workspace roots",
                    turn.session_source
                )));
            }
        }
    };

    let ensured = match ensure_subagent_write_leases(EnsureLeaseRequest {
        service: &service,
        coordinator: &session.services.lease_coordinator,
        actor,
        paths: &paths,
        environment_id: &environment_id,
        ttl: Duration::from_millis(policy.auto_ttl_ms as u64),
        wait: Duration::from_millis(policy.provider_wait_ms as u64),
        auto_acquire: policy.auto_acquire,
        cancel: None,
    })
    .await
    {
        Ok(ensured) => ensured,
        Err(error) => return Ok(degraded_access(describe_ownership_error(error))),
    };
    if ensured.is_exempt() {
        return Ok(ClaudeProviderAccess::Unmanaged);
    }

    let operation = MutationOperation {
        digest: operation_digest(b"claude.provider", &paths, &environment_id),
    };
    let guard = match service
        .authorize_mutation(MutationAuthorizationRequest {
            actor,
            paths: paths.clone(),
            operation,
            override_authorization: OwnershipOverrideAuthorization::NotRequested,
        })
        .await
    {
        Ok(guard) => guard,
        Err(error) => return Ok(degraded_access(describe_ownership_error(error))),
    };
    if let Err(error) = service
        .require_full_environment_lease(&guard, &paths, &environment_id)
        .await
    {
        return Ok(degraded_access(describe_ownership_error(error)));
    }
    Ok(ClaudeProviderAccess::Mutable(ProviderMutationGuard {
        service,
        guard,
        scope,
        environment_id,
        _lease_hold: ensured.hold(),
    }))
}

/// FORK: a read-only turn the next sampling request will try to upgrade.
fn degraded_access(reason: String) -> ClaudeProviderAccess {
    ClaudeProviderAccess::ReadOnly {
        notice: Some(format!(
            "Write access is paused for this reply: {reason}. Codex retries automatically on the next request, so keep working read-only and say what you would change."
        )),
    }
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
    let policy = session.get_config().await.workspace_ownership.clone();
    if !policy.enforce {
        return Ok(None);
    }
    let service = match session.ownership_service().await {
        Ok(service) => service,
        Err(error)
            if actor.authority() == OwnershipAuthority::Root
                && super::ownership_state_is_absent(&error) =>
        {
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
        match checkout_paths(&service, turn) {
            Ok(paths) => paths,
            // Nothing writable here needs coordinating.
            Err(CheckoutScope::AllExempt) => return Ok(None),
            Err(CheckoutScope::NoRoots) => {
                return Err(format!(
                    "{} has no authorized workspace roots",
                    turn.session_source
                ));
            }
        }
    } else {
        known_paths.clone()
    };
    let environment_id = environment.selection.environment_id.clone();
    if actor.authority() == OwnershipAuthority::Subagent
        && let Some(worktree_root) = detected_linked_worktree(environment, environment.cwd()).await
    {
        let scope_paths = vec![worktree_root.into_path_buf()];
        let ensured = ensure_mcp_leases(
            session,
            &service,
            actor,
            &scope_paths,
            &environment_id,
            &policy,
        )
        .await?;
        if ensured.is_exempt() {
            return Ok(None);
        }
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
            _lease_hold: ensured.hold(),
        };
        if !known_paths.is_empty() && !provider_guard.covers_paths(&known_paths)? {
            return Err("MCP path is outside the isolated worktree".to_string());
        }
        return Ok(Some(provider_guard));
    }
    let ensured =
        ensure_mcp_leases(session, &service, actor, &paths, &environment_id, &policy).await?;
    if ensured.is_exempt() {
        return Ok(None);
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
        _lease_hold: ensured.hold(),
    }))
}

async fn ensure_mcp_leases(
    session: &Session,
    service: &Arc<WorkspaceOwnershipService>,
    actor: OwnershipActor,
    paths: &[PathBuf],
    environment_id: &str,
    policy: &crate::config::WorkspaceOwnershipConfig,
) -> Result<super::EnsuredLeases, String> {
    ensure_subagent_write_leases(EnsureLeaseRequest {
        service,
        coordinator: &session.services.lease_coordinator,
        actor,
        paths,
        environment_id,
        ttl: Duration::from_millis(policy.auto_ttl_ms as u64),
        wait: Duration::from_millis(policy.exec_wait_ms as u64),
        auto_acquire: policy.auto_acquire,
        cancel: None,
    })
    .await
    .map_err(format_ownership_error)
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

/// Why a provider could not be scoped to a leasable checkout.
enum CheckoutScope {
    /// The session has no authorized workspace roots at all.
    NoRoots,
    /// Every authorized root is lease-exempt scratch.
    AllExempt,
}

fn checkout_paths(
    service: &WorkspaceOwnershipService,
    _turn: &TurnContext,
) -> Result<Vec<PathBuf>, CheckoutScope> {
    let roots = service.authorized_roots();
    let all = roots.roots().map(Path::to_path_buf).collect::<Vec<_>>();
    if all.is_empty() {
        return Err(CheckoutScope::NoRoots);
    }
    // The scratch directory is authorized but never leased, so asking for a
    // lease over it would make every provider turn depend on a claim nobody
    // ever grants.
    let leasable = all
        .into_iter()
        .filter(|path| {
            roots
                .normalize(path)
                .map(|normalized| !roots.is_lease_exempt(&normalized))
                .unwrap_or(true)
        })
        .collect::<Vec<_>>();
    if leasable.is_empty() {
        return Err(CheckoutScope::AllExempt);
    }
    Ok(leasable)
}

/// FORK: what still applies to a Claude tool call when leases are switched off.
///
/// The destructive-Git denial never depended on a lease, and a shared, dirty
/// working tree is exactly where it matters most.
fn authorize_unmanaged_claude_tool(
    tool_name: &str,
    input: &serde_json::Value,
) -> Result<(), String> {
    if tool_name != "Bash" {
        return Ok(());
    }
    let command = input
        .get("command")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| "Claude Bash request has no literal command".to_string())?;
    let words = vec!["bash".to_string(), "-lc".to_string(), command.to_string()];
    match codex_shell_command::classify_command(&words) {
        codex_shell_command::MutationIntent::DestructiveGit { .. } => {
            Err("subagents cannot execute destructive Git commands".to_string())
        }
        _ => Ok(()),
    }
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
    describe_ownership_error(error)
}

#[cfg(test)]
#[path = "provider_tests.rs"]
mod tests;
