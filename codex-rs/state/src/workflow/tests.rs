use super::*;
use crate::SqliteConfig;
use crate::runtime::test_support::unique_temp_dir;
use codex_utils_absolute_path::test_support::PathExt;
use pretty_assertions::assert_eq;
use serde_json::json;

fn sqlite_config() -> SqliteConfig {
    let home = unique_temp_dir();
    SqliteConfig::new_for_testing(home.as_path().abs())
}

fn run_create(run_id: &str, idempotency_key: Option<&str>) -> WorkflowRunCreate {
    WorkflowRunCreate {
        run_id: run_id.to_string(),
        thread_id: run_id.to_string(),
        root_thread_id: Some("root-thread".to_string()),
        parent_run_id: None,
        thread_class: WorkflowThreadClass::TransientJob,
        status: "pending".to_string(),
        idempotency_key: idempotency_key.map(str::to_string),
        provider: Some("openai".to_string()),
        model: Some("gpt-5".to_string()),
        cwd: Some("C:/workspace".to_string()),
        metadata: Some(json!({"kind": "test"})),
    }
}

#[tokio::test]
async fn workflow_migration_opens_reopens_and_is_exposed_by_runtime() {
    let sqlite = sqlite_config();
    let store = WorkflowStore::open(&sqlite)
        .await
        .expect("open workflow db");
    assert!(sqlite.workflow_db_path().exists());
    assert!(store.fts5_available().await.expect("check FTS5"));
    let table_names = sqlx::query_scalar::<_, String>(
        "SELECT name FROM sqlite_master WHERE type IN ('table', 'virtual table') ORDER BY name",
    )
    .fetch_all(store.pool())
    .await
    .expect("read workflow schema");
    for table in [
        "workflow_runs",
        "workflow_receipts",
        "workflow_checkpoints",
        "workflow_mailbox",
        "workflow_path_leases",
        "workflow_backfill_state",
        "workflow_backfill_journal",
        "workflow_search_generations",
        "workflow_search_state",
        "workflow_search_documents",
        "workflow_search_fts",
        "workflow_search_live_state",
        "workflow_search_live_documents",
        "workflow_search_live_fts",
        "workflow_terminal_observations",
    ] {
        assert!(
            table_names.iter().any(|name| name == table),
            "missing {table}"
        );
    }
    store.close().await;

    let reopened = WorkflowStore::open(&sqlite)
        .await
        .expect("reopen workflow db");
    assert!(
        reopened
            .fts5_available()
            .await
            .expect("check FTS5 after reopen")
    );
    reopened.close().await;

    let runtime = crate::StateRuntime::init(sqlite.clone(), "test-provider".to_string())
        .await
        .expect("open state runtime");
    assert!(
        runtime
            .workflow()
            .fts5_available()
            .await
            .expect("runtime FTS5")
    );
    runtime.close().await;
    let runtime = crate::StateRuntime::init(sqlite, "test-provider".to_string())
        .await
        .expect("reopen state runtime");
    assert!(
        runtime
            .workflow_store()
            .fts5_available()
            .await
            .expect("runtime FTS5 after reopen")
    );
    runtime.close().await;
}

#[tokio::test]
async fn workflow_corruption_is_reported_with_its_separate_runtime_path() {
    let sqlite = sqlite_config();
    tokio::fs::create_dir_all(sqlite.home())
        .await
        .expect("create sqlite home");
    tokio::fs::write(sqlite.workflow_db_path(), b"not sqlite")
        .await
        .expect("write malformed workflow db");
    let error = match crate::StateRuntime::init(sqlite.clone(), "test-provider".to_string()).await {
        Ok(_) => panic!("malformed workflow db should fail initialization"),
        Err(error) => error,
    };
    assert_eq!(
        crate::runtime_db_path_for_corruption_error(&error),
        Some(sqlite.workflow_db_path())
    );
}

