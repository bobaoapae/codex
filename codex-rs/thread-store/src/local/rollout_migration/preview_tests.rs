use super::super::super::test_support::test_config;
use super::super::LocalThreadStore;
use super::super::preview_types::RolloutMigrationPreviewOptions;
use codex_protocol::ThreadId;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::HistoryPosition;
use codex_protocol::protocol::InternalSessionSource;
use codex_protocol::protocol::SessionMeta;
use codex_protocol::protocol::SessionMetaLine;
use codex_protocol::protocol::SessionSource;
use codex_protocol::protocol::SubAgentSource;
use codex_protocol::protocol::ThreadHistoryMode;
use codex_protocol::protocol::ThreadSource;
use codex_protocol::protocol::UserMessageEvent;
use codex_rollout::RolloutItem;
use codex_rollout::RolloutLine;
use pretty_assertions::assert_eq;
use std::fs;
use std::io::Write;
use std::path::Path;
use std::path::PathBuf;
use tempfile::TempDir;

const FILENAME_TIMESTAMP: &str = "2025-01-03T12-00-00";
const METADATA_TIMESTAMP: &str = "2025-01-03T12:00:00Z";

fn thread_id(suffix: u128) -> ThreadId {
    ThreadId::from_string(&format!("00000000-0000-0000-0000-{suffix:012}"))
        .expect("valid thread id")
}

fn write_rollout(
    home: &Path,
    thread_id: ThreadId,
    rollout_id: ThreadId,
    source: SessionSource,
    thread_source: Option<ThreadSource>,
    history_mode: ThreadHistoryMode,
    history_base: Option<HistoryPosition>,
) -> PathBuf {
    let directory = home.join("sessions/2025/01/03");
    fs::create_dir_all(&directory).expect("create rollout directory");
    let path = directory.join(if thread_id == rollout_id {
        format!("rollout-{FILENAME_TIMESTAMP}-{thread_id}.jsonl")
    } else {
        format!("rollout-{FILENAME_TIMESTAMP}-{thread_id}_{rollout_id}.jsonl")
    });
    let metadata = SessionMeta {
        session_id: thread_id.into(),
        id: thread_id,
        timestamp: METADATA_TIMESTAMP.to_string(),
        cwd: home.to_path_buf(),
        originator: "preview-test".to_string(),
        cli_version: "preview-test".to_string(),
        source,
        thread_source,
        history_mode,
        history_base,
        model_provider: Some("preview-provider".to_string()),
        ..SessionMeta::default()
    };
    let mut file = fs::File::create(&path).expect("create rollout");
    write_line(
        &mut file,
        &RolloutItem::SessionMeta(SessionMetaLine {
            meta: metadata,
            git: None,
        }),
        0,
    );
    write_line(
        &mut file,
        &RolloutItem::EventMsg(EventMsg::UserMessage(UserMessageEvent {
            message: format!("preview message {thread_id}"),
            ..UserMessageEvent::default()
        })),
        1,
    );
    path
}

fn write_migration_receipt_rollout(home: &Path, thread_id: ThreadId) -> PathBuf {
    let directory = home.join("sessions/2025/01/04");
    fs::create_dir_all(&directory).expect("create migration receipt directory");
    let path = directory.join(format!("rollout-2025-01-04T12-00-00-{thread_id}.jsonl"));
    let metadata = SessionMeta {
        session_id: thread_id.into(),
        id: thread_id,
        timestamp: "2025-01-04T12:00:00Z".to_string(),
        cwd: home.to_path_buf(),
        originator: super::super::MIGRATION_RECEIPT_ORIGINATOR.to_string(),
        cli_version: "preview-test".to_string(),
        source: SessionSource::Internal(InternalSessionSource::MemoryConsolidation),
        thread_source: Some(ThreadSource::Feature(
            super::super::MIGRATION_RECEIPT_THREAD_SOURCE.to_string(),
        )),
        history_mode: ThreadHistoryMode::Paginated,
        model_provider: Some("preview-provider".to_string()),
        ..SessionMeta::default()
    };
    let mut file = fs::File::create(&path).expect("create migration receipt rollout");
    write_line(
        &mut file,
        &RolloutItem::SessionMeta(SessionMetaLine {
            meta: metadata,
            git: None,
        }),
        0,
    );
    path
}

fn write_line(file: &mut fs::File, item: &RolloutItem, ordinal: u64) {
    let line = RolloutLine {
        timestamp: METADATA_TIMESTAMP.to_string(),
        ordinal: Some(ordinal),
        item: item.clone(),
    };
    writeln!(
        file,
        "{}",
        serde_json::to_string(&line).expect("serialize rollout line")
    )
    .expect("write rollout line");
}

