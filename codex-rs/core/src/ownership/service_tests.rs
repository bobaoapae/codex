use super::*;
use codex_agent_roles::capabilities_for_canonical_role;
use codex_protocol::ThreadId;
use codex_state::SqliteConfig;
use codex_state::WorkflowLeaseAcquireRequest;
use codex_state::WorkflowLeaseAuthority;
use codex_state::WorkflowLeaseMode;
use codex_state::WorkflowLeaseState;
use codex_state::WorkflowStore;
use codex_utils_absolute_path::AbsolutePathBuf;
use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::Mutex;
use std::time::Duration;
use tempfile::TempDir;

#[derive(Default)]
struct RecordingReceiptSink {
    receipts: Mutex<Vec<OwnershipOverrideReceipt>>,
}

impl OwnershipReceiptSink for RecordingReceiptSink {
    fn append_ownership_override_receipt(
        &self,
        receipt: OwnershipOverrideReceipt,
    ) -> Pin<Box<dyn Future<Output = anyhow::Result<()>> + Send + '_>> {
        self.receipts
            .lock()
            .expect("receipt sink lock")
            .push(receipt);
        Box::pin(async { Ok(()) })
    }
}

async fn make_service() -> (TempDir, WorkspaceOwnershipService, PathBuf, ThreadId) {
    let home = tempfile::tempdir().expect("temporary home");
    let root = home.path().join("workspace");
    std::fs::create_dir(&root).expect("workspace root");
    for directory in ["src", "shared", "prepared"] {
        std::fs::create_dir(root.join(directory)).expect("workspace child directory");
    }
    std::fs::write(root.join("src").join("file.rs"), "").expect("source file");
    #[cfg(unix)]
    {
        use std::os::unix::fs::symlink;

        std::fs::create_dir(root.join("first")).expect("first target");
        std::fs::create_dir(root.join("second")).expect("second target");
        symlink(root.join("first"), root.join("link")).expect("initial symlink");
    }
    // The database lives beside the workspace, not above it: every ancestor of
    // an authorized root is fingerprinted, so a SQLite write in a parent
    // directory reads as the tree changing under the guard.
    let state = home.path().join("state");
    std::fs::create_dir_all(&state).expect("state directory");
    let store = WorkflowStore::open(&SqliteConfig::new_for_testing(
        AbsolutePathBuf::from_absolute_path(&state).expect("absolute state home"),
    ))
    .await
    .expect("workflow store");
    let authorized_roots = AuthorizedWorkspaceRoots::new([root.clone()]).expect("authorized root");
    let root_run_id = ThreadId::new();
    (
        home,
        WorkspaceOwnershipService::new(store, root_run_id, authorized_roots),
        root,
        root_run_id,
    )
}

fn grant_request(
    root: ThreadId,
    target: OwnershipActor,
    paths: Vec<PathBuf>,
) -> OwnershipGrantRequest {
    OwnershipGrantRequest {
        requester: OwnershipActor::root(root),
        target,
        paths,
        mode: WorkflowLeaseMode::Write,
        lease_duration: Duration::from_secs(60),
        environment: OwnershipEnvironment::Default,
    }
}

fn mutation_request(
    actor: OwnershipActor,
    path: PathBuf,
    override_authorization: OwnershipOverrideAuthorization,
) -> MutationAuthorizationRequest {
    mutation_request_with_digest(actor, path, override_authorization, "operation-1")
}

fn mutation_request_with_digest(
    actor: OwnershipActor,
    path: PathBuf,
    override_authorization: OwnershipOverrideAuthorization,
    digest: &str,
) -> MutationAuthorizationRequest {
    MutationAuthorizationRequest {
        actor,
        paths: vec![path],
        operation: MutationOperation {
            digest: digest.to_string(),
        },
        override_authorization,
    }
}

#[tokio::test]
async fn read_only_role_cannot_mutate_even_when_a_lease_exists() {
    let (_home, service, root, root_run_id) = make_service().await;
    let reader =
        OwnershipActor::subagent(ThreadId::new(), capabilities_for_canonical_role("explorer"));
    let normalized = service
        .authorized_roots()
        .normalize(root.join("src"))
        .expect("read-only path normalizes");
    let state_path = super::service_helpers::state_path(&normalized).expect("state path");
    service
        .workflow
        .acquire_path_leases(&WorkflowLeaseAcquireRequest {
            root_run_id: root_run_id.to_string(),
            owner_run_id: reader.run_id().to_string(),
            environment_id: None,
            paths: vec![state_path],
            mode: WorkflowLeaseMode::Write,
            lease_duration_ms: 60_000,
            authority: WorkflowLeaseAuthority::Owner,
        })
        .await
        .expect("seed a write lease for the read-only check");

    let error = service
        .authorize_mutation(mutation_request(
            reader,
            root.join("src").join("file.rs"),
            OwnershipOverrideAuthorization::NotRequested,
        ))
        .await
        .expect_err("read-only role must fail closed");
    assert!(matches!(error, OwnershipError::ReadOnlyRole));
}