#[tokio::test]
async fn workflow_fts5_indexes_only_building_generation_documents() {
    let store = WorkflowStore::open(&sqlite_config())
        .await
        .expect("open workflow db");
    let generation = store
        .begin_search_generation()
        .await
        .expect("begin search generation");
    store
        .insert_search_document(&SearchDocumentCreate {
            generation_id: generation.generation_id,
            thread_id: "thread-a".to_string(),
            source_id: "item-a".to_string(),
            source_kind: SearchSourceKind::User,
            ordinal: 0,
            content: "durable workflow search".to_string(),
            metadata: SearchDocumentMetadata::default(),
        })
        .await
        .expect("insert indexed document");
    store
        .insert_search_document(&SearchDocumentCreate {
            generation_id: generation.generation_id,
            thread_id: "thread-b".to_string(),
            source_id: "item-b".to_string(),
            source_kind: SearchSourceKind::FinalAssistant,
            ordinal: 1,
            content: "unpublished search document".to_string(),
            metadata: SearchDocumentMetadata::default(),
        })
        .await
        .expect("insert second document");
    assert!(
        store
            .search("durable", 10)
            .await
            .expect("search before publish")
            .is_empty()
    );
    assert!(
        store
            .publish_search_generation(generation.generation_id)
            .await
            .expect("publish generation")
    );
    let results = store.search("workflow", 10).await.expect("query FTS5");
    assert_eq!(results.len(), 1);
    assert_eq!(results[0].thread_id, "thread-a");
    assert_eq!(results[0].source_id, "item-a");
    assert_eq!(results[0].metadata, SearchDocumentMetadata::default());
    store.close().await;
}

#[tokio::test]
async fn search_generation_publication_is_atomic_and_survives_reopen() {
    let sqlite = sqlite_config();
    let store = WorkflowStore::open(&sqlite)
        .await
        .expect("open workflow db");
    let first = store.begin_search_generation().await.expect("begin first");
    assert!(
        store
            .publish_search_generation(first.generation_id)
            .await
            .expect("publish first")
    );
    let second = store.begin_search_generation().await.expect("begin second");

    let mut tx = store.pool().begin().await.expect("begin test transaction");
    sqlx::query(
        "UPDATE workflow_search_generations SET state = 'published' WHERE generation_id = ?",
    )
    .bind(second.generation_id)
    .execute(&mut *tx)
    .await
    .expect("mutate uncommitted generation");
    tx.rollback().await.expect("rollback test transaction");
    assert_eq!(
        store
            .active_search_generation()
            .await
            .expect("read active generation")
            .expect("active generation")
            .generation_id,
        first.generation_id
    );
    assert!(
        !store
            .publish_search_generation(first.generation_id)
            .await
            .expect("stale publish should be a CAS miss")
    );
    assert!(
        store
            .publish_search_generation(second.generation_id)
            .await
            .expect("publish second")
    );
    store.close().await;

    let reopened = WorkflowStore::open(&sqlite)
        .await
        .expect("reopen workflow db");
    assert_eq!(
        reopened
            .active_search_generation()
            .await
            .expect("read active generation after reopen")
            .expect("active generation after reopen")
            .generation_id,
        second.generation_id
    );
    reopened.close().await;
}

