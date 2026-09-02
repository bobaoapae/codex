use super::*;
use crate::migrations::WORKFLOW_MIGRATOR;
use crate::runtime::test_support::unique_temp_dir;
use codex_utils_absolute_path::AbsolutePathBuf;
use pretty_assertions::assert_eq;
use sqlx::migrate::Migrator;
use std::borrow::Cow;
use std::path::Path;

fn sqlite_config(home: &Path) -> crate::SqliteConfig {
    crate::SqliteConfig::new_for_testing(
        AbsolutePathBuf::from_absolute_path(home).expect("temporary home is absolute"),
    )
}

fn path(display: &str, comparison_key: &str) -> WorkflowLeasePath {
    WorkflowLeasePath::new(display, comparison_key).expect("normalized absolute path")
}

fn acquire(
    root_run_id: &str,
    owner_run_id: &str,
    mode: WorkflowLeaseMode,
    paths: Vec<WorkflowLeasePath>,
) -> WorkflowLeaseAcquireRequest {
    WorkflowLeaseAcquireRequest {
        root_run_id: root_run_id.to_string(),
        owner_run_id: owner_run_id.to_string(),
        environment_id: None,
        paths,
        mode,
        lease_duration_ms: 60_000,
        authority: WorkflowLeaseAuthority::Owner,
    }
}

fn migration_through(version: i64) -> Migrator {
    Migrator {
        migrations: Cow::Owned(
            WORKFLOW_MIGRATOR
                .migrations
                .iter()
                .filter(|migration| migration.version <= version)
                .cloned()
                .collect(),
        ),
        ignore_missing: WORKFLOW_MIGRATOR.ignore_missing,
        locking: WORKFLOW_MIGRATOR.locking,
        table_name: WORKFLOW_MIGRATOR.table_name.clone(),
        create_schemas: WORKFLOW_MIGRATOR.create_schemas.clone(),
        no_tx: WORKFLOW_MIGRATOR.no_tx,
    }
}

#[tokio::test]
async fn path_leases_are_component_aware_and_allow_read_sharing() {
    let home = unique_temp_dir();
    let store = WorkflowStore::open(&sqlite_config(&home))
        .await
        .expect("open workflow store");
    let root = "root-paths";
    let read_path = path("C:\\Repo\\Src", "c:/repo/src");
    let first = store
        .acquire_path_leases(&acquire(
            root,
            "reader-a",
            WorkflowLeaseMode::Read,
            vec![read_path.clone()],
        ))
        .await
        .expect("acquire read lease");
    let second = store
        .acquire_path_leases(&acquire(
            root,
            "reader-b",
            WorkflowLeaseMode::Read,
            vec![path("c:/repo/src/file", "c:/repo/src/file")],
        ))
        .await
        .expect("read/read leases coexist");
    assert_eq!(first.len(), 1);
    assert_eq!(second.len(), 1);

    let conflict = store
        .acquire_path_leases(&acquire(
            root,
            "writer-a",
            WorkflowLeaseMode::Write,
            vec![path("c:/repo/src/file", "c:/repo/src/file")],
        ))
        .await
        .expect_err("write must conflict with an ancestor read");
    assert!(matches!(
        conflict.downcast_ref::<WorkflowLeaseError>(),
        Some(WorkflowLeaseError::Conflict { conflicts }) if conflicts.len() == 2
    ));

    let unrelated = store
        .acquire_path_leases(&acquire(
            root,
            "writer-b",
            WorkflowLeaseMode::Write,
            vec![path("C:\\Repo\\Src2", "c:/repo/src2")],
        ))
        .await
        .expect("component boundary must not conflict");
    assert_eq!(unrelated.len(), 1);
    store.close().await;
}

