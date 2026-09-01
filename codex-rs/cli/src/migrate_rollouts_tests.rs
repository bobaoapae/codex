use std::time::Duration;

use clap::Parser;
use codex_core::config::ConfigBuilder;
use codex_protocol::ThreadId;
use codex_protocol::models::BaseInstructions;
use codex_protocol::protocol::SessionSource;
use codex_protocol::protocol::ThreadHistoryMode;
use codex_rollout::RolloutRecorder;
use codex_rollout::RolloutRecorderParams;
use codex_thread_store::LocalThreadStore;
use codex_thread_store::LocalThreadStoreConfig;
use codex_thread_store::RolloutMigrationApplyReceipt;
use codex_thread_store::RolloutMigrationPreviewOptions;
use codex_thread_store::RolloutMigrationPreviewReport;
use serde_json::Value;
use tempfile::TempDir;

use super::MigrateRolloutsCommand;
use super::receipt::persist_apply_receipt;
use super::receipt::rollout_id_for_watermark;
use super::report::CliMigrationReport;
use super::report::effective_apply_receipt;

#[test]
fn migration_command_defaults_to_read_only_preview() {
    let command = MigrateRolloutsCommand::try_parse_from(["codex"]).expect("command parses");
    assert!(!command.apply);
    assert!(!command.json);
    assert!(!command.verbose);
    assert!(command.thread.is_empty());
    assert_eq!(command.max_mib_per_second, None);
}

#[test]
fn migration_command_accepts_explicit_apply_and_rate_limit() {
    let command = MigrateRolloutsCommand::try_parse_from([
        "codex",
        "--preview-report",
        "preview.json",
        "--apply",
        "--json",
        "--thread",
        "01a05464-12ca-75c3-b7a8-856c95a3aaee",
        "--max-mib-per-second",
        "4",
    ])
    .expect("explicit apply command parses");
    assert!(command.apply);
    assert!(command.json);
    assert_eq!(command.max_mib_per_second, Some(4));
    assert_eq!(command.thread.len(), 1);
}

#[test]
fn migration_command_rejects_zero_rate_limit() {
    assert!(
        MigrateRolloutsCommand::try_parse_from(["codex", "--max-mib-per-second", "0"]).is_err()
    );
}

#[test]
fn preview_report_serializes_required_metadata_and_informational_receipt() {
    let report = CliMigrationReport::for_preview(
        RolloutMigrationPreviewReport::default(),
        Some(2),
        Duration::from_millis(7),
    );
    let value = serde_json::to_value(report).expect("report serializes");
    for field in [
        "watermark",
        "counts",
        "plainBytes",
        "zstBytes",
        "canonicalBytes",
        "estimatedTempSpaceBytes",
        "indexableAllowlistedItems",
        "excludedItems",
        "estimatedDurationMs",
        "maxMibPerSecond",
        "durationMs",
        "generationId",
        "applyStatus",
        "permanentSkips",
        "pending",
        "skippedInternalReceipts",
        "receipt",
    ] {
        assert!(value.get(field).is_some(), "missing JSON field {field}");
    }
    assert_eq!(value["mode"], Value::String("preview".to_string()));
    assert_eq!(value["applyStatus"], Value::String("preview".to_string()));
    assert_eq!(
        value["receipt"]["status"],
        Value::String("informational".to_string())
    );
}

#[test]
fn apply_receipt_is_recoverable_when_pending_markers_remain() {
    let preview = RolloutMigrationPreviewReport {
        pending_markers: 1,
        ..Default::default()
    };
    let receipt = RolloutMigrationApplyReceipt {
        schema_version: 1,
        kind: "rollout.migration.apply".to_string(),
        subject: "rollout-migration".to_string(),
        status: "complete".to_string(),
        watermark: None,
        generation_id: Some(7),
        completed: 2,
        skipped_permanent: 0,
        recoverable: 0,
        failed: 0,
    };
    let effective = effective_apply_receipt(&preview, &receipt);
    assert_eq!(effective.status, "recoverable");
    assert_eq!(effective.recoverable, 1);
}

#[tokio::test]
async fn migration_receipt_is_marked_skipped_and_idempotent() -> anyhow::Result<()> {
    let home = TempDir::new()?;
    let config = ConfigBuilder::default()
        .codex_home(home.path().to_path_buf())
        .build()
        .await?;
    let source_thread_id = ThreadId::from_u128(1);
    let ordinary = RolloutRecorder::new(
        &codex_rollout::RolloutConfig::from_view(&config),
        RolloutRecorderParams::new(
            source_thread_id,
            /*forked_from_id*/ None,
            /*parent_thread_id*/ None,
            SessionSource::Cli,
            None,
            "migration-test".to_string(),
            BaseInstructions::default(),
            Vec::new(),
        )
        .with_history_mode(ThreadHistoryMode::Legacy),
    )
    .await?;
    ordinary.persist().await?;
    let ordinary_path = ordinary.rollout_path().to_path_buf();
    let ordinary_rollout_id = codex_rollout::rollout_id_from_path(&ordinary_path)
        .expect("ordinary rollout should have an ID");
    ordinary.shutdown().await?;
    let filename = ordinary_path
        .file_name()
        .expect("ordinary rollout should have a filename")
        .to_string_lossy();
    let created_at = filename
        .strip_prefix("rollout-")
        .and_then(|name| name.get(..19))
        .expect("ordinary rollout should have a timestamp")
        .to_string();
    let apply_receipt = RolloutMigrationApplyReceipt {
        schema_version: 1,
        kind: "rollout.migration.apply".to_string(),
        subject: "rollout-migration".to_string(),
        status: "complete".to_string(),
        watermark: Some(codex_thread_store::RolloutMigrationPreviewWatermark {
            created_at,
            rollout_id: Some(ordinary_rollout_id),
        }),
        generation_id: Some(7),
        completed: 1,
        skipped_permanent: 0,
        recoverable: 0,
        failed: 0,
    };

    persist_apply_receipt(&config, &apply_receipt).await?;
    let receipt_rollout_id = rollout_id_for_watermark(
        apply_receipt
            .watermark
            .as_ref()
            .expect("receipt watermark should exist"),
    )
    .expect("receipt rollout ID should be deterministic");
    let receipt_path = codex_rollout::find_rollout_path_by_rollout_id(
        config.codex_home.as_path(),
        receipt_rollout_id,
    )
    .await?
    .expect("migration receipt rollout should be readable");
    let entries_before = std::fs::read_dir(
        receipt_path
            .parent()
            .expect("receipt rollout should have a parent"),
    )?
    .count();
    assert!(
        tokio::fs::read_to_string(&receipt_path)
            .await?
            .contains("receipt.attached")
    );
    let metadata = codex_rollout::read_session_meta_line(&receipt_path).await?;
    assert!(metadata.meta.source.is_internal());
    assert_eq!(metadata.meta.originator, "codex-cli-migration");

    persist_apply_receipt(&config, &apply_receipt).await?;
    let entries_after = std::fs::read_dir(
        receipt_path
            .parent()
            .expect("receipt rollout should have a parent"),
    )?
    .count();
    assert_eq!(entries_after, entries_before);

    let store = LocalThreadStore::new(
        LocalThreadStoreConfig::from_config(&config),
        /*state_db*/ None,
    );
    let preview = store
        .preview_rollout_migration(RolloutMigrationPreviewOptions::default())
        .await?;
    assert_eq!(preview.skipped_internal_receipts, 1);
    assert_eq!(
        preview
            .watermark
            .as_ref()
            .and_then(|watermark| watermark.rollout_id),
        Some(ordinary_rollout_id)
    );
    Ok(())
}
