use super::EnsureLeaseRequest;
use super::EnsuredLeases;
use super::LeaseCoordinator;
use super::WorkspaceOwnershipService;
use super::ensure_subagent_write_leases;
use crate::ownership::AuthorizedWorkspaceRoots;
use crate::ownership::OwnershipActor;
use crate::ownership::OwnershipError;
use codex_agent_roles::capabilities_for_canonical_role;
use codex_protocol::ThreadId;
use codex_state::SqliteConfig;
use codex_state::WorkflowLeaseMode;
use codex_state::WorkflowLeaseReleaseRequest;
use codex_state::WorkflowStore;
use codex_utils_absolute_path::AbsolutePathBuf;
use pretty_assertions::assert_eq;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use tempfile::TempDir;
use tokio_util::sync::CancellationToken;

struct Harness {
    _home: TempDir,
    service: Arc<WorkspaceOwnershipService>,
    coordinator: LeaseCoordinator,
    root: PathBuf,
    scratch: PathBuf,
}

async fn harness() -> Harness {
    let home = tempfile::tempdir().expect("temporary home");
    let root = home.path().join("workspace");
    let scratch = home.path().join("visualizations").join("thread-1");
    std::fs::create_dir_all(root.join("src")).expect("workspace source directory");
    std::fs::create_dir_all(root.join("docs")).expect("workspace docs directory");
    std::fs::create_dir_all(&scratch).expect("scratch directory");
    let state = home.path().join("state");
    std::fs::create_dir_all(&state).expect("state directory");
    let store = WorkflowStore::open(&SqliteConfig::new_for_testing(
        AbsolutePathBuf::from_absolute_path(&state).expect("absolute state home"),
    ))
    .await
    .expect("workflow store");
    let authorized_roots = AuthorizedWorkspaceRoots::new([root.clone(), scratch.clone()])
        .expect("authorized roots")
        .with_lease_exempt_roots([scratch.clone()]);
    Harness {
        _home: home,
        service: Arc::new(WorkspaceOwnershipService::new(
            store,
            ThreadId::new(),
            authorized_roots,
        )),
        coordinator: LeaseCoordinator::default(),
        root,
        scratch,
    }
}

fn editor() -> OwnershipActor {
    OwnershipActor::subagent(
        ThreadId::new(),
        capabilities_for_canonical_role("executor_luna"),
    )
}

fn request<'a>(
    harness: &'a Harness,
    actor: OwnershipActor,
    paths: &'a [PathBuf],
    wait: Duration,
    cancel: Option<&'a CancellationToken>,
) -> EnsureLeaseRequest<'a> {
    EnsureLeaseRequest {
        service: &harness.service,
        coordinator: &harness.coordinator,
        actor,
        paths,
        environment_id: "local",
        ttl: Duration::from_secs(60),
        wait,
        auto_acquire: true,
        cancel,
    }
}

#[tokio::test]
async fn a_free_path_is_acquired_and_tracked_for_renewal() {
    let harness = harness().await;
    let actor = editor();
    let paths = vec![harness.root.join("src")];
    let ensured = ensure_subagent_write_leases(request(
        &harness,
        actor,
        &paths,
        Duration::from_secs(1),
        None,
    ))
    .await
    .expect("a free path is acquired on demand");
    assert!(matches!(ensured, EnsuredLeases::Acquired(_)));
    assert_eq!(harness.coordinator.tracked_lease_ids().await.len(), 1);

    // Asking again reuses the lease it already holds rather than conflicting
    // with itself, and still hands back custody so a background process started
    // by this second command keeps the lease alive.
    let again = ensure_subagent_write_leases(request(
        &harness,
        actor,
        &paths,
        Duration::from_secs(1),
        None,
    ))
    .await
    .expect("an actor's own lease covers the second request");
    assert!(matches!(again, EnsuredLeases::Acquired(_)));
    assert_eq!(harness.coordinator.tracked_lease_ids().await.len(), 1);
}