#[tokio::test]
async fn multipath_acquisition_is_sorted_deduplicated_and_atomic() {
    let home = unique_temp_dir();
    let store = WorkflowStore::open(&sqlite_config(&home))
        .await
        .expect("open workflow store");
    let leases = store
        .acquire_path_leases(&acquire(
            "root-multipath",
            "owner-a",
            WorkflowLeaseMode::Read,
            vec![
                path("/workspace/z", "/workspace/z"),
                path("/workspace/A", "/workspace/a"),
                path("/workspace/a", "/workspace/a"),
            ],
        ))
        .await
        .expect("acquire normalized multipath lease");
    assert_eq!(
        leases
            .iter()
            .map(|lease| lease.path.comparison_key.as_str())
            .collect::<Vec<_>>(),
        ["/workspace/a", "/workspace/z"]
    );

    store
        .acquire_path_leases(&acquire(
            "root-multipath",
            "owner-write",
            WorkflowLeaseMode::Write,
            vec![path("/workspace/conflict", "/workspace/conflict")],
        ))
        .await
        .expect("acquire independent write lease");
    let mut request = acquire(
        "root-multipath",
        "owner-b",
        WorkflowLeaseMode::Write,
        vec![
            path("/workspace/good", "/workspace/good"),
            path("/workspace/conflict/child", "/workspace/conflict/child"),
        ],
    );
    let conflict = store
        .acquire_path_leases(&request)
        .await
        .expect_err("one conflict must reject the complete set");
    assert!(matches!(
        conflict.downcast_ref::<WorkflowLeaseError>(),
        Some(WorkflowLeaseError::Conflict { conflicts }) if conflicts.len() == 1
    ));
    request.paths = vec![path("/workspace/good", "/workspace/good")];
    assert_eq!(
        store
            .acquire_path_leases(&request)
            .await
            .expect("good path remains unclaimed")
            .len(),
        1
    );
    store.close().await;
}

#[tokio::test]
async fn leases_are_fenced_released_idempotently_and_expire_recoverably() {
    let home = unique_temp_dir();
    let sqlite = sqlite_config(&home);
    let store = WorkflowStore::open(&sqlite)
        .await
        .expect("open workflow store");
    let lease = store
        .acquire_path_leases(&acquire(
            "root-lifecycle",
            "owner-a",
            WorkflowLeaseMode::Write,
            vec![path("/workspace/lifecycle", "/workspace/lifecycle")],
        ))
        .await
        .unwrap()
        .pop()
        .unwrap();
    let release = WorkflowLeaseReleaseRequest {
        lease_id: lease.lease_id.clone(),
        token: lease.token.clone(),
        generation: lease.generation,
    };
    let released = store
        .release_path_lease(&release)
        .await
        .expect("release lease");
    assert_eq!(released.state, WorkflowLeaseState::Released);
    assert_eq!(store.release_path_lease(&release).await.unwrap(), released);

    let expiring = store
        .acquire_path_leases(&acquire(
            "root-lifecycle",
            "owner-b",
            WorkflowLeaseMode::Write,
            vec![path("/workspace/expiring", "/workspace/expiring")],
        ))
        .await
        .unwrap()
        .pop()
        .unwrap();
    let expired = store
        .expire_path_leases(expiring.expires_at_ms.unwrap())
        .await
        .expect("expire active lease");
    assert_eq!(expired.len(), 1);
    assert_eq!(expired[0].state, WorkflowLeaseState::Recoverable);
    assert_eq!(expired[0].generation, expiring.generation + 1);
    let stale = store
        .release_path_lease(&WorkflowLeaseReleaseRequest {
            lease_id: expiring.lease_id.clone(),
            token: expiring.token.clone(),
            generation: expiring.generation,
        })
        .await
        .expect_err("expired lease fence must be stale");
    assert!(matches!(
        stale.downcast_ref::<WorkflowLeaseError>(),
        Some(WorkflowLeaseError::Stale { lease_id }) if lease_id == &expiring.lease_id
    ));
    store.close().await;

    let reopened = WorkflowStore::open(&sqlite)
        .await
        .expect("reopen workflow store");
    assert_eq!(
        reopened
            .get_path_lease(&expiring.lease_id)
            .await
            .unwrap()
            .unwrap()
            .state,
        WorkflowLeaseState::Recoverable
    );
    reopened.close().await;
}