#[tokio::test]
async fn search_projection_rejects_private_sources_and_duplicate_payloads() {
    let store = WorkflowStore::open(&sqlite_config())
        .await
        .expect("open workflow db");
    let generation = store
        .begin_search_generation()
        .await
        .expect("begin generation");
    let input = SearchDocumentCreate {
        generation_id: generation.generation_id,
        thread_id: "thread-a".to_string(),
        source_id: "item-a".to_string(),
        source_kind: SearchSourceKind::User,
        ordinal: 0,
        content: "public searchable message".to_string(),
        metadata: SearchDocumentMetadata {
            root_thread_id: Some("root-a".to_string()),
            project_id: Some("project-a".to_string()),
            cwd: Some("C:/workspace".to_string()),
            provider: Some("openai".to_string()),
            thread_class: Some(WorkflowThreadClass::Interactive),
            outcome: None,
            archived: false,
            event_time_ms: Some(1),
        },
    };
    let first = store
        .insert_search_document(&input)
        .await
        .expect("insert document");
    let repeated = store
        .insert_search_document(&input)
        .await
        .expect("repeat document");
    assert_eq!(first.document_id, repeated.document_id);
    assert_eq!(
        sqlx::query_scalar::<_, i64>(
            "SELECT document_count FROM workflow_search_generations WHERE generation_id = ?",
        )
        .bind(generation.generation_id)
        .fetch_one(store.pool())
        .await
        .expect("read document count"),
        1
    );
    let mut conflict = input.clone();
    conflict.content = "changed payload".to_string();
    assert!(store.insert_search_document(&conflict).await.is_err());
    assert!(SearchSourceKind::from_str("tool").is_err());
    assert!(SearchDocumentMetadata::from_json(json!({"secret": "value"})).is_err());
    store.close().await;
}

#[tokio::test]
async fn search_page_filters_paginates_and_rejects_stale_cursors() {
    let store = WorkflowStore::open(&sqlite_config())
        .await
        .expect("open workflow db");
    let generation = store
        .begin_search_generation()
        .await
        .expect("begin generation");
    for index in 0..3 {
        store
            .insert_search_document(&SearchDocumentCreate {
                generation_id: generation.generation_id,
                thread_id: format!("thread-{index}"),
                source_id: format!("item-{index}"),
                source_kind: SearchSourceKind::FinalAssistant,
                ordinal: index,
                content: format!("needle result {index}"),
                metadata: SearchDocumentMetadata {
                    root_thread_id: Some("root-a".to_string()),
                    project_id: Some("project-a".to_string()),
                    cwd: Some("C:/workspace".to_string()),
                    provider: Some("openai".to_string()),
                    thread_class: Some(WorkflowThreadClass::Interactive),
                    outcome: None,
                    archived: index == 2,
                    event_time_ms: Some(index),
                },
            })
            .await
            .expect("insert result");
    }
    store
        .publish_search_generation(generation.generation_id)
        .await
        .expect("publish generation");
    let request = SearchRequest::new(
        "needle",
        SearchFilter {
            project_id: Some("project-a".to_string()),
            archived: Some(false),
            ..SearchFilter::default()
        },
        None,
        1,
    )
    .expect("valid search request");
    let first = store.search_page(&request).await.expect("first page");
    assert_eq!(first.documents.len(), 1);
    assert!(
        first.documents[0]
            .snippet
            .as_deref()
            .unwrap_or_default()
            .len()
            <= 512
    );
    let cursor = first.next_cursor.clone().expect("next cursor");
    let next = store
        .search_page(&SearchRequest {
            cursor: Some(cursor.clone()),
            ..request.clone()
        })
        .await
        .expect("second page");
    assert_eq!(next.documents.len(), 1);
    let changed_filter = SearchRequest {
        filter: SearchFilter {
            provider: Some("other".to_string()),
            ..request.filter.clone()
        },
        cursor: Some(cursor),
        ..request.clone()
    };
    assert!(store.search_page(&changed_filter).await.is_err());
    let next_generation = store
        .begin_search_generation()
        .await
        .expect("begin next generation");
    store
        .publish_search_generation(next_generation.generation_id)
        .await
        .expect("publish next generation");
    assert!(
        store
            .search_page(&SearchRequest {
                cursor: first.next_cursor,
                ..request
            })
            .await
            .is_err()
    );
    store.close().await;
}