#[tokio::test]
async fn scratch_paths_never_need_a_lease() {
    let harness = harness().await;
    let paths = vec![harness.scratch.join("chart.html")];
    let ensured = ensure_subagent_write_leases(request(
        &harness,
        editor(),
        &paths,
        Duration::from_secs(1),
        None,
    ))
    .await
    .expect("a lease-exempt path is admitted without a lease");
    assert!(ensured.is_exempt());
    assert!(harness.coordinator.tracked_lease_ids().await.is_empty());
}

#[tokio::test]
async fn a_waiter_proceeds_as_soon_as_the_holder_releases() {
    let harness = harness().await;
    let holder = editor();
    let paths = vec![harness.root.join("src")];
    let held = ensure_subagent_write_leases(request(
        &harness,
        holder,
        &paths,
        Duration::from_secs(1),
        None,
    ))
    .await
    .expect("the first agent takes the path");
    assert!(matches!(held, EnsuredLeases::Acquired(_)));
    let held_lease = harness
        .service
        .active_leases()
        .await
        .expect("active leases")
        .remove(0);

    let service = Arc::clone(&harness.service);
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(150)).await;
        service
            .release_runtime_lease(WorkflowLeaseReleaseRequest {
                lease_id: held_lease.lease_id,
                token: held_lease.token,
                generation: held_lease.generation,
            })
            .await
            .expect("the holder finishes and releases");
    });

    let waiter = editor();
    let ensured = ensure_subagent_write_leases(request(
        &harness,
        waiter,
        &paths,
        Duration::from_secs(10),
        None,
    ))
    .await
    .expect("the sibling waits for the release instead of failing");
    assert!(matches!(ensured, EnsuredLeases::Acquired(_)));
}

#[tokio::test]
async fn a_timed_out_wait_names_the_holder_and_the_time_spent() {
    let harness = harness().await;
    let paths = vec![harness.root.join("src")];
    let holder = editor();
    // Bound, not dropped: releasing custody would hand the path straight back.
    let _held = ensure_subagent_write_leases(request(
        &harness,
        holder,
        &paths,
        Duration::from_secs(1),
        None,
    ))
    .await
    .expect("the first agent takes the path");

    let error = ensure_subagent_write_leases(request(
        &harness,
        editor(),
        &paths,
        Duration::from_millis(400),
        None,
    ))
    .await
    .expect_err("the bounded wait eventually gives up");
    let OwnershipError::LeaseWaitTimeout { owners, .. } = &error else {
        panic!("expected a wait timeout, got {error:?}");
    };
    assert!(owners.contains(&holder.run_id().to_string()), "{owners}");
    assert!(
        super::describe_ownership_error(error).contains("retry shortly"),
        "the message must tell the agent to retry rather than ask for a grant"
    );
}

#[tokio::test]
async fn a_cancelled_turn_stops_waiting_immediately() {
    let harness = harness().await;
    let paths = vec![harness.root.join("src")];
    let _held = ensure_subagent_write_leases(request(
        &harness,
        editor(),
        &paths,
        Duration::from_secs(1),
        None,
    ))
    .await
    .expect("the first agent takes the path");

    let cancel = CancellationToken::new();
    let cancel_for_task = cancel.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(50)).await;
        cancel_for_task.cancel();
    });
    let error = ensure_subagent_write_leases(request(
        &harness,
        editor(),
        &paths,
        Duration::from_secs(30),
        Some(&cancel),
    ))
    .await
    .expect_err("a cancelled turn must not keep waiting");
    assert!(matches!(error, OwnershipError::InvalidRequest { .. }));
}