#[tokio::test]
async fn editor_requires_a_covering_write_lease() {
    let (_home, service, root, root_run_id) = make_service().await;
    let editor = OwnershipActor::subagent(
        ThreadId::new(),
        capabilities_for_canonical_role("executor_luna"),
    );
    let path = root.join("src").join("file.rs");
    let without_lease = service
        .authorize_mutation(mutation_request(
            editor,
            path.clone(),
            OwnershipOverrideAuthorization::NotRequested,
        ))
        .await
        .expect_err("editor without lease must fail");
    assert!(matches!(
        without_lease,
        OwnershipError::LeaseRequired { .. }
    ));

    service
        .grant_agent_ownership(grant_request(root_run_id, editor, vec![root.join("src")]))
        .await
        .expect("grant editor lease");
    let guard = service
        .authorize_mutation(mutation_request(
            editor,
            path,
            OwnershipOverrideAuthorization::NotRequested,
        ))
        .await
        .expect("covered write lease should authorize");
    assert_eq!(guard.leases().len(), 1);
}

#[tokio::test]
async fn linked_worktree_scope_requires_exclusive_actor_environment_lease() {
    let (_home, service, root, root_run_id) = make_service().await;
    let worktree = root.join("shared");
    let editor = OwnershipActor::subagent(
        ThreadId::new(),
        capabilities_for_canonical_role("executor_luna"),
    );
    let other_editor = OwnershipActor::subagent(
        ThreadId::new(),
        capabilities_for_canonical_role("executor_luna"),
    );
    let missing = service
        .authorize_mutation(mutation_request(
            editor,
            worktree.clone(),
            OwnershipOverrideAuthorization::NotRequested,
        ))
        .await
        .expect_err("a linked worktree without an assignment must fail closed");
    assert!(matches!(missing, OwnershipError::LeaseRequired { .. }));

    service
        .grant_agent_ownership(OwnershipGrantRequest {
            requester: OwnershipActor::root(root_run_id),
            target: editor,
            paths: vec![worktree.clone()],
            mode: WorkflowLeaseMode::Write,
            lease_duration: Duration::from_secs(60),
            environment: OwnershipEnvironment::Named("local".to_string()),
        })
        .await
        .expect("root assigns the linked worktree to the editor");
    let guard = service
        .authorize_mutation(mutation_request(
            editor,
            worktree.clone(),
            OwnershipOverrideAuthorization::NotRequested,
        ))
        .await
        .expect("the assigned actor receives a durable guard");
    service
        .require_full_environment_lease(&guard, std::slice::from_ref(&worktree), "local")
        .await
        .expect("the guard is bound to the selected environment");
    assert!(
        service
            .require_full_environment_lease(&guard, std::slice::from_ref(&worktree), "other")
            .await
            .is_err()
    );

    let denied = service
        .authorize_mutation(mutation_request(
            other_editor,
            worktree.clone(),
            OwnershipOverrideAuthorization::NotRequested,
        ))
        .await
        .expect_err("a second actor cannot bypass the assignment");
    assert!(matches!(denied, OwnershipError::LeaseRequired { .. }));

    service
        .workflow
        .expire_path_leases(i64::MAX)
        .await
        .expect("expire the durable worktree lease");
    assert!(service.revalidate_guard(&guard).await.is_err());
}