#[tokio::test]
async fn root_override_is_exact_one_shot_and_atomic() {
    let home = unique_temp_dir();
    let sqlite = sqlite_config(&home);
    let mut store = WorkflowStore::open(&sqlite)
        .await
        .expect("open workflow store");
    let conflict_path = path("/workspace/shared", "/workspace/shared");
    let existing = store
        .acquire_path_leases(&acquire(
            "root-override",
            "owner-existing",
            WorkflowLeaseMode::Write,
            vec![conflict_path.clone()],
        ))
        .await
        .unwrap();
    let override_record = store
        .issue_path_lease_override(&WorkflowLeaseOverrideCreate {
            root_run_id: "root-override".to_string(),
            paths: vec![conflict_path.clone()],
            conflict_owner_run_ids: vec!["owner-existing".to_string()],
            operation_digest: "digest-1".to_string(),
            reason: "root emergency operation".to_string(),
            receipt_id: "receipt-1".to_string(),
        })
        .await
        .expect("issue root override");
    store.close().await;
    store = WorkflowStore::open(&sqlite)
        .await
        .expect("reopen workflow store before consuming override");
    let mut request = acquire(
        "root-override",
        "owner-root",
        WorkflowLeaseMode::Write,
        vec![conflict_path.clone()],
    );
    request.authority = WorkflowLeaseAuthority::RootOverride(WorkflowLeaseOverrideUse {
        override_id: override_record.override_id.clone(),
        token: override_record.token.clone(),
        generation: override_record.generation,
        operation_digest: override_record.operation_digest.clone(),
        paths: override_record.paths.clone(),
        conflict_owner_run_ids: override_record.conflict_owner_run_ids.clone(),
    });
    let acquired = store
        .acquire_path_leases(&request)
        .await
        .expect("consume exact root override");
    assert_eq!(
        acquired[0].override_receipt_id.as_deref(),
        Some("receipt-1")
    );
    let consumed = store
        .get_path_lease_override(&override_record.override_id)
        .await
        .unwrap()
        .unwrap();
    assert!(consumed.consumed_at_ms.is_some());

    let reused = store
        .acquire_path_leases(&request)
        .await
        .expect_err("override is one-shot");
    assert!(matches!(
        reused.downcast_ref::<WorkflowLeaseError>(),
        Some(WorkflowLeaseError::OverrideMismatch { override_id })
            if override_id == &override_record.override_id
    ));

    let expanded = store
        .issue_path_lease_override(&WorkflowLeaseOverrideCreate {
            root_run_id: "root-override".to_string(),
            paths: vec![conflict_path.clone()],
            conflict_owner_run_ids: vec!["owner-existing".to_string()],
            operation_digest: "digest-2".to_string(),
            reason: "second exact operation".to_string(),
            receipt_id: "receipt-2".to_string(),
        })
        .await
        .unwrap();
    let mut expanded_request = acquire(
        "root-override",
        "owner-root-2",
        WorkflowLeaseMode::Write,
        vec![conflict_path, path("/workspace/extra", "/workspace/extra")],
    );
    expanded_request.authority = WorkflowLeaseAuthority::RootOverride(WorkflowLeaseOverrideUse {
        override_id: expanded.override_id.clone(),
        token: expanded.token.clone(),
        generation: expanded.generation,
        operation_digest: expanded.operation_digest.clone(),
        paths: expanded.paths.clone(),
        conflict_owner_run_ids: expanded.conflict_owner_run_ids.clone(),
    });
    let mismatch = store
        .acquire_path_leases(&expanded_request)
        .await
        .expect_err("override cannot expand its path set");
    assert!(matches!(
        mismatch.downcast_ref::<WorkflowLeaseError>(),
        Some(WorkflowLeaseError::OverrideMismatch { override_id }) if override_id == &expanded.override_id
    ));
    assert!(
        store
            .get_path_lease_override(&expanded.override_id)
            .await
            .unwrap()
            .unwrap()
            .consumed_at_ms
            .is_none()
    );
    assert_eq!(existing.len(), 1);
    store.close().await;
}