#[tokio::test]
async fn an_actor_upgrades_its_own_read_lease() {
    let harness = harness().await;
    let actor = editor();
    let normalized = harness
        .service
        .authorized_roots()
        .normalize(harness.root.join("src"))
        .expect("path normalizes");
    let state_path = super::service_helpers::state_path(&normalized).expect("state path");
    harness
        .service
        .workflow
        .acquire_path_leases(&codex_state::WorkflowLeaseAcquireRequest {
            root_run_id: harness.service.root_run_id().to_string(),
            owner_run_id: actor.run_id().to_string(),
            environment_id: None,
            paths: vec![state_path],
            mode: WorkflowLeaseMode::Read,
            lease_duration_ms: 60_000,
            authority: codex_state::WorkflowLeaseAuthority::Owner,
        })
        .await
        .expect("seed the actor's own read lease");

    let paths = vec![harness.root.join("src")];
    let ensured = ensure_subagent_write_leases(request(
        &harness,
        actor,
        &paths,
        Duration::from_millis(500),
        None,
    ))
    .await
    .expect("an actor's own read lease must not block its own write");
    assert!(matches!(ensured, EnsuredLeases::Acquired(_)));
}

#[tokio::test]
async fn dropping_the_last_hold_releases_every_tracked_lease() {
    let harness = harness().await;
    let paths = vec![harness.root.join("src")];
    let ensured = ensure_subagent_write_leases(request(
        &harness,
        editor(),
        &paths,
        Duration::from_secs(1),
        None,
    ))
    .await
    .expect("acquire a lease to release");
    // A second hold stands in for a background process that outlives the turn.
    let process_hold = ensured.hold().expect("an acquired admission has a hold");
    drop(ensured);
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert_eq!(
        harness
            .service
            .active_leases()
            .await
            .expect("active leases")
            .len(),
        1,
        "the lease outlives the turn while a process still holds it"
    );

    drop(process_hold);
    for _ in 0..50 {
        tokio::time::sleep(Duration::from_millis(20)).await;
        if harness
            .service
            .active_leases()
            .await
            .expect("active leases")
            .is_empty()
        {
            return;
        }
    }
    panic!("the last hold dropping must release the lease");
}

#[tokio::test]
async fn auto_acquire_off_leaves_admission_to_fail_closed() {
    let harness = harness().await;
    let paths = vec![harness.root.join("src")];
    let ensured = ensure_subagent_write_leases(EnsureLeaseRequest {
        auto_acquire: false,
        ..request(&harness, editor(), &paths, Duration::from_secs(1), None)
    })
    .await
    .expect("switching auto-acquisition off is not an error by itself");
    assert!(matches!(ensured, EnsuredLeases::Covered));
    assert!(
        harness
            .service
            .active_leases()
            .await
            .expect("active leases")
            .is_empty()
    );
}

/// FORK: a lease that is not renewed dies mid-turn.
///
/// The TTL is short enough that a long build would outlive it, and the moment
/// it expires another agent can take the same paths — two writers, no owner.
#[tokio::test]
async fn the_coordinator_renews_a_lease_before_it_expires() {
    let harness = harness().await;
    let paths = vec![harness.root.join("src")];
    // TTL/3 is the renew interval, so this renews about once a second.
    let _held = ensure_subagent_write_leases(EnsureLeaseRequest {
        ttl: Duration::from_secs(3),
        ..request(&harness, editor(), &paths, Duration::from_secs(1), None)
    })
    .await
    .expect("acquire a short-lived lease");
    let issued = harness
        .service
        .active_leases()
        .await
        .expect("active leases")
        .remove(0);

    tokio::time::sleep(Duration::from_millis(1_500)).await;
    let renewed = harness
        .service
        .active_leases()
        .await
        .expect("active leases");
    assert_eq!(renewed.len(), 1, "the lease must not have expired");
    assert!(
        renewed[0].expires_at_ms > issued.expires_at_ms,
        "the renewal loop must push the expiry out: {:?} -> {:?}",
        issued.expires_at_ms,
        renewed[0].expires_at_ms
    );
}
