//! FORK: runtime custody of the path leases this session acquires for itself.
//!
//! Enforcement alone is a deadlock: a subagent that needs a write lease has no
//! way to obtain one, and a lease that is obtained is never renewed or given
//! back. The coordinator closes that loop. It tracks only leases the *runtime*
//! took (self-acquired subagent leases and root override leases); a lease the
//! root granted by hand stays the root's to release.
//!
//! Custody is reference counted through [`LeaseHold`]. A turn takes one for its
//! whole lifetime and a long-running process takes another, so a background
//! build keeps its lease past the turn that started it. When the last hold
//! drops, the fences are released and every waiting sibling is woken; if the
//! process dies first, the TTL is the backstop.

use super::WorkspaceOwnershipService;
use codex_state::WorkflowLeaseExtendRequest;
use codex_state::WorkflowLeaseReleaseRequest;
use codex_state::WorkflowPathLease;
use std::sync::Arc;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;
use std::time::Duration;
use tokio::sync::Mutex;
use tokio::sync::OwnedSemaphorePermit;
use tokio::sync::Semaphore;
use tokio::task::JoinHandle;
use tracing::debug;
use tracing::warn;

/// Never renew faster than this, whatever the configured TTL works out to.
const MIN_RENEW_INTERVAL: Duration = Duration::from_secs(1);

/// Fencing identity of one runtime-held lease.
#[derive(Clone, Debug, PartialEq, Eq)]
struct TrackedLease {
    lease_id: String,
    token: String,
    generation: i64,
}

impl From<&WorkflowPathLease> for TrackedLease {
    fn from(lease: &WorkflowPathLease) -> Self {
        Self {
            lease_id: lease.lease_id.clone(),
            token: lease.token.clone(),
            generation: lease.generation,
        }
    }
}

#[derive(Default)]
struct CoordinatorState {
    service: Option<Arc<WorkspaceOwnershipService>>,
    leases: Vec<TrackedLease>,
    renew: Option<JoinHandle<()>>,
    ttl: Duration,
}

struct CoordinatorInner {
    state: Mutex<CoordinatorState>,
    holds: AtomicUsize,
    /// Serializes acquire decisions for this session so two admissions racing
    /// on the same path cannot both read "free" and both try to take it. A
    /// permit rather than a mutex guard: it is deliberately held across the
    /// bounded wait for a sibling.
    admission: Arc<Semaphore>,
}

/// Session-scoped custody of runtime-acquired path leases.
#[derive(Clone)]
pub(crate) struct LeaseCoordinator {
    inner: Arc<CoordinatorInner>,
}

impl Default for LeaseCoordinator {
    fn default() -> Self {
        Self {
            inner: Arc::new(CoordinatorInner {
                state: Mutex::new(CoordinatorState::default()),
                holds: AtomicUsize::new(0),
                admission: Arc::new(Semaphore::new(1)),
            }),
        }
    }
}

impl LeaseCoordinator {
    /// Take custody for as long as the returned hold lives.
    ///
    /// Cloning the hold shares it; the leases are released only when every
    /// clone of every outstanding hold is gone.
    pub(crate) fn hold(&self) -> LeaseHold {
        LeaseHold::new(Arc::clone(&self.inner))
    }

    /// Serialize one session's admission decisions.
    pub(crate) async fn admission_guard(&self) -> Option<OwnedSemaphorePermit> {
        Arc::clone(&self.inner.admission).acquire_owned().await.ok()
    }

    /// Track freshly acquired fences and start renewing them.
    pub(crate) async fn track(
        &self,
        service: &Arc<WorkspaceOwnershipService>,
        leases: &[WorkflowPathLease],
        ttl: Duration,
    ) {
        if leases.is_empty() {
            return;
        }
        let inner = Arc::clone(&self.inner);
        let mut state = inner.state.lock().await;
        state.service = Some(Arc::clone(service));
        state.ttl = ttl;
        for lease in leases {
            let tracked = TrackedLease::from(lease);
            if !state
                .leases
                .iter()
                .any(|existing| existing.lease_id == tracked.lease_id)
            {
                state.leases.push(tracked);
            }
        }
        if state.renew.is_none() {
            let renew_inner = Arc::clone(&inner);
            let interval = renew_interval(ttl);
            state.renew = Some(tokio::spawn(async move {
                renew_loop(renew_inner, interval).await;
            }));
        }
    }