#[tokio::test]
async fn search_page_does_not_use_archived_generation_metadata_as_a_filter() {
    let store = WorkflowStore::open(&sqlite_config())
        .await
        .expect("open workflow db");
    let generation = store
        .begin_search_generation()
        .await
        .expect("begin generation");
    store
        .insert_search_document(&SearchDocumentCreate {
            generation_id: generation.generation_id,
            thread_id: "thread-archived".to_string(),
            source_id: "item-archived".to_string(),
            source_kind: SearchSourceKind::FinalAssistant,
            ordinal: 0,
            content: "mutable archive state".to_string(),
            metadata: SearchDocumentMetadata {
                archived: true,
                ..SearchDocumentMetadata::default()
            },
        })
        .await
        .expect("insert archived generation document");
    store
        .publish_search_generation(generation.generation_id)
        .await
        .expect("publish generation");

    for archived in [false, true] {
        let request = SearchRequest::new(
            "mutable",
            SearchFilter {
                archived: Some(archived),
                ..SearchFilter::default()
            },
            None,
            10,
        )
        .expect("valid search request");
        let page = store.search_page(&request).await.expect("search page");
        assert_eq!(page.documents.len(), 1);
        assert_eq!(page.documents[0].thread_id, "thread-archived");
    }
    store.close().await;
}

#[tokio::test]
async fn live_overlay_is_deduplicated_epoch_bound_and_literal_safe() {
    let store = WorkflowStore::open(&sqlite_config())
        .await
        .expect("open workflow db");
    let input = LiveSearchDocumentCreate {
        thread_id: "thread-live".to_string(),
        source_id: "item-live".to_string(),
        source_kind: SearchSourceKind::User,
        ordinal: 0,
        content: "needle quote C++ punctuation".to_string(),
        metadata: SearchDocumentMetadata::default(),
    };
    store
        .upsert_live_search_document(&input)
        .await
        .expect("insert live document");
    let first_epoch = store.live_search_epoch().await.expect("read live epoch");
    store
        .upsert_live_search_document(&input)
        .await
        .expect("repeat live document");
    assert_eq!(
        store
            .live_search_epoch()
            .await
            .expect("read repeated epoch"),
        first_epoch
    );
    store
        .upsert_live_search_document(&LiveSearchDocumentCreate {
            content: format!("{} {}", input.content, "x".repeat(900)),
            ..input.clone()
        })
        .await
        .expect("update live document");
    assert_eq!(
        store.live_search_epoch().await.expect("read changed epoch"),
        first_epoch + 1
    );
    let request = SearchRequest::new("needle OR *", SearchFilter::default(), None, 10)
        .expect("literal query");
    let results = store.search_page(&request).await.expect("literal search");
    assert_eq!(
        results.documents.len(),
        0,
        "operator syntax must not broaden a literal query"
    );
    let request =
        SearchRequest::new("quote", SearchFilter::default(), None, 10).expect("snippet query");
    let results = store.search_page(&request).await.expect("snippet search");
    assert_eq!(results.documents.len(), 1);
    assert!(results.documents[0].is_live);
    assert!(
        results.documents[0]
            .snippet
            .as_deref()
            .unwrap_or_default()
            .len()
            <= 512
    );
    assert!(
        store
            .remove_live_search_document("thread-live", "item-live", SearchSourceKind::User)
            .await
            .expect("remove live document")
    );
    assert_eq!(
        store.live_search_epoch().await.expect("read removal epoch"),
        first_epoch + 2
    );
    store.close().await;
}

