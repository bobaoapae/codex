use chrono::Utc;
use codex_protocol::ThreadId;
use codex_protocol::protocol::SessionSource;
use codex_protocol::protocol::ThreadHistoryMode;
use codex_state::StateRuntime;
use codex_state::ThreadMetadataBuilder;
use pretty_assertions::assert_eq;
use tempfile::TempDir;
use uuid::Uuid;

use crate::ListThreadsParams;
use crate::ThreadSortKey;
use crate::ThreadStore;
use crate::TombstoneThreadParams;
use crate::local::LocalThreadStore;
use crate::local::test_support::test_config;
use crate::local::test_support::write_session_file;
use crate::local::test_support::write_session_file_with_history_mode;

#[tokio::test]
async fn tombstone_retains_rollouts_and_hides_default_reads_and_lists() {
    let home = TempDir::new().expect("temporary codex home");
    let config = test_config(home.path());
    let runtime = codex_state::StateRuntime::init(
        config.sqlite.clone(),
        config.default_model_provider_id.clone(),
    )
    .await
    .expect("state database");
    let uuid = Uuid::from_u128(701);
    let thread_id = ThreadId::from_string(&uuid.to_string()).expect("thread id");
    let rollout_path =
        write_session_file(home.path(), "2025-01-03T12-00-00", uuid).expect("rollout file");
    let compressed_path = rollout_path.with_extension("jsonl.zst");
    std::fs::write(&compressed_path, b"compressed rollout").expect("compressed rollout");
    let mut builder = ThreadMetadataBuilder::new(
        thread_id,
        rollout_path.clone(),
        Utc::now(),
        SessionSource::Cli,
    );
    builder.cwd = home.path().to_path_buf();
    builder.model_provider = Some(config.default_model_provider_id.clone());
    runtime
        .upsert_thread(&builder.build(config.default_model_provider_id.as_str()))
        .await
        .expect("thread metadata");
    let store = LocalThreadStore::new(config, Some(runtime.clone()));

    store
        .tombstone_thread(TombstoneThreadParams { thread_id })
        .await
        .expect("tombstone thread");
    assert!(rollout_path.exists());
    assert!(compressed_path.exists());
    assert!(
        runtime
            .is_thread_tombstoned(thread_id)
            .await
            .expect("tombstone state")
    );
    assert!(
        store
            .read_thread(crate::ReadThreadParams {
                thread_id,
                include_archived: true,
                include_history: false,
            })
            .await
            .is_err()
    );
    let page = store
        .list_threads(ListThreadsParams {
            page_size: 10,
            cursor: None,
            sort_key: ThreadSortKey::CreatedAt,
            sort_direction: crate::SortDirection::Desc,
            allowed_sources: Vec::new(),
            model_providers: None,
            cwd_filters: None,
            section: None,
            project_id: None,
            archived: false,
            search_term: None,
            relation_filter: None,
            use_state_db_only: true,
        })
        .await
        .expect("visible thread list");
    assert_eq!(page.items.len(), 0);

    store
        .tombstone_thread(TombstoneThreadParams { thread_id })
        .await
        .expect("repeated tombstone is idempotent");
    assert!(rollout_path.exists());
    assert!(compressed_path.exists());
}

#[tokio::test]
async fn tombstone_rejects_external_writer_before_state_mutation() {
    let home = TempDir::new().expect("temporary codex home");
    let config = test_config(home.path());
    let runtime = StateRuntime::init(
        config.sqlite.clone(),
        config.default_model_provider_id.clone(),
    )
    .await
    .expect("state database");
    let uuid = Uuid::from_u128(702);
    let thread_id = ThreadId::from_string(&uuid.to_string()).expect("thread id");
    let rollout_path = write_session_file_with_history_mode(
        home.path(),
        "2025-01-03T12-00-01",
        uuid,
        ThreadHistoryMode::Paginated,
    )
    .expect("rollout file");
    let mut builder = ThreadMetadataBuilder::new(
        thread_id,
        rollout_path.clone(),
        Utc::now(),
        SessionSource::Cli,
    );
    builder.cwd = home.path().to_path_buf();
    builder.model_provider = Some(config.default_model_provider_id.clone());
    runtime
        .upsert_thread(&builder.build(config.default_model_provider_id.as_str()))
        .await
        .expect("thread metadata");
    let store = LocalThreadStore::new(config.clone(), Some(runtime.clone()));
    let owner = LocalThreadStore::new(config, Some(runtime.clone()));
    let writer_guard = owner
        .writer_lock_coordinator
        .acquire(thread_id)
        .expect("acquire writer lock");

    let error = store
        .tombstone_thread(TombstoneThreadParams { thread_id })
        .await
        .expect_err("external writer should block tombstone");
    assert!(matches!(error, crate::ThreadStoreError::Conflict { .. }));
    assert!(
        !runtime
            .is_thread_tombstoned(thread_id)
            .await
            .expect("tombstone state")
    );
    assert!(rollout_path.exists());

    drop(writer_guard);
    store
        .tombstone_thread(TombstoneThreadParams { thread_id })
        .await
        .expect("tombstone after writer exits");
    assert!(
        runtime
            .is_thread_tombstoned(thread_id)
            .await
            .expect("tombstone state")
    );
}