#[tokio::test]
async fn lease_release_requires_owner_or_root_and_exact_fence() {
    let (_home, service, root, root_run_id) = make_service().await;
    let editor =
        OwnershipActor::subagent(ThreadId::new(), capabilities_for_canonical_role("worker"));
    let lease = service
        .grant_agent_ownership(grant_request(
            root_run_id,
            editor,
            vec![root.join("shared")],
        ))
        .await
        .expect("grant lease")
        .pop()
        .expect("one lease");
    let other_editor =
        OwnershipActor::subagent(ThreadId::new(), capabilities_for_canonical_role("worker"));
    let wrong_owner = service
        .release_agent_ownership(OwnershipReleaseRequest {
            requester: other_editor,
            lease_id: lease.lease_id.clone(),
            token: lease.token.clone(),
            generation: lease.generation,
        })
        .await
        .expect_err("only owner or root may release");
    assert!(matches!(wrong_owner, OwnershipError::RootRequired));

    let stale = service
        .release_agent_ownership(OwnershipReleaseRequest {
            requester: editor,
            lease_id: lease.lease_id.clone(),
            token: "stale-token".to_string(),
            generation: lease.generation,
        })
        .await
        .expect_err("stale token must fail closed");
    assert!(matches!(stale, OwnershipError::State { .. }));

    let released = service
        .release_agent_ownership(OwnershipReleaseRequest {
            requester: editor,
            lease_id: lease.lease_id.clone(),
            token: lease.token.clone(),
            generation: lease.generation,
        })
        .await
        .expect("owner releases lease");
    assert_eq!(released.state, WorkflowLeaseState::Released);
    assert_eq!(
        service
            .release_agent_ownership(OwnershipReleaseRequest {
                requester: OwnershipActor::root(root_run_id),
                lease_id: lease.lease_id,
                token: lease.token,
                generation: lease.generation,
            })
            .await
            .expect("repeated release is idempotent"),
        released
    );
}

#[tokio::test]
async fn root_conflict_requires_receipt_backed_one_shot_override() {
    let (_home, service, root, root_run_id) = make_service().await;
    let editor =
        OwnershipActor::subagent(ThreadId::new(), capabilities_for_canonical_role("worker"));
    let path = root.join("shared");
    service
        .grant_agent_ownership(grant_request(root_run_id, editor, vec![path.clone()]))
        .await
        .expect("grant conflicting editor lease");

    let conflict = service
        .authorize_mutation(mutation_request(
            OwnershipActor::root(root_run_id),
            path.clone(),
            OwnershipOverrideAuthorization::NotRequested,
        ))
        .await
        .expect_err("root conflict must not be implicit");
    assert!(matches!(conflict, OwnershipError::Conflict { .. }));

    let sink = Arc::new(RecordingReceiptSink::default());
    let guard = service
        .authorize_mutation(mutation_request(
            OwnershipActor::root(root_run_id),
            path,
            OwnershipOverrideAuthorization::Request(OwnershipOverrideRequest {
                reason: "explicit root recovery".to_string(),
                receipt_sink: sink.clone(),
            }),
        ))
        .await
        .expect("receipt-backed root override");
    assert_eq!(guard.leases().len(), 1);
    let receipts = sink.receipts.lock().expect("receipt sink lock");
    assert_eq!(receipts.len(), 1);
    assert_eq!(receipts[0].root_run_id, root_run_id);
    assert_eq!(receipts[0].operation_digest, "operation-1");
    assert_eq!(guard.leases()[0].state, WorkflowLeaseState::Active);
}

#[tokio::test]
async fn prepared_override_is_consumed_only_for_an_exact_retry() {
    let (_home, service, root, root_run_id) = make_service().await;
    let editor =
        OwnershipActor::subagent(ThreadId::new(), capabilities_for_canonical_role("worker"));
    let path = root.join("prepared");
    service
        .grant_agent_ownership(grant_request(root_run_id, editor, vec![path.clone()]))
        .await
        .expect("grant editor lease");
    let sink = Arc::new(RecordingReceiptSink::default());
    let proof = service
        .prepare_override(
            OwnershipActor::root(root_run_id),
            vec![path.clone()],
            MutationOperation {
                digest: "operation-prepared".to_string(),
            },
            "prepare exact retry".to_string(),
            sink.clone(),
        )
        .await
        .expect("prepare override proof");
    assert_eq!(proof.operation_digest, "operation-prepared");
    assert_eq!(sink.receipts.lock().expect("receipt sink lock").len(), 1);
    let repeated = service
        .prepare_override(
            OwnershipActor::root(root_run_id),
            vec![path.clone()],
            MutationOperation {
                digest: "operation-prepared".to_string(),
            },
            "prepare exact retry".to_string(),
            sink.clone(),
        )
        .await
        .expect("repeated preparation should reuse the proof");
    assert_eq!(repeated, proof);
    assert_eq!(sink.receipts.lock().expect("receipt sink lock").len(), 1);

    let mismatch = service
        .authorize_mutation(mutation_request_with_digest(
            OwnershipActor::root(root_run_id),
            path.clone(),
            OwnershipOverrideAuthorization::NotRequested,
            "different-operation",
        ))
        .await
        .expect_err("mismatched retry must not consume proof");
    assert!(matches!(mismatch, OwnershipError::Conflict { .. }));

    let guard = service
        .authorize_mutation(mutation_request_with_digest(
            OwnershipActor::root(root_run_id),
            path.clone(),
            OwnershipOverrideAuthorization::NotRequested,
            "operation-prepared",
        ))
        .await
        .expect("exact retry consumes prepared proof");
    assert_eq!(guard.leases().len(), 1);

    let reused = service
        .authorize_mutation(mutation_request_with_digest(
            OwnershipActor::root(root_run_id),
            path,
            OwnershipOverrideAuthorization::NotRequested,
            "operation-prepared",
        ))
        .await
        .expect_err("prepared proof is one-shot");
    assert!(matches!(reused, OwnershipError::Conflict { .. }));
}