fn store(home: &TempDir) -> LocalThreadStore {
    LocalThreadStore::new(test_config(home.path()), None)
}

#[tokio::test]
async fn preview_reports_classes_and_revert_watermark_without_uuid_heuristics() {
    let home = tempfile::tempdir().expect("temporary home");
    let interactive = thread_id(1);
    let subagent = thread_id(2);
    let transient = thread_id(3);
    let internal = thread_id(4);
    let legacy_exec = thread_id(5);
    let revert_thread = thread_id(6);
    let revert_rollout = thread_id(7);
    write_rollout(
        home.path(),
        interactive,
        interactive,
        SessionSource::Cli,
        Some(ThreadSource::User),
        ThreadHistoryMode::Legacy,
        None,
    );
    write_rollout(
        home.path(),
        subagent,
        subagent,
        SessionSource::SubAgent(SubAgentSource::Other("worker".to_string())),
        Some(ThreadSource::Subagent),
        ThreadHistoryMode::Legacy,
        None,
    );
    write_rollout(
        home.path(),
        transient,
        transient,
        SessionSource::Custom("job".to_string()),
        Some(ThreadSource::Feature("transient_job".to_string())),
        ThreadHistoryMode::Legacy,
        None,
    );
    write_rollout(
        home.path(),
        internal,
        internal,
        SessionSource::Internal(InternalSessionSource::Guardian),
        None,
        ThreadHistoryMode::Legacy,
        None,
    );
    write_rollout(
        home.path(),
        legacy_exec,
        legacy_exec,
        SessionSource::Exec,
        None,
        ThreadHistoryMode::Legacy,
        None,
    );
    write_rollout(
        home.path(),
        revert_thread,
        revert_rollout,
        SessionSource::Cli,
        Some(ThreadSource::User),
        ThreadHistoryMode::Legacy,
        Some(HistoryPosition {
            thread_id: interactive,
            end_ordinal_exclusive: 1,
            end_byte_offset: 1,
        }),
    );

    let report = store(&home)
        .preview_rollout_migration(RolloutMigrationPreviewOptions::default())
        .await
        .expect("preview should succeed");

    assert_eq!(report.counts.interactive, 2);
    assert_eq!(report.counts.sub_agent, 1);
    assert_eq!(report.counts.transient_job, 1);
    assert_eq!(report.counts.internal, 1);
    assert_eq!(report.counts.legacy_exec, 1);
    assert_eq!(report.entries.len(), 6);
    assert_eq!(
        report.watermark.as_ref().unwrap().rollout_id,
        Some(revert_rollout)
    );
    let revert = report
        .entries
        .iter()
        .find(|entry| entry.thread_id == Some(revert_thread))
        .expect("revert entry should be present");
    assert_eq!(revert.rollout_id, Some(revert_rollout));
    assert_eq!(revert.history_base.as_ref().unwrap().thread_id, interactive);
}

#[tokio::test]
async fn preview_deduplicates_plain_and_reads_compressed_sources() {
    let home = tempfile::tempdir().expect("temporary home");
    let plain_thread = thread_id(10);
    let compressed_thread = thread_id(11);
    let plain_path = write_rollout(
        home.path(),
        plain_thread,
        plain_thread,
        SessionSource::Cli,
        None,
        ThreadHistoryMode::Legacy,
        None,
    );
    let compressed_only_path = write_rollout(
        home.path(),
        compressed_thread,
        compressed_thread,
        SessionSource::Cli,
        None,
        ThreadHistoryMode::Legacy,
        None,
    );
    let plain_bytes = fs::read(&plain_path).expect("read plain rollout");
    let compressed_sibling = plain_path.with_extension("jsonl.zst");
    fs::write(
        &compressed_sibling,
        zstd::stream::encode_all(plain_bytes.as_slice(), 3).expect("compress sibling"),
    )
    .expect("write compressed sibling");
    let compressed_bytes = fs::read(&compressed_only_path).expect("read second rollout");
    let compressed_only = compressed_only_path.with_extension("jsonl.zst");
    fs::write(
        &compressed_only,
        zstd::stream::encode_all(compressed_bytes.as_slice(), 3).expect("compress rollout"),
    )
    .expect("write compressed rollout");
    fs::remove_file(&compressed_only_path).expect("remove plain second rollout");

    let report = store(&home)
        .preview_rollout_migration(RolloutMigrationPreviewOptions::default())
        .await
        .expect("preview should read plain and zst");
    assert!(report.plain_bytes > 0);
    assert!(report.zst_bytes > 0);
    assert_eq!(report.entries.len(), 2);
    let plain_entry = report
        .entries
        .iter()
        .find(|entry| entry.thread_id == Some(plain_thread))
        .expect("plain source should be present");
    assert_eq!(plain_entry.zst_bytes, 0);
    assert!(
        report
            .entries
            .iter()
            .any(|entry| entry.thread_id == Some(compressed_thread) && entry.zst_bytes > 0)
    );
}

