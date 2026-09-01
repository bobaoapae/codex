use std::fs;
use std::io::Write;
use std::path::Path;
use std::path::PathBuf;

use codex_extension_items::ExtensionItem;
use codex_extension_items::receipt::ReceiptAttachedItem;
use codex_extension_items::receipt::ReceiptStatus;
use codex_protocol::ThreadId;
use codex_protocol::items::TurnItem;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::ItemCompletedEvent;
use codex_rollout::RolloutItem;
use codex_state::SearchMetadata;
use codex_state::SqliteConfig;
use codex_state::WorkflowStore;
use codex_utils_absolute_path::test_support::PathExt;
use pretty_assertions::assert_eq;
use serde_json::Value;
use serde_json::json;
use tempfile::TempDir;

use super::super::LocalThreadStore;

fn rollout_path(home: &Path, name: &str) -> PathBuf {
    let directory = home.join("sessions/2025/01/03");
    fs::create_dir_all(&directory).expect("create rollout directory");
    directory.join(name)
}

fn session_meta(thread_id: ThreadId, home: &Path) -> Value {
    json!({
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
            "history_mode": "legacy",
            "git": null
        },
        "ordinal": 0
    })
}

fn receipt_item(thread_id: ThreadId, receipt_id: &str, status: ReceiptStatus) -> RolloutItem {
    let mut receipt = ReceiptAttachedItem::new(
        receipt_id,
        3,
        "physical.smoke",
        "physical smoke",
        status,
        "2025-01-03T10:00:03Z",
        "test-hook",
    )
    .expect("valid receipt");
    receipt.thread_id = Some(thread_id.to_string());
    receipt.turn_id = Some("turn-receipt".to_string());
    receipt.job_id = Some("job-receipt".to_string());
    receipt.plan_snapshot_id = Some("plan-receipt".to_string());
    receipt.provenance = Some(json!({"runner": "focused"}));
    receipt
        .tags
        .insert("platform".to_string(), "windows".to_string());
    receipt
        .refs
        .push(codex_extension_items::receipt::ReceiptReference {
            kind: "artifact".to_string(),
            id: "artifact-1".to_string(),
        });
    receipt.metadata = Some(json!({"testName": "smoke"}));
    RolloutItem::EventMsg(EventMsg::ItemCompleted(ItemCompletedEvent {
        thread_id,
        turn_id: "turn-receipt".to_string(),
        item: TurnItem::Extension(ExtensionItem::ReceiptAttached(receipt)),
        started_at_ms: None,
        completed_at_ms: 3_000,
    }))
}

fn receipt_line(ordinal: u64, item: RolloutItem) -> Value {
    let mut value = serde_json::to_value(item).expect("serialize receipt item");
    value["ordinal"] = json!(ordinal);
    value["timestamp"] = json!("2025-01-03T10:00:03Z");
    value
}

fn write_lines(path: &Path, lines: impl IntoIterator<Item = Value>) {
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
    WorkflowStore::open(&SqliteConfig::new_for_testing(home.abs()))
        .await
        .expect("open workflow store")
}

#[tokio::test]
async fn plain_and_zstd_rollouts_project_receipts_and_reopen_losslessly() {
    let home = TempDir::new().expect("temporary home");
    let thread_id = ThreadId::from_u128(1);
    let plain = rollout_path(home.path(), "rollout-plain.jsonl");
    write_lines(
        &plain,
        [
            session_meta(thread_id, home.path()),
            receipt_line(
                1,
                receipt_item(thread_id, "receipt-plain", ReceiptStatus::Pass),
            ),
        ],
    );
    let compressed = rollout_path(home.path(), "rollout-compressed.jsonl.zst");
    let blocked_item = receipt_item(thread_id, "receipt-zstd", ReceiptStatus::Blocked);
    assert_eq!(
        serde_json::to_value(&blocked_item).expect("serialize blocked receipt")["payload"]["item"]
            ["status"],
        "blocked"
    );
    let compressed_lines = [
        session_meta(thread_id, home.path()),
        receipt_line(1, blocked_item),
    ];
    let mut compressed_json = Vec::new();
    for line in compressed_lines {
        writeln!(&mut compressed_json, "{line}").expect("write compressed json");
    }
    let compressed_bytes =
        zstd::stream::encode_all(compressed_json.as_slice(), 3).expect("compress receipt rollout");
    fs::write(&compressed, compressed_bytes).expect("write compressed rollout");

    let workflow = workflow_store(home.path()).await;
    let generation = workflow
        .begin_search_generation()
        .await
        .expect("begin generation");
    let plain_progress = super::super::search_index_projection::project_rollout_into_generation(
        &workflow,
        &plain,
        thread_id,
        generation.generation_id,
        super::super::search_index::SearchProjectionCursor::default(),
        metadata(),
    )
    .await
    .expect("project plain receipt rollout");
    assert_eq!(plain_progress.parse_errors, 0);
    let zstd_progress = super::super::search_index_projection::project_rollout_into_generation(
        &workflow,
        &compressed,
        thread_id,
        generation.generation_id,
        super::super::search_index::SearchProjectionCursor::default(),
        metadata(),
    )
    .await
    .expect("project zstd receipt rollout");
    assert_eq!(zstd_progress.parse_errors, 0);
    let plain_receipt = workflow
        .get_receipt("receipt-plain")
        .await
        .expect("read plain receipt")
        .expect("plain receipt");
    let expected_thread_id = thread_id.to_string();
    assert_eq!(
        plain_receipt.thread_id.as_deref(),
        Some(expected_thread_id.as_str())
    );
    assert_eq!(plain_receipt.turn_id.as_deref(), Some("turn-receipt"));
    assert_eq!(plain_receipt.job_id.as_deref(), Some("job-receipt"));
    assert_eq!(
        plain_receipt.plan_snapshot_id.as_deref(),
        Some("plan-receipt")
    );
    assert_eq!(plain_receipt.status, "pass");
    assert_eq!(plain_receipt.tags[0].key, "platform");
    assert_eq!(plain_receipt.references[0].id, "artifact-1");
    assert_eq!(plain_receipt.payload, Some(json!({"testName": "smoke"})));
    workflow.close().await;

    let reopened = workflow_store(home.path()).await;
    assert!(
        reopened
            .get_receipt("receipt-plain")
            .await
            .expect("read receipt after reopen")
            .is_some()
    );
    assert_eq!(
        reopened
            .get_receipt("receipt-zstd")
            .await
            .expect("read compressed receipt")
            .expect("compressed receipt")
            .status,
        "blocked"
    );
    reopened.close().await;
}