    /// Whether the runtime already has custody of leases for this session.
    pub(crate) async fn is_tracking(&self) -> bool {
        !self.inner.state.lock().await.leases.is_empty()
    }

    /// Whether this session currently holds any runtime-acquired lease.
    #[cfg(test)]
    pub(crate) async fn tracked_lease_ids(&self) -> Vec<String> {
        self.inner
            .state
            .lock()
            .await
            .leases
            .iter()
            .map(|lease| lease.lease_id.clone())
            .collect()
    }
}

fn renew_interval(ttl: Duration) -> Duration {
    (ttl / 3).max(MIN_RENEW_INTERVAL)
}

async fn renew_loop(inner: Arc<CoordinatorInner>, interval: Duration) {
    loop {
        tokio::time::sleep(interval).await;
        let (service, leases, ttl) = {
            let state = inner.state.lock().await;
            match state.service.as_ref() {
                Some(service) if !state.leases.is_empty() => {
                    (Arc::clone(service), state.leases.clone(), state.ttl)
                }
                _ => return,
            }
        };
        let extend_duration_ms = i64::try_from(ttl.as_millis()).unwrap_or(i64::MAX);
        let requests = leases
            .iter()
            .map(|lease| WorkflowLeaseExtendRequest {
                lease_id: lease.lease_id.clone(),
                token: lease.token.clone(),
                generation: lease.generation,
                extend_duration_ms,
            })
            .collect::<Vec<_>>();
        if service.renew_agent_ownership(&requests).await.is_ok() {
            continue;
        }
        // The batch is all-or-nothing, so one dead fence hides the rest. Renew
        // them one at a time to keep the live ones and forget the dead one.
        let mut surviving = Vec::with_capacity(requests.len());
        for request in requests {
            match service
                .renew_agent_ownership(std::slice::from_ref(&request))
                .await
            {
                Ok(_) => surviving.push(request.lease_id),
                Err(error) => {
                    warn!(
                        lease_id = %request.lease_id,
                        "dropping workspace lease whose fence no longer renews: {error}"
                    );
                }
            }
        }
        let mut state = inner.state.lock().await;
        state
            .leases
            .retain(|lease| surviving.contains(&lease.lease_id));
        if state.leases.is_empty() {
            state.renew = None;
            return;
        }
    }
}

/// Reference-counted custody of this session's runtime-acquired leases.
#[derive(Clone)]
pub(crate) struct LeaseHold {
    _inner: Arc<HoldInner>,
}

impl LeaseHold {
    fn new(coordinator: Arc<CoordinatorInner>) -> Self {
        coordinator.holds.fetch_add(1, Ordering::SeqCst);
        Self {
            _inner: Arc::new(HoldInner { coordinator }),
        }
    }
}

impl std::fmt::Debug for LeaseHold {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.debug_struct("LeaseHold").finish_non_exhaustive()
    }
}

struct HoldInner {
    coordinator: Arc<CoordinatorInner>,
}

impl Drop for HoldInner {
    fn drop(&mut self) {
        if self.coordinator.holds.fetch_sub(1, Ordering::SeqCst) != 1 {
            return;
        }
        let coordinator = Arc::clone(&self.coordinator);
        // Drop also runs when a turn is aborted, which never reaches the normal
        // completion path; releasing here is what covers that case.
        let Ok(runtime) = tokio::runtime::Handle::try_current() else {
            debug!("no runtime to release workspace leases on; falling back to their TTL");
            return;
        };
        runtime.spawn(async move { release_all(coordinator).await });
    }
}

async fn release_all(inner: Arc<CoordinatorInner>) {
    let (service, leases, renew) = {
        let mut state = inner.state.lock().await;
        if state.leases.is_empty() {
            return;
        }
        // A hold taken again while this task was queued means the session is
        // still working; leave its leases alone.
        if inner.holds.load(Ordering::SeqCst) > 0 {
            return;
        }
        (
            state.service.clone(),
            std::mem::take(&mut state.leases),
            state.renew.take(),
        )
    };
    if let Some(renew) = renew {
        renew.abort();
    }
    let Some(service) = service else {
        return;
    };
    for lease in leases {
        if let Err(error) = service
            .release_runtime_lease(WorkflowLeaseReleaseRequest {
                lease_id: lease.lease_id.clone(),
                token: lease.token,
                generation: lease.generation,
            })
            .await
        {
            debug!(
                lease_id = %lease.lease_id,
                "workspace lease release failed; its TTL will reclaim it: {error}"
            );
        }
    }
    service.notify_lease_activity();
}