#[cfg(unix)]
#[tokio::test]
async fn mutation_guard_revalidates_symlink_changes() {
    use std::os::unix::fs::symlink;

    let (_home, service, root, root_run_id) = make_service().await;
    let link = root.join("link");
    let editor =
        OwnershipActor::subagent(ThreadId::new(), capabilities_for_canonical_role("worker"));
    service
        .grant_agent_ownership(grant_request(
            root_run_id,
            editor,
            vec![link.join("file.rs")],
        ))
        .await
        .expect("grant symlink lease");
    let guard = service
        .authorize_mutation(mutation_request(
            editor,
            link.join("file.rs"),
            OwnershipOverrideAuthorization::NotRequested,
        ))
        .await
        .expect("authorize before swap");

    std::fs::remove_file(&link).expect("remove initial link");
    symlink(root.join("second"), &link).expect("replace link");
    assert!(guard.revalidate().is_err());
}

/// FORK: the regression that stalled every root command after a child ran.
///
/// Lease rows are never deleted, so a released or expired child claim used to
/// stay in the conflict scan forever and could only be cleared with a one-shot
/// override per operation digest.
#[tokio::test]
async fn released_child_lease_no_longer_blocks_root() {
    let (_home, service, root, root_run_id) = make_service().await;
    let child = OwnershipActor::subagent(
        ThreadId::new(),
        capabilities_for_canonical_role("executor_luna"),
    );
    let leases = service
        .grant_agent_ownership(grant_request(root_run_id, child, vec![root.join("src")]))
        .await
        .expect("grant the child a write lease");

    let blocked = service
        .authorize_mutation(mutation_request(
            OwnershipActor::root(root_run_id),
            root.join("src").join("file.rs"),
            OwnershipOverrideAuthorization::NotRequested,
        ))
        .await
        .expect_err("an active child write lease blocks the root");
    assert!(matches!(blocked, OwnershipError::Conflict { .. }));

    service
        .release_agent_ownership(OwnershipReleaseRequest {
            requester: child,
            lease_id: leases[0].lease_id.clone(),
            token: leases[0].token.clone(),
            generation: leases[0].generation,
        })
        .await
        .expect("the child releases its own lease");

    service
        .authorize_mutation(mutation_request(
            OwnershipActor::root(root_run_id),
            root.join("src").join("file.rs"),
            OwnershipOverrideAuthorization::NotRequested,
        ))
        .await
        .expect("a released child lease must not block the root");
}

#[tokio::test]
async fn child_read_lease_does_not_block_root_write() {
    let (_home, service, root, root_run_id) = make_service().await;
    let child = OwnershipActor::subagent(
        ThreadId::new(),
        capabilities_for_canonical_role("executor_luna"),
    );
    service
        .grant_agent_ownership(OwnershipGrantRequest {
            mode: WorkflowLeaseMode::Read,
            ..grant_request(root_run_id, child, vec![root.join("src")])
        })
        .await
        .expect("grant the child a read lease");

    service
        .authorize_mutation(mutation_request(
            OwnershipActor::root(root_run_id),
            root.join("src").join("file.rs"),
            OwnershipOverrideAuthorization::NotRequested,
        ))
        .await
        .expect("a child read lease must not block the root");
}

