//! Ownership admission for apply_patch before and during filesystem mutation.

use crate::function_tool::FunctionCallError;
use crate::ownership::EnsureLeaseRequest;
use crate::ownership::LeaseHold;
use crate::ownership::MutationAuthorizationRequest;
use crate::ownership::MutationGuard;
use crate::ownership::MutationOperation;
use crate::ownership::NormalizedLeasePath;
use crate::ownership::OwnershipActor;
use crate::ownership::OwnershipAuthority;
use crate::ownership::OwnershipError;
use crate::ownership::OwnershipOverrideAuthorization;
use crate::ownership::OwnershipPathError;
use crate::ownership::WorkspaceOwnershipService;
use crate::ownership::describe_ownership_error;
use crate::ownership::ensure_subagent_write_leases;
use crate::session::session::Session;
use crate::session::turn_context::TurnContext;
use codex_apply_patch::ApplyPatchAction;
use codex_utils_absolute_path::AbsolutePathBuf;
use codex_utils_path_uri::PathUri;
use sha2::Digest;
use sha2::Sha256;
use std::sync::Arc;
use std::time::Duration;

pub(crate) struct ApplyPatchOwnership {
    pub(crate) service: Arc<WorkspaceOwnershipService>,
    pub(crate) guard: MutationGuard,
    /// FORK: custody of a lease acquired for this patch, released with it.
    pub(crate) _lease_hold: Option<LeaseHold>,
}

/// Admit an apply_patch operation through the root-scoped ownership service.
/// Root sessions without workflow state retain legacy behavior; descendants
/// fail closed when ownership state is unavailable or the lease is missing.
pub(crate) async fn authorize_apply_patch(
    session: &Session,
    turn: &TurnContext,
    action: &ApplyPatchAction,
    file_paths: &[PathUri],
) -> Result<Option<ApplyPatchOwnership>, FunctionCallError> {
    let actor = ownership_actor(session, turn);
    let ownership_policy = session.get_config().await.workspace_ownership.clone();
    if !ownership_policy.enforce {
        return Ok(None);
    }
    let service = match session.ownership_service().await {
        Ok(service) => service,
        Err(error)
            if actor.authority() == OwnershipAuthority::Root
                && matches!(
                    error,
                    OwnershipError::Unavailable
                        | OwnershipError::Path(
                            OwnershipPathError::NoRoots | OwnershipPathError::OutsideRoots { .. }
                        )
                ) =>
        {
            return Ok(None);
        }
        Err(error) => return Err(ownership_error(error)),
    };
    let paths = file_paths
        .iter()
        .map(PathUri::to_abs_path)
        .collect::<Result<Vec<_>, _>>()
        .map_err(|error| {
            FunctionCallError::RespondToModel(format!(
                "apply_patch ownership path resolution failed: {error}"
            ))
        })?;
    let normalized_paths = match paths
        .iter()
        .map(|path| service.authorized_roots().normalize(path))
        .collect::<Result<Vec<_>, _>>()
    {
        Ok(paths) => paths,
        Err(error)
            if actor.authority() == OwnershipAuthority::Root
                && matches!(error, OwnershipPathError::OutsideRoots { .. }) =>
        {
            return Ok(None);
        }
        Err(error) => return Err(ownership_error(OwnershipError::Path(error))),
    };
    let operation = MutationOperation {
        digest: operation_digest(action, &normalized_paths),
    };
    let paths = paths
        .into_iter()
        .map(AbsolutePathBuf::into_path_buf)
        .collect::<Vec<_>>();
    // apply_patch names its exact targets, so the lease it takes is exactly
    // those files rather than the whole checkout.
    let ensured = ensure_subagent_write_leases(EnsureLeaseRequest {
        service: &service,
        coordinator: &session.services.lease_coordinator,
        actor,
        paths: &paths,
        environment_id: codex_exec_server::LOCAL_ENVIRONMENT_ID,
        ttl: Duration::from_millis(ownership_policy.auto_ttl_ms as u64),
        wait: Duration::from_millis(ownership_policy.exec_wait_ms as u64),
        auto_acquire: ownership_policy.auto_acquire,
        cancel: None,
    })
    .await
    .map_err(ownership_error)?;
    if ensured.is_exempt() {
        return Ok(None);
    }
    let guard = service
        .authorize_mutation(MutationAuthorizationRequest {
            actor,
            paths,
            operation,
            override_authorization: OwnershipOverrideAuthorization::NotRequested,
        })
        .await
        .map_err(ownership_error)?;
    Ok(Some(ApplyPatchOwnership {
        service,
        guard,
        _lease_hold: ensured.hold(),
    }))
}

fn ownership_actor(session: &Session, turn: &TurnContext) -> OwnershipActor {
    if turn.session_source.is_non_root_agent() {
        let role = turn.session_source.get_agent_role();
        OwnershipActor::subagent_for_role(session.thread_id(), role.as_deref())
    } else {
        OwnershipActor::root(session.thread_id())
    }
}

fn operation_digest(action: &ApplyPatchAction, paths: &[NormalizedLeasePath]) -> String {
    let mut path_keys = paths
        .iter()
        .map(|path| path.comparison_key().to_string())
        .collect::<Vec<_>>();
    path_keys.sort();
    path_keys.dedup();

    let mut digest = Sha256::new();
    digest.update(b"codex.apply_patch.ownership.v1\0");
    update_digest_part(&mut digest, action.patch.as_bytes());
    for path in path_keys {
        update_digest_part(&mut digest, path.as_bytes());
    }
    format!("{:x}", digest.finalize())
}

fn update_digest_part(digest: &mut Sha256, value: &[u8]) {
    digest.update((value.len() as u64).to_be_bytes());
    digest.update(value);
}

fn ownership_error(error: OwnershipError) -> FunctionCallError {
    FunctionCallError::RespondToModel(format!(
        "apply_patch ownership check failed: {}",
        describe_ownership_error(error)
    ))
}