#[tokio::test]
async fn preview_counts_malformed_pending_and_does_not_write_state() {
    let home = tempfile::tempdir().expect("temporary home");
    let thread = thread_id(20);
    let path = write_rollout(
        home.path(),
        thread,
        thread,
        SessionSource::Cli,
        None,
        ThreadHistoryMode::Legacy,
        None,
    );
    let mut file = fs::OpenOptions::new()
        .append(true)
        .open(&path)
        .expect("open rollout for malformed suffix");
    file.write_all(b"{\"trailing")
        .expect("write malformed suffix");
    let marker_dir = home.path().join("rollout-migrations");
    fs::create_dir_all(&marker_dir).expect("create marker directory");
    fs::write(marker_dir.join(format!("{thread}.pending")), b"").expect("write pending marker");
    let before = fs::read(&path).expect("read source before preview");

    let report = store(&home)
        .preview_rollout_migration(RolloutMigrationPreviewOptions::default())
        .await
        .expect("preview should tolerate malformed suffix");

    assert!(report.malformed_items >= 1);
    assert!(report.trailing_partial_items >= 1);
    assert_eq!(report.pending_markers, 1);
    assert!(report.entries[0].pending_marker);
    assert_eq!(fs::read(&path).expect("read source after preview"), before);
    assert!(!home.path().join("thread-writer-locks").exists());
}

#[tokio::test]
async fn preview_marks_paginated_and_invalid_sources_and_estimates_duration() {
    let home = tempfile::tempdir().expect("temporary home");
    let paginated = thread_id(30);
    write_rollout(
        home.path(),
        paginated,
        paginated,
        SessionSource::Cli,
        None,
        ThreadHistoryMode::Paginated,
        None,
    );
    let invalid_directory = home.path().join("sessions/2025/01/03");
    let invalid_path = invalid_directory.join("rollout-not-a-valid-name.jsonl");
    fs::write(&invalid_path, b"not-json\n").expect("write invalid rollout");

    let report = store(&home)
        .preview_rollout_migration(RolloutMigrationPreviewOptions {
            max_mib_per_second: Some(1),
            ..Default::default()
        })
        .await
        .expect("preview should classify skipped sources");

    assert_eq!(report.counts.skipped, 1);
    assert_eq!(report.counts.malformed, 1);
    assert!(report.estimated_duration_ms.is_some());
    assert!(
        report
            .entries
            .iter()
            .any(|entry| entry.status == super::RolloutMigrationPreviewStatus::AlreadyPaginated)
    );
    assert!(
        report
            .entries
            .iter()
            .any(|entry| entry.status == super::RolloutMigrationPreviewStatus::Malformed)
    );
}

#[tokio::test]
async fn preview_excludes_migration_receipts_from_watermark_and_totals() {
    let home = tempfile::tempdir().expect("temporary home");
    let interactive = thread_id(40);
    let receipt_thread = thread_id(41);
    let interactive_path = write_rollout(
        home.path(),
        interactive,
        interactive,
        SessionSource::Cli,
        Some(ThreadSource::User),
        ThreadHistoryMode::Legacy,
        None,
    );
    write_migration_receipt_rollout(home.path(), receipt_thread);

    let report = store(&home)
        .preview_rollout_migration(RolloutMigrationPreviewOptions::default())
        .await
        .expect("preview should skip migration receipt rollouts");

    assert_eq!(report.skipped_internal_receipts, 1);
    assert_eq!(report.counts.skipped_internal_receipts, 1);
    assert_eq!(report.counts.internal, 0);
    assert_eq!(
        report
            .watermark
            .as_ref()
            .and_then(|watermark| watermark.rollout_id),
        Some(interactive)
    );
    assert_eq!(
        report.plain_bytes,
        fs::metadata(interactive_path)
            .expect("interactive metadata")
            .len()
    );
    let receipt = report
        .entries
        .iter()
        .find(|entry| entry.thread_id == Some(receipt_thread))
        .expect("migration receipt entry should be reported");
    assert_eq!(
        receipt.status,
        super::RolloutMigrationPreviewStatus::SkippedInternalReceipt
    );
    assert_eq!(receipt.canonical_bytes, 0);
    assert_eq!(receipt.indexable_allowlisted_items, 0);
}