#[tokio::test]
async fn published_search_documents_cannot_be_mutated_after_crash_like_rollback() {
    let store = WorkflowStore::open(&sqlite_config())
        .await
        .expect("open workflow db");
    let generation = store
        .begin_search_generation()
        .await
        .expect("begin generation");
    store
        .insert_search_document(&SearchDocumentCreate {
            generation_id: generation.generation_id,
            thread_id: "thread-a".to_string(),
            source_id: "item-a".to_string(),
            source_kind: SearchSourceKind::User,
            ordinal: 0,
            content: "immutable".to_string(),
            metadata: SearchDocumentMetadata::default(),
        })
        .await
        .expect("insert document");
    store
        .publish_search_generation(generation.generation_id)
        .await
        .expect("publish generation");
    let update = sqlx::query(
        "UPDATE workflow_search_documents SET content = 'tampered' WHERE document_id = 1",
    )
    .execute(store.pool())
    .await;
    assert!(update.is_err());
    let delete = sqlx::query("DELETE FROM workflow_search_documents WHERE document_id = 1")
        .execute(store.pool())
        .await;
    assert!(delete.is_err());
    let generation_delete =
        sqlx::query("DELETE FROM workflow_search_generations WHERE generation_id = ?")
            .bind(generation.generation_id)
            .execute(store.pool())
            .await;
    assert!(generation_delete.is_err());
    assert_eq!(
        store
            .active_search_generation()
            .await
            .expect("read active generation")
            .unwrap()
            .generation_id,
        generation.generation_id
    );
    store.close().await;
}

#[tokio::test]
async fn workflow_run_idempotency_transition_cas_and_checkpoints_are_bounded() {
    let store = WorkflowStore::open(&sqlite_config())
        .await
        .expect("open workflow db");
    let input = run_create("run-1", Some("job-key"));
    let created = store.create_run(&input).await.expect("create run");
    assert_eq!(created.version, 0);
    assert_eq!(
        store.create_run(&input).await.expect("idempotent create"),
        created
    );
    assert_eq!(
        store
            .create_run(&run_create("run-2", Some("job-key")))
            .await
            .expect("same-key retry"),
        created
    );
    assert_eq!(
        store.get_run("run-1").await.expect("get run"),
        Some(created.clone())
    );

    assert!(
        store
            .transition_run_cas("run-1", 0, "pending", "running", None)
            .await
            .expect("transition to running")
    );
    assert!(
        !store
            .transition_run_cas("run-1", 0, "pending", "failed", Some("stale"))
            .await
            .expect("stale transition")
    );
    assert!(
        store
            .transition_run_status_cas("run-1", "running", "succeeded", Some("pass"))
            .await
            .expect("transition to succeeded")
    );
    let finished = store
        .get_run("run-1")
        .await
        .expect("get finished run")
        .expect("run");
    assert_eq!(finished.status, "succeeded");
    assert_eq!(finished.outcome.as_deref(), Some("pass"));
    assert_eq!(finished.version, 2);

    for kind in ["start", "finish"] {
        store
            .append_checkpoint(&WorkflowCheckpointCreate {
                run_id: "run-1".to_string(),
                checkpoint_kind: kind.to_string(),
                rollout_ordinal: Some(0),
                rollout_byte_offset: Some(10),
                payload: json!({"kind": kind}),
            })
            .await
            .expect("append checkpoint");
    }
    let checkpoints = store
        .list_checkpoints("run-1", None, 10)
        .await
        .expect("list checkpoints");
    assert_eq!(checkpoints.len(), 2);
    assert_eq!(checkpoints[0].sequence, 0);
    assert_eq!(checkpoints[1].sequence, 1);
    assert_eq!(
        store
            .list_checkpoints("run-1", Some(0), 10)
            .await
            .expect("list after checkpoint")
            .len(),
        1
    );
    assert!(
        store
            .append_checkpoint(&WorkflowCheckpointCreate {
                run_id: "run-1".to_string(),
                checkpoint_kind: "invalid".to_string(),
                rollout_ordinal: Some(-1),
                rollout_byte_offset: None,
                payload: json!({}),
            })
            .await
            .is_err()
    );
    store.close().await;
}