#[tokio::test]
async fn duplicate_receipt_is_idempotent_and_divergent_duplicate_marks_generation_dirty() {
    let home = TempDir::new().expect("temporary home");
    let thread_id = ThreadId::from_u128(2);
    let first_path = rollout_path(home.path(), "rollout-first.jsonl");
    write_lines(
        &first_path,
        [
            session_meta(thread_id, home.path()),
            receipt_line(
                1,
                receipt_item(thread_id, "receipt-duplicate", ReceiptStatus::Pass),
            ),
        ],
    );
    let second_path = rollout_path(home.path(), "rollout-second.jsonl");
    write_lines(
        &second_path,
        [
            session_meta(thread_id, home.path()),
            receipt_line(
                1,
                receipt_item(thread_id, "receipt-duplicate", ReceiptStatus::Fail),
            ),
        ],
    );
    let workflow = workflow_store(home.path()).await;
    let generation = workflow
        .begin_search_generation()
        .await
        .expect("begin generation");
    super::super::search_index_projection::project_rollout_into_generation(
        &workflow,
        &first_path,
        thread_id,
        generation.generation_id,
        super::super::search_index::SearchProjectionCursor::default(),
        metadata(),
    )
    .await
    .expect("project first receipt");
    super::super::search_index_projection::project_rollout_into_generation(
        &workflow,
        &first_path,
        thread_id,
        generation.generation_id,
        super::super::search_index::SearchProjectionCursor::default(),
        metadata(),
    )
    .await
    .expect("repeat identical receipt");
    let second_rollout_id = ThreadId::from_u128(22);
    let error = super::super::search_index_projection::project_rollout_into_generation(
        &workflow,
        &second_path,
        second_rollout_id,
        generation.generation_id,
        super::super::search_index::SearchProjectionCursor::default(),
        metadata(),
    )
    .await
    .expect_err("divergent receipt must block generation publication");
    assert!(error.to_string().contains("receipt projection conflict"));
    let journal_status = sqlx::query_scalar::<_, String>(
        "SELECT status FROM workflow_backfill_journal WHERE rollout_id = ?",
    )
    .bind(second_rollout_id.to_string())
    .fetch_optional(workflow.pool())
    .await
    .expect("read conflict journal")
    .expect("conflict journal row");
    assert_eq!(journal_status, "dirty");
    assert_eq!(
        workflow
            .get_receipt("receipt-duplicate")
            .await
            .expect("read canonical duplicate")
            .expect("canonical receipt")
            .status,
        "pass"
    );
    workflow.close().await;
}

#[tokio::test]
async fn live_receipt_projection_is_best_effort_and_marks_conflict_dirty() {
    let home = TempDir::new().expect("temporary home");
    let thread_id = ThreadId::from_u128(3);
    let path = rollout_path(home.path(), "rollout-live.jsonl");
    write_lines(
        &path,
        [
            session_meta(thread_id, home.path()),
            receipt_line(
                1,
                receipt_item(thread_id, "receipt-live", ReceiptStatus::Pass),
            ),
        ],
    );
    let config = super::super::test_support::test_config(home.path());
    let runtime =
        codex_state::StateRuntime::init(config.sqlite.clone(), "test-provider".to_string())
            .await
            .expect("open state runtime");
    let store = LocalThreadStore::new(config, Some(runtime.clone()));
    super::super::search_index::project_live_rollout(&store, thread_id, thread_id, &path).await;
    assert!(
        runtime
            .workflow()
            .get_receipt("receipt-live")
            .await
            .expect("read live receipt")
            .is_some()
    );
    write_lines(
        &path,
        [
            session_meta(thread_id, home.path()),
            receipt_line(
                1,
                receipt_item(thread_id, "receipt-live", ReceiptStatus::Pass),
            ),
            receipt_line(
                2,
                receipt_item(thread_id, "receipt-live", ReceiptStatus::Fail),
            ),
        ],
    );
    super::super::search_index::project_live_rollout(&store, thread_id, thread_id, &path).await;
    let journal_status = sqlx::query_scalar::<_, String>(
        "SELECT status FROM workflow_backfill_journal WHERE rollout_id = ?",
    )
    .bind(thread_id.to_string())
    .fetch_optional(runtime.workflow().pool())
    .await
    .expect("read live conflict journal")
    .expect("live conflict journal");
    assert_eq!(journal_status, "dirty");
    runtime.close().await;
}