#[tokio::test]
async fn concurrent_write_acquisition_has_one_winner() {
    let home = unique_temp_dir();
    let store = WorkflowStore::open(&sqlite_config(&home))
        .await
        .expect("open workflow store");
    let first_store = store.clone();
    let second_store = store.clone();
    let first_request = acquire(
        "root-race",
        "owner-a",
        WorkflowLeaseMode::Write,
        vec![path("/workspace/race", "/workspace/race")],
    );
    let second_request = acquire(
        "root-race",
        "owner-b",
        WorkflowLeaseMode::Write,
        vec![path("/workspace/race/child", "/workspace/race/child")],
    );
    let (first, second) = tokio::join!(
        first_store.acquire_path_leases(&first_request),
        second_store.acquire_path_leases(&second_request),
    );
    assert!(first.is_ok() ^ second.is_ok());
    store.close().await;
}

#[tokio::test]
async fn path_lease_migration_preserves_legacy_rows() {
    let home = unique_temp_dir();
    let sqlite = sqlite_config(&home);
    tokio::fs::create_dir_all(&home)
        .await
        .expect("create workflow sqlite home");
    let pool = sqlite
        .open_workflow_db(&migration_through(4), None)
        .await
        .expect("open legacy workflow schema");
    sqlx::query(
        "INSERT INTO workflow_path_leases
         (lease_id, root_run_id, owner_run_id, path, mode, generation, state,
          issued_at_ms, expires_at_ms, released_at_ms, override_receipt_id)
         VALUES ('legacy-lease', 'legacy-root', 'legacy-owner', '/legacy/path',
                 'read', 4, 'expired', 10, 20, 30, 'legacy-receipt')",
    )
    .execute(&pool)
    .await
    .expect("insert legacy path lease");
    pool.close().await;

    let store = WorkflowStore::open(&sqlite)
        .await
        .expect("migrate workflow schema");
    let lease = store.get_path_lease("legacy-lease").await.unwrap().unwrap();
    assert_eq!(lease.path.display, "/legacy/path");
    assert_eq!(lease.path.comparison_key, "/legacy/path");
    assert_eq!(lease.state, WorkflowLeaseState::Expired);
    assert_eq!(lease.generation, 4);
    assert!(!lease.token.is_empty());
    assert_eq!(lease.override_receipt_id.as_deref(), Some("legacy-receipt"));
    store.close().await;
}

#[tokio::test]
async fn extending_a_lease_is_fenced_and_all_or_nothing() {
    let home = unique_temp_dir();
    let store = WorkflowStore::open(&sqlite_config(&home))
        .await
        .expect("open workflow store");
    let root = "root-extend";
    let leases = store
        .acquire_path_leases(&acquire(
            root,
            "worker",
            WorkflowLeaseMode::Write,
            vec![
                path("c:/repo/a", "c:/repo/a"),
                path("c:/repo/b", "c:/repo/b"),
            ],
        ))
        .await
        .expect("acquire two write leases");
    assert_eq!(leases.len(), 2);
    let extend = |lease: &WorkflowPathLease, generation: i64| WorkflowLeaseExtendRequest {
        lease_id: lease.lease_id.clone(),
        token: lease.token.clone(),
        generation,
        extend_duration_ms: 3_600_000,
    };

    let extended = store
        .extend_path_leases(&[extend(&leases[0], 1), extend(&leases[1], 1)])
        .await
        .expect("extend both leases under their exact fences");
    for (before, after) in leases.iter().zip(&extended) {
        assert!(after.expires_at_ms > before.expires_at_ms);
        assert_eq!(after.generation, before.generation);
        assert_eq!(after.state, WorkflowLeaseState::Active);
    }

    // A stale fence anywhere rolls the whole batch back, so a renewer never
    // keeps a partially valid claim.
    let stale = store
        .extend_path_leases(&[extend(&leases[0], 1), extend(&leases[1], 7)])
        .await
        .expect_err("a mismatched generation is stale");
    assert!(matches!(
        stale.downcast_ref::<WorkflowLeaseError>(),
        Some(WorkflowLeaseError::Stale { lease_id }) if *lease_id == leases[1].lease_id
    ));
    let unchanged = store
        .get_path_lease(&leases[0].lease_id)
        .await
        .expect("read the first lease")
        .expect("the first lease still exists");
    assert_eq!(unchanged.expires_at_ms, extended[0].expires_at_ms);
}