#[tokio::test]
async fn transient_job_idempotency_uses_bounded_immutable_parameters() {
    let store = WorkflowStore::open(&sqlite_config())
        .await
        .expect("open workflow db");
    let first_input = run_create("run-1", Some("same-key"));
    let first = store.create_run(&first_input).await.expect("create first");
    let digest = first_input
        .immutable_params_digest()
        .expect("digest first parameters");
    assert_eq!(digest, first_input.immutable_params_digest().unwrap());
    assert_eq!(digest.as_bytes().len(), 32);

    let retry_input = run_create("run-2", Some("same-key"));
    let retry = store
        .create_run(&retry_input)
        .await
        .expect("idempotent retry");
    assert_eq!(retry, first);

    let mut changed_input = retry_input;
    changed_input.metadata = Some(json!({"kind": "changed"}));
    assert!(
        store.create_run(&changed_input).await.is_err(),
        "same root/key with different immutable parameters must conflict"
    );
    assert_eq!(
        store
            .get_run_by_idempotency_key("root-thread", "same-key")
            .await
            .expect("lookup by idempotency key"),
        Some(first)
    );
    store.close().await;
}

#[tokio::test]
async fn transient_job_creation_is_concurrent_and_identity_safe() {
    let store = std::sync::Arc::new(
        WorkflowStore::open(&sqlite_config())
            .await
            .expect("open workflow db"),
    );
    let first_input = run_create("run-1", Some("concurrent-key"));
    let second_input = run_create("run-2", Some("concurrent-key"));
    let (first, second) = tokio::join!(
        store.create_run(&first_input),
        store.create_run(&second_input)
    );
    let first = first.expect("first concurrent create");
    let second = second.expect("second concurrent create");
    assert_eq!(first, second);
    assert_eq!(first.run_id, first.thread_id);

    let mut invalid_identity = run_create("run-3", Some("different-key"));
    invalid_identity.thread_id = "different-thread".to_string();
    assert!(store.create_run(&invalid_identity).await.is_err());
    store.close().await;
}

#[tokio::test]
async fn workflow_run_listing_is_keyset_paginated_and_filtered() {
    let store = WorkflowStore::open(&sqlite_config())
        .await
        .expect("open workflow db");
    for run_id in ["run-1", "run-2", "run-3"] {
        store
            .create_run(&run_create(run_id, None))
            .await
            .expect("create run");
    }
    sqlx::query("UPDATE workflow_runs SET created_at_ms = 100, updated_at_ms = 100")
        .execute(store.pool())
        .await
        .expect("make equal timestamps");

    let first_page = store
        .list_runs(
            &WorkflowRunListRequest::new(
                WorkflowRunListFilter {
                    thread_class: Some(WorkflowThreadClass::TransientJob),
                    status: Some("pending".to_string()),
                    root_thread_id: Some("root-thread".to_string()),
                },
                None,
                2,
            )
            .expect("valid list request"),
        )
        .await
        .expect("list first page");
    assert_eq!(
        first_page
            .runs
            .iter()
            .map(|run| run.run_id.as_str())
            .collect::<Vec<_>>(),
        ["run-3", "run-2"]
    );
    let cursor = first_page.next_cursor.expect("next cursor");
    let second_page = store
        .list_runs(
            &WorkflowRunListRequest::new(
                WorkflowRunListFilter {
                    thread_class: Some(WorkflowThreadClass::TransientJob),
                    status: Some("pending".to_string()),
                    root_thread_id: Some("root-thread".to_string()),
                },
                Some(cursor.clone()),
                2,
            )
            .expect("valid second request"),
        )
        .await
        .expect("list second page");
    assert_eq!(
        second_page
            .runs
            .iter()
            .map(|run| run.run_id.as_str())
            .collect::<Vec<_>>(),
        ["run-1"]
    );
    assert!(second_page.next_cursor.is_none());
    assert!(WorkflowRunCursor::new(-1, "run-1").is_err());
    assert!(
        WorkflowRunListRequest::new(
            WorkflowRunListFilter::default(),
            Some(WorkflowRunCursor {
                created_at_ms: 0,
                run_id: String::new(),
            }),
            1,
        )
        .is_err()
    );
    assert!(WorkflowRunListRequest::new(WorkflowRunListFilter::default(), None, 0).is_err());
    assert!(
        WorkflowRunListRequest::new(
            WorkflowRunListFilter {
                status: Some("x".repeat(33)),
                ..WorkflowRunListFilter::default()
            },
            None,
            1,
        )
        .is_err()
    );
    store.close().await;
}

