//! FORK: on-demand acquisition of the write leases an admission needs.
//!
//! Every mutating admission (exec, apply_patch, MCP, and the Claude provider
//! prepare) used to require a lease that only a root tool could grant, and no
//! model called that tool. This is the runtime path that obtains one instead:
//! it skips lease-exempt scratch roots, reuses a lease the actor already holds,
//! and otherwise takes one — waiting a bounded time when a sibling is mid-write
//! rather than failing the turn on contact.

use super::LeaseCoordinator;
use super::LeaseHold;
use super::OwnershipActor;
use super::OwnershipAuthority;
use super::OwnershipError;
use super::WorkspaceOwnershipService;
use super::service_helpers::select_actor_leases;
use super::service_helpers::state_path_covers;
use super::service_helpers::state_path_overlaps;
use codex_state::WorkflowLeaseMode;
use codex_state::WorkflowLeaseReleaseRequest;
use codex_state::WorkflowLeaseState;
use codex_state::WorkflowPathLease;
use rand::Rng;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use std::time::Instant;
use tokio_util::sync::CancellationToken;
use tracing::debug;

/// Shortest and longest re-check delay while waiting for a sibling.
///
/// Expiry is lazy — nothing publishes it — so a waiter cannot rely on the
/// activity signal alone; the jittered floor is what notices a dead holder, and
/// the jitter is what keeps two waiters from retrying in lockstep forever.
const MIN_RETRY_DELAY: Duration = Duration::from_millis(300);
const MAX_RETRY_DELAY: Duration = Duration::from_millis(500);

/// What the runtime did to satisfy an admission's lease requirement.
#[derive(Debug)]
pub(crate) enum EnsuredLeases {
    /// Every requested path lies under a lease-exempt root.
    Exempt,
    /// Nothing was acquired: the actor already holds covering leases, is the
    /// root, or auto-acquisition is switched off.
    Covered,
    /// The runtime took the leases and holds custody of them.
    Acquired(LeaseHold),
}

impl EnsuredLeases {
    /// The custody handle to keep alive for as long as the mutation may run.
    pub(crate) fn hold(&self) -> Option<LeaseHold> {
        match self {
            Self::Acquired(hold) => Some(hold.clone()),
            Self::Exempt | Self::Covered => None,
        }
    }

    /// Whether admission can skip the lease check entirely for these paths.
    pub(crate) fn is_exempt(&self) -> bool {
        matches!(self, Self::Exempt)
    }
}

/// Everything one call needs; grouped because all of it is required together.
pub(crate) struct EnsureLeaseRequest<'a> {
    pub(crate) service: &'a Arc<WorkspaceOwnershipService>,
    pub(crate) coordinator: &'a LeaseCoordinator,
    pub(crate) actor: OwnershipActor,
    pub(crate) paths: &'a [PathBuf],
    pub(crate) environment_id: &'a str,
    /// Lifetime of a lease this call acquires.
    pub(crate) ttl: Duration,
    /// Bounded time to wait for a conflicting sibling. Zero never waits.
    pub(crate) wait: Duration,
    /// Whether the runtime may acquire on the actor's behalf at all.
    pub(crate) auto_acquire: bool,
    pub(crate) cancel: Option<&'a CancellationToken>,
}