#[tokio::test]
async fn extending_an_expired_lease_is_stale() {
    let home = unique_temp_dir();
    let store = WorkflowStore::open(&sqlite_config(&home))
        .await
        .expect("open workflow store");
    let root = "root-extend-expired";
    let mut request = acquire(
        root,
        "worker",
        WorkflowLeaseMode::Write,
        vec![path("c:/repo/a", "c:/repo/a")],
    );
    request.lease_duration_ms = 1;
    let leases = store
        .acquire_path_leases(&request)
        .await
        .expect("acquire a lease that expires immediately");
    tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    let error = store
        .extend_path_leases(&[WorkflowLeaseExtendRequest {
            lease_id: leases[0].lease_id.clone(),
            token: leases[0].token.clone(),
            generation: leases[0].generation,
            extend_duration_ms: 60_000,
        }])
        .await
        .expect_err("an expired lease cannot be extended");
    assert!(matches!(
        error.downcast_ref::<WorkflowLeaseError>(),
        Some(WorkflowLeaseError::Stale { .. })
    ));
}

#[tokio::test]
async fn release_by_owner_only_touches_that_owner_and_unblocks_the_path() {
    let home = unique_temp_dir();
    let store = WorkflowStore::open(&sqlite_config(&home))
        .await
        .expect("open workflow store");
    let root = "root-release-owner";
    let departing = store
        .acquire_path_leases(&acquire(
            root,
            "evicted",
            WorkflowLeaseMode::Write,
            vec![path("c:/repo/src", "c:/repo/src")],
        ))
        .await
        .expect("acquire the departing agent's lease");
    let staying = store
        .acquire_path_leases(&acquire(
            root,
            "staying",
            WorkflowLeaseMode::Write,
            vec![path("c:/repo/docs", "c:/repo/docs")],
        ))
        .await
        .expect("acquire an unrelated sibling's lease");

    let released = store
        .release_active_path_leases_for_owner(root, "evicted")
        .await
        .expect("release every active lease of the departing owner");
    assert_eq!(released.len(), 1);
    assert_eq!(released[0].lease_id, departing[0].lease_id);
    assert_eq!(released[0].state, WorkflowLeaseState::Released);
    assert_eq!(
        store
            .get_path_lease(&staying[0].lease_id)
            .await
            .expect("read the sibling lease")
            .expect("the sibling lease still exists")
            .state,
        WorkflowLeaseState::Active
    );

    // The released row is still in the table; it must no longer conflict.
    store
        .acquire_path_leases(&acquire(
            root,
            "successor",
            WorkflowLeaseMode::Write,
            vec![path("c:/repo/src", "c:/repo/src")],
        ))
        .await
        .expect("a released lease no longer blocks the path");

    assert!(
        store
            .release_active_path_leases_for_owner(root, "evicted")
            .await
            .expect("releasing twice is a no-op")
            .is_empty()
    );
}