#[tokio::test]
async fn workflow_run_batch_read_and_terminal_transition_are_idempotent() {
    let store = WorkflowStore::open(&sqlite_config())
        .await
        .expect("open workflow db");
    let first = store
        .create_run(&run_create("run-1", None))
        .await
        .expect("create first");
    let second = store
        .create_run(&run_create("run-2", None))
        .await
        .expect("create second");
    let batch = store
        .get_runs_by_thread_ids(&[first.thread_id.clone(), second.thread_id.clone()])
        .await
        .expect("batch read");
    assert_eq!(batch.len(), 2);
    assert_eq!(
        store
            .get_runs_by_thread_id(&first.thread_id)
            .await
            .expect("single-thread read"),
        vec![first.clone()]
    );
    assert!(store.get_runs_by_thread_ids(&[]).await.unwrap().is_empty());

    assert!(
        store
            .transition_run_cas("run-1", first.version, "pending", "succeeded", Some("done"))
            .await
            .expect("terminal transition")
    );
    let terminal = store.get_run("run-1").await.unwrap().unwrap();
    assert_eq!(terminal.version, 1);
    assert!(
        store
            .transition_run_cas("run-1", first.version, "pending", "succeeded", Some("done"))
            .await
            .expect("repeat terminal transition")
    );
    assert_eq!(store.get_run("run-1").await.unwrap().unwrap().version, 1);
    assert_eq!(
        store
            .transition_run_cas_outcome("run-1", 0, "pending", "failed", Some("other"))
            .await
            .expect("read terminal conflict"),
        WorkflowRunTransitionOutcome::Stale
    );
    store.close().await;
}

#[tokio::test]
async fn stale_workflow_runs_recover_once_without_retry() {
    let store = std::sync::Arc::new(
        WorkflowStore::open(&sqlite_config())
            .await
            .expect("open workflow db"),
    );
    let _pending = store
        .create_run(&run_create("run-1", None))
        .await
        .expect("create pending run");
    let running = store
        .create_run(&run_create("run-2", None))
        .await
        .expect("create running run");
    store
        .transition_run_cas(&running.run_id, running.version, "pending", "running", None)
        .await
        .expect("start running run");
    sqlx::query("UPDATE workflow_runs SET updated_at_ms = 0 WHERE run_id IN ('run-1', 'run-2')")
        .execute(store.pool())
        .await
        .expect("age runs");

    let (first, second) = tokio::join!(
        store.recover_stale_run("run-1", 1),
        store.recover_stale_run("run-1", 1)
    );
    assert!(first.expect("first stale recovery").is_some());
    assert!(second.expect("second stale recovery").is_some());
    let recovered = store.get_run("run-1").await.unwrap().unwrap();
    assert_eq!(recovered.status, "inconclusive");
    assert_eq!(recovered.outcome.as_deref(), Some("stale"));
    assert_eq!(
        recovered.version, 1,
        "concurrent recovery must increment once"
    );

    let recovered_batch = store
        .recover_stale_runs(1)
        .await
        .expect("recover stale batch");
    assert_eq!(recovered_batch.len(), 1);
    assert_eq!(recovered_batch[0].run_id, "run-2");
    assert_eq!(
        store.get_run("run-2").await.unwrap().unwrap().status,
        "inconclusive"
    );
    assert!(store.recover_stale_runs(1).await.unwrap().is_empty());

    assert_eq!(
        store
            .recover_stale_run_cas("run-1", 0, 1)
            .await
            .expect("stale generation result"),
        WorkflowRunTransitionOutcome::AlreadyApplied
    );
    store.close().await;
}
