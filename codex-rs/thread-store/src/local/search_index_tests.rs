use std::fs;
use std::io::Write;
use std::path::Path;
use std::path::PathBuf;

use codex_protocol::ThreadId;
use codex_protocol::protocol::ThreadHistoryMode;
use codex_state::SearchMetadata;
use codex_state::SearchRequest;
use codex_state::SqliteConfig;
use codex_state::WorkflowStore;
use codex_utils_absolute_path::test_support::PathExt;
use tempfile::TempDir;

use super::*;

fn rollout_path(home: &Path, name: &str) -> PathBuf {
    let directory = home.join("sessions/2025/01/03");
    fs::create_dir_all(&directory).expect("create rollout directory");
    directory.join(name)
}

fn session_meta(thread_id: ThreadId, home: &Path, mode: ThreadHistoryMode) -> serde_json::Value {
    serde_json::json!({
        "timestamp": "2025-01-03T10:00:00Z",
        "type": "session_meta",
        "payload": {
            "session_id": thread_id,
            "id": thread_id,
            "timestamp": "2025-01-03T10:00:00Z",
            "cwd": home,
            "originator": "test",
            "cli_version": "test",
            "source": "cli",
            "model_provider": "test-provider",
            "history_mode": mode,
            "git": null
        },
        "ordinal": 0
    })
}

fn user_line(ordinal: u64, message: &str) -> serde_json::Value {
    serde_json::json!({
        "timestamp": "2025-01-03T10:00:01Z",
        "ordinal": ordinal,
        "type": "event_msg",
        "payload": {
            "type": "user_message",
            "message": message
        }
    })
}

fn assistant_complete_line(ordinal: u64, message: &str) -> serde_json::Value {
    serde_json::json!({
        "timestamp": "2025-01-03T10:00:02Z",
        "ordinal": ordinal,
        "type": "event_msg",
        "payload": {
            "type": "turn_complete",
            "turn_id": "turn-1",
            "last_agent_message": message
        }
    })
}

fn write_lines(path: &Path, lines: impl IntoIterator<Item = serde_json::Value>) {
    let mut file = fs::File::create(path).expect("create rollout");
    for line in lines {
        writeln!(file, "{line}").expect("write rollout line");
    }
}

fn metadata() -> SearchMetadata {
    SearchMetadata {
        root_thread_id: None,
        project_id: None,
        cwd: Some("C:/workspace".to_string()),
        provider: Some("test-provider".to_string()),
        thread_class: Some(codex_state::WorkflowThreadClass::Interactive),
        outcome: None,
        archived: false,
        event_time_ms: None,
    }
}

async fn workflow_store(home: &Path) -> WorkflowStore {
    let sqlite = SqliteConfig::new_for_testing(home.abs());
    WorkflowStore::open(&sqlite)
        .await
        .expect("open workflow store")
}

#[tokio::test]
async fn generation_projection_reads_plain_and_zstd_rollouts() {
    let home = TempDir::new().expect("temp home");
    let thread_id = ThreadId::default();
    let plain = rollout_path(home.path(), "rollout-plain.jsonl");
    write_lines(
        &plain,
        [
            session_meta(thread_id, home.path(), ThreadHistoryMode::Legacy),
            user_line(1, "plain user query"),
            assistant_complete_line(2, "plain final answer"),
        ],
    );
    let compressed = home.path().join("rollout-compressed.jsonl.zst");
    let bytes = fs::read(&plain).expect("read plain rollout");
    let compressed_bytes = zstd::stream::encode_all(bytes.as_slice(), 3).expect("compress rollout");
    fs::write(&compressed, compressed_bytes).expect("write compressed rollout");

    let workflow = workflow_store(home.path()).await;
    let generation = workflow
        .begin_search_generation()
        .await
        .expect("begin generation");
    let plain_progress = project_rollout_into_generation(
        &workflow,
        &plain,
        thread_id,
        generation.generation_id,
        SearchProjectionCursor::default(),
        metadata(),
    )
    .await
    .expect("project plain rollout");
    assert_eq!(plain_progress.indexed_documents, 2);
    assert_eq!(plain_progress.parse_errors, 0);
    let compressed_progress = project_rollout_into_generation(
        &workflow,
        &compressed,
        ThreadId::from_string("00000000-0000-0000-0000-000000000001").expect("rollout id"),
        generation.generation_id,
        SearchProjectionCursor::default(),
        metadata(),
    )
    .await
    .expect("project compressed rollout");
    assert_eq!(compressed_progress.indexed_documents, 2);
    workflow
        .publish_search_generation(generation.generation_id)
        .await
        .expect("publish generation");
    let page = workflow
        .search_page(
            &SearchRequest::new("final answer", Default::default(), None, 50)
                .expect("search request"),
        )
        .await
        .expect("search indexed generation");
    assert_eq!(page.documents.len(), 2);
}