/// FORK: the subtlest part of narrowing the root's blocking rule.
///
/// Once a write lease blocks, the override's conflict-owner set has to include
/// the overlapping *read* holders too: the store's own scan is mode-agnostic
/// for a write acquisition, so a proof that omits them is rejected as a
/// mismatch exactly when it is consumed.
#[tokio::test]
async fn root_override_covers_overlapping_read_and_write_holders() {
    let (_home, service, root, root_run_id) = make_service().await;
    let writer = OwnershipActor::subagent(
        ThreadId::new(),
        capabilities_for_canonical_role("executor_luna"),
    );
    let reader = OwnershipActor::subagent(
        ThreadId::new(),
        capabilities_for_canonical_role("executor_luna"),
    );
    // Disjoint children, both inside the root's requested scope.
    service
        .grant_agent_ownership(grant_request(root_run_id, writer, vec![root.join("src")]))
        .await
        .expect("grant the writer a write lease");
    service
        .grant_agent_ownership(OwnershipGrantRequest {
            mode: WorkflowLeaseMode::Read,
            ..grant_request(root_run_id, reader, vec![root.join("shared")])
        })
        .await
        .expect("grant the reader a read lease elsewhere in the tree");

    let sink: Arc<dyn OwnershipReceiptSink> = Arc::new(RecordingReceiptSink::default());
    let guard = service
        .authorize_mutation(mutation_request(
            OwnershipActor::root(root_run_id),
            root.clone(),
            OwnershipOverrideAuthorization::Request(OwnershipOverrideRequest {
                reason: "root must reclaim the checkout".to_string(),
                receipt_sink: sink,
            }),
        ))
        .await
        .expect("the override consumes cleanly with both holders in its set");
    assert!(!guard.leases().is_empty());
}

#[tokio::test]
async fn subagent_acquires_and_renews_its_own_lease() {
    let (_home, service, root, _root_run_id) = make_service().await;
    let editor = OwnershipActor::subagent(
        ThreadId::new(),
        capabilities_for_canonical_role("executor_luna"),
    );
    let normalized = service
        .authorized_roots()
        .normalize(root.join("src"))
        .expect("path normalizes");
    let state_path = super::service_helpers::state_path(&normalized).expect("state path");

    let leases = service
        .acquire_subagent_leases(
            editor,
            std::slice::from_ref(&state_path),
            Some("local"),
            Duration::from_secs(60),
        )
        .await
        .expect("a capable subagent acquires its own lease");
    // "local" is the implicit environment and must be stored as NULL: an
    // exact-equality mismatch there would let two agents write one path.
    assert_eq!(leases[0].environment_id, None);

    service
        .authorize_mutation(mutation_request(
            editor,
            root.join("src").join("file.rs"),
            OwnershipOverrideAuthorization::NotRequested,
        ))
        .await
        .expect("the self-acquired lease admits the mutation");

    let renewed = service
        .renew_agent_ownership(&[codex_state::WorkflowLeaseExtendRequest {
            lease_id: leases[0].lease_id.clone(),
            token: leases[0].token.clone(),
            generation: leases[0].generation,
            extend_duration_ms: 3_600_000,
        }])
        .await
        .expect("renewal extends the lease under its exact fence");
    assert!(renewed[0].expires_at_ms > leases[0].expires_at_ms);

    let stale = service
        .renew_agent_ownership(&[codex_state::WorkflowLeaseExtendRequest {
            lease_id: leases[0].lease_id.clone(),
            token: "not-the-token".to_string(),
            generation: leases[0].generation,
            extend_duration_ms: 3_600_000,
        }])
        .await
        .expect_err("a lost fence cannot renew");
    assert!(matches!(stale, OwnershipError::State { .. }));
}

#[tokio::test]
async fn read_only_role_cannot_acquire_its_own_lease() {
    let (_home, service, root, _root_run_id) = make_service().await;
    let reader =
        OwnershipActor::subagent(ThreadId::new(), capabilities_for_canonical_role("explorer"));
    let normalized = service
        .authorized_roots()
        .normalize(root.join("src"))
        .expect("path normalizes");
    let state_path = super::service_helpers::state_path(&normalized).expect("state path");
    let error = service
        .acquire_subagent_leases(reader, &[state_path], None, Duration::from_secs(60))
        .await
        .expect_err("self-acquisition is not a wider door for read-only roles");
    assert!(matches!(error, OwnershipError::ReadOnlyRole));
}

#[tokio::test]
async fn releasing_by_owner_clears_every_lease_an_evicted_agent_held() {
    let (_home, service, root, root_run_id) = make_service().await;
    let evicted = OwnershipActor::subagent(
        ThreadId::new(),
        capabilities_for_canonical_role("executor_luna"),
    );
    service
        .grant_agent_ownership(grant_request(
            root_run_id,
            evicted,
            vec![root.join("src"), root.join("shared")],
        ))
        .await
        .expect("grant the agent two leases");

    let released = service
        .release_leases_for_owner(evicted.run_id())
        .await
        .expect("release everything the evicted agent held");
    assert_eq!(released.len(), 2);
    assert!(
        service
            .active_leases()
            .await
            .expect("active leases")
            .is_empty()
    );
}