/// Make sure `actor` can write `paths`, acquiring and waiting as needed.
pub(crate) async fn ensure_subagent_write_leases(
    request: EnsureLeaseRequest<'_>,
) -> Result<EnsuredLeases, OwnershipError> {
    if request.actor.authority() != OwnershipAuthority::Subagent {
        return Ok(EnsuredLeases::Covered);
    }
    let service = request.service;
    let (normalized, state_paths) = service.normalize_paths(request.paths)?;
    let roots = service.authorized_roots();
    // Drop the exempt paths from both views together, so a diagnostic never
    // names a scratch path as the one being waited on.
    let (normalized, state_paths): (Vec<_>, Vec<_>) = normalized
        .into_iter()
        .zip(state_paths)
        .filter(|(normalized, _)| !roots.is_lease_exempt(normalized))
        .unzip();
    if state_paths.is_empty() {
        return Ok(EnsuredLeases::Exempt);
    }
    if !request.auto_acquire || !request.actor.capabilities().may_request_workspace_lease() {
        return Ok(EnsuredLeases::Covered);
    }

    let owner_run_id = request.actor.run_id().to_string();
    // One session decides at a time, so two of its own admissions racing on the
    // same path cannot both read "free" and both try to take it.
    let _admission = request.coordinator.admission_guard().await;
    // Custody for the whole decision, not just its result: the previous
    // admission's release runs on its own task, and without a hold in place it
    // could hand back the very lease this call is about to reuse.
    let hold = request.coordinator.hold();
    let activity = service.lease_activity();
    let started = Instant::now();
    let mut released_own_read_lease = false;
    loop {
        // Subscribe before looking, so a release that lands between the check
        // and the wait still wakes this task.
        let notified = activity.notified();
        tokio::pin!(notified);
        notified.as_mut().enable();

        let leases = service.active_leases().await?;
        if covers_all(&leases, &state_paths, &owner_run_id) {
            // Custody, not just coverage: a background process started by this
            // admission has to keep a runtime-acquired lease alive past the
            // turn. Leases the root granted by hand are not the runtime's to
            // keep or release.
            return Ok(if request.coordinator.is_tracking().await {
                EnsuredLeases::Acquired(hold)
            } else {
                EnsuredLeases::Covered
            });
        }
        // A read lease this actor took earlier overlaps the write it now wants,
        // and the store does not exempt an owner from its own conflicts.
        if !released_own_read_lease
            && release_own_read_leases(service, &leases, &state_paths, &owner_run_id).await?
        {
            released_own_read_lease = true;
            continue;
        }
        match service
            .acquire_subagent_leases(
                request.actor,
                &state_paths,
                Some(request.environment_id),
                request.ttl,
            )
            .await
        {
            Ok(acquired) => {
                request
                    .coordinator
                    .track(service, &acquired, request.ttl)
                    .await;
                return Ok(EnsuredLeases::Acquired(hold));
            }
            Err(OwnershipError::Conflict { .. }) => {}
            Err(error) => return Err(error),
        }

        let waited = started.elapsed();
        if waited >= request.wait {
            return Err(wait_timeout_error(
                &normalized,
                &state_paths,
                &leases,
                &owner_run_id,
                waited,
            ));
        }
        let remaining = request.wait.saturating_sub(waited);
        let delay = retry_delay().min(remaining);
        let cancelled = async {
            match request.cancel {
                Some(cancel) => cancel.cancelled().await,
                None => std::future::pending().await,
            }
        };
        tokio::select! {
            () = notified => {}
            () = tokio::time::sleep(delay) => {}
            () = cancelled => {
                return Err(OwnershipError::InvalidRequest {
                    message: "workspace lease wait was cancelled".to_string(),
                });
            }
        }
    }
}

fn covers_all(
    leases: &[WorkflowPathLease],
    state_paths: &[codex_state::WorkflowLeasePath],
    owner_run_id: &str,
) -> bool {
    let owned = select_actor_leases(leases, state_paths, owner_run_id);
    state_paths.iter().all(|path| {
        owned.iter().any(|lease| {
            lease.mode == WorkflowLeaseMode::Write && state_path_covers(&lease.path, path)
        })
    })
}

/// Release this actor's own overlapping read leases so it can take a write one.
/// Returns whether anything was released.
async fn release_own_read_leases(
    service: &Arc<WorkspaceOwnershipService>,
    leases: &[WorkflowPathLease],
    state_paths: &[codex_state::WorkflowLeasePath],
    owner_run_id: &str,
) -> Result<bool, OwnershipError> {
    let blocking = leases
        .iter()
        .filter(|lease| {
            lease.state == WorkflowLeaseState::Active
                && lease.owner_run_id == owner_run_id
                && lease.mode == WorkflowLeaseMode::Read
                && state_paths
                    .iter()
                    .any(|path| state_path_overlaps(&lease.path, path))
        })
        .collect::<Vec<_>>();
    if blocking.is_empty() {
        return Ok(false);
    }
    for lease in blocking {
        if let Err(error) = service
            .release_runtime_lease(WorkflowLeaseReleaseRequest {
                lease_id: lease.lease_id.clone(),
                token: lease.token.clone(),
                generation: lease.generation,
            })
            .await
        {
            debug!(
                lease_id = %lease.lease_id,
                "could not upgrade this agent's own read lease: {error}"
            );
        }
    }
    Ok(true)
}

fn wait_timeout_error(
    normalized: &[super::NormalizedLeasePath],
    state_paths: &[codex_state::WorkflowLeasePath],
    leases: &[WorkflowPathLease],
    owner_run_id: &str,
    waited: Duration,
) -> OwnershipError {
    let mut owners = leases
        .iter()
        .filter(|lease| lease.owner_run_id != owner_run_id)
        .filter(|lease| {
            state_paths
                .iter()
                .any(|path| state_path_overlaps(&lease.path, path))
        })
        .map(|lease| lease.owner_run_id.clone())
        .collect::<Vec<_>>();
    owners.sort();
    owners.dedup();
    let path = normalized
        .first()
        .map(|path| path.display_path().to_path_buf())
        .unwrap_or_default();
    OwnershipError::LeaseWaitTimeout {
        path,
        waited_ms: u64::try_from(waited.as_millis()).unwrap_or(u64::MAX),
        owners: if owners.is_empty() {
            "another agent".to_string()
        } else {
            owners.join(", ")
        },
    }
}

fn retry_delay() -> Duration {
    let span = MAX_RETRY_DELAY.as_millis() as u64 - MIN_RETRY_DELAY.as_millis() as u64;
    MIN_RETRY_DELAY + Duration::from_millis(rand::rng().random_range(0..=span))
}