#[tokio::test]
async fn generation_projection_respects_history_base_and_resumes_by_cursor() {
    let home = TempDir::new().expect("temp home");
    let thread_id = ThreadId::default();
    let path = rollout_path(home.path(), "rollout-lineage.jsonl");
    let parent_id =
        ThreadId::from_string("00000000-0000-0000-0000-000000000002").expect("parent id");
    let mut meta = session_meta(thread_id, home.path(), ThreadHistoryMode::Paginated);
    meta["payload"]["history_base"] = serde_json::json!({
        "thread_id": parent_id,
        "end_ordinal_exclusive": 5,
        "end_byte_offset": 0
    });
    write_lines(
        &path,
        [
            meta,
            user_line(1, "inherited user must be hidden"),
            user_line(5, "child user is visible"),
        ],
    );
    let workflow = workflow_store(home.path()).await;
    let generation = workflow
        .begin_search_generation()
        .await
        .expect("begin generation");
    let progress = project_rollout_into_generation(
        &workflow,
        &path,
        thread_id,
        generation.generation_id,
        SearchProjectionCursor::default(),
        metadata(),
    )
    .await
    .expect("project lineage rollout");
    assert_eq!(progress.indexed_documents, 1);
    assert!(progress.next_cursor.ordinal >= 6);

    let resumed = project_rollout_into_generation(
        &workflow,
        &path,
        thread_id,
        generation.generation_id,
        progress.next_cursor,
        metadata(),
    )
    .await
    .expect("resume projection");
    assert_eq!(resumed.indexed_documents, 0);
}

#[tokio::test]
async fn live_overlay_is_bounded_and_best_effort_on_parse_failure() {
    let home = TempDir::new().expect("temp home");
    let thread_id = ThreadId::default();
    let path = rollout_path(home.path(), "rollout-live.jsonl");
    let mut file = fs::File::create(&path).expect("create live rollout");
    writeln!(
        file,
        "{}",
        session_meta(thread_id, home.path(), ThreadHistoryMode::Legacy)
    )
    .expect("write metadata");
    writeln!(file, "not-json").expect("write malformed line");
    writeln!(file, "{}", user_line(2, "live overlay query")).expect("write user line");

    let config = super::super::test_support::test_config(home.path());
    let runtime =
        codex_state::StateRuntime::init(config.sqlite.clone(), "test-provider".to_string())
            .await
            .expect("open state runtime");
    let store = LocalThreadStore::new(config, Some(runtime.clone()));
    project_live_rollout(&store, thread_id, thread_id, &path).await;
    project_live_rollout(&store, thread_id, thread_id, &path).await;

    let page = runtime
        .workflow()
        .search_page(
            &SearchRequest::new("live overlay", Default::default(), None, 50)
                .expect("search request"),
        )
        .await
        .expect("search live overlay");
    assert_eq!(page.documents.len(), 1);
    assert!(page.documents[0].is_live);

    let status = sqlx::query_scalar::<_, String>(
        "SELECT status FROM workflow_backfill_journal WHERE rollout_id = ?",
    )
    .bind(thread_id.to_string())
    .fetch_one(runtime.workflow().pool())
    .await
    .expect("recoverable source journal");
    assert_eq!(status, "recoverable");
}

#[tokio::test]
async fn parse_failure_does_not_disturb_a_journal_row_being_processed() {
    let home = TempDir::new().expect("temp home");
    let thread_id = ThreadId::default();
    let path = rollout_path(home.path(), "rollout-processing.jsonl");
    let mut file = fs::File::create(&path).expect("create rollout");
    writeln!(
        file,
        "{}",
        session_meta(thread_id, home.path(), ThreadHistoryMode::Legacy)
    )
    .expect("write metadata");
    writeln!(file, "not-json").expect("write malformed line");

    let config = super::super::test_support::test_config(home.path());
    let runtime =
        codex_state::StateRuntime::init(config.sqlite.clone(), "test-provider".to_string())
            .await
            .expect("open state runtime");
    let store = LocalThreadStore::new(config, Some(runtime.clone()));

    // A coordinator already owns this rollout.
    sqlx::query(
        "INSERT INTO workflow_backfill_journal
            (rollout_id, source_path, byte_offset, rollout_ordinal, status,
             updated_at_ms, owner_id, generation)
         VALUES (?, ?, 0, 0, 'processing', 1, 'owner-1', 0)",
    )
    .bind(thread_id.to_string())
    .bind(path.to_string_lossy().to_string())
    .execute(runtime.workflow().pool())
    .await
    .expect("seed processing journal row");

    project_live_rollout(&store, thread_id, thread_id, &path).await;

    let (status, owner_id) = sqlx::query_as::<_, (String, Option<String>)>(
        "SELECT status, owner_id FROM workflow_backfill_journal WHERE rollout_id = ?",
    )
    .bind(thread_id.to_string())
    .fetch_one(runtime.workflow().pool())
    .await
    .expect("processing source journal");
    assert_eq!(status, "processing");
    assert_eq!(owner_id.as_deref(), Some("owner-1"));
}
