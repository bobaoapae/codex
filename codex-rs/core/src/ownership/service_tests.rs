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
    let store = WorkflowStore::open(&SqliteConfig::new_for_testing(
        AbsolutePathBuf::from_absolute_path(home.path()).expect("absolute home"),
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
