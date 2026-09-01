//! Durable, bounded receipt for an explicit rollout migration apply.

use anyhow::Context;
use chrono::SecondsFormat;
use chrono::Utc;
use codex_core::config::Config;
use codex_extension_items::ExtensionItem;
use codex_extension_items::receipt::ReceiptAttachedItem;
use codex_extension_items::receipt::ReceiptStatus;
use codex_protocol::ThreadId;
use codex_protocol::items::TurnItem;
use codex_protocol::models::BaseInstructions;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::InternalSessionSource;
use codex_protocol::protocol::ItemCompletedEvent;
use codex_protocol::protocol::SessionSource;
use codex_protocol::protocol::ThreadHistoryMode;
use codex_protocol::protocol::ThreadSource;
use codex_rollout::RolloutItem;
use codex_rollout::RolloutRecorder;
use codex_rollout::RolloutRecorderParams;
use codex_thread_store::RolloutMigrationApplyReceipt;
use codex_thread_store::RolloutMigrationPreviewWatermark;
use serde_json::json;
use uuid::Uuid;

const RECEIPT_NAMESPACE: Uuid = Uuid::from_u128(0x8f2e_7b0a_3c21_4e4c_9c1a_6b11_2d3f_7701);

/// Append the one canonical migration receipt for a completed or partial apply.
///
/// The rollout and receipt IDs are derived from the frozen preview digest (with a watermark
/// fallback for legacy callers). Looking up the deterministic rollout ID before writing makes
/// retries idempotent while the maintenance lock closes the check/write race between concurrent
/// explicit apply commands.
pub(crate) async fn persist_apply_receipt(
    config: &Config,
    apply_receipt: &RolloutMigrationApplyReceipt,
) -> anyhow::Result<()> {
    let Some(watermark) = apply_receipt.watermark.as_ref() else {
        return Ok(());
    };
    let preview_digest = apply_receipt
        .subject
        .strip_prefix("rollout-migration:")
        .filter(|value| value.starts_with("sha256:"));
    let thread_id = preview_digest
        .and_then(rollout_id_for_preview_digest)
        .or_else(|| rollout_id_for_watermark(watermark))
        .context("deterministic migration receipt ID was not a valid thread ID")?;
    let identity = preview_digest.map_or_else(
        || {
            watermark
                .rollout_id
                .map(|source_rollout_id| format!("{}:{source_rollout_id}", watermark.created_at))
                .unwrap_or_else(|| watermark.created_at.clone())
        },
        |digest| format!("digest:{digest}"),
    );
    let receipt_id = format!("rollout-migration:{identity}");

    let _maintenance_guard =
        codex_rollout::try_acquire_rollout_maintenance_lock(config.codex_home.as_path())
            .context("failed to acquire rollout maintenance lock for migration receipt")?
            .ok_or_else(|| {
                anyhow::anyhow!("rollout maintenance is busy while writing migration receipt")
            })?;
    if codex_rollout::find_rollout_path_by_rollout_id(config.codex_home.as_path(), thread_id)
        .await
        .context("failed to check for an existing migration receipt rollout")?
        .is_some()
    {
        return Ok(());
    }

    let status = match apply_receipt.status.as_str() {
        "complete" => ReceiptStatus::Pass,
        "recoverable" | "pending" => ReceiptStatus::Inconclusive,
        "failed" => ReceiptStatus::Fail,
        _ => ReceiptStatus::Informational,
    };
    let created_at = Utc::now().to_rfc3339_opts(SecondsFormat::Millis, true);
    let mut receipt = ReceiptAttachedItem::new(
        receipt_id.clone(),
        u64::from(apply_receipt.schema_version),
        "rollout.migration.apply",
        "local rollout migration",
        status,
        created_at,
        "codex.cli",
    )
    .context("failed to construct migration receipt")?;
    receipt.thread_id = Some(thread_id.to_string());
    receipt.turn_id = Some(receipt_id.clone());
    receipt.provenance = Some(json!({
        "watermark": apply_receipt.watermark,
        "generationId": apply_receipt.generation_id,
        "previewDigest": preview_digest,
    }));
    receipt.metadata = Some(json!({
        "completed": apply_receipt.completed,
        "skippedPermanent": apply_receipt.skipped_permanent,
        "recoverable": apply_receipt.recoverable,
        "failed": apply_receipt.failed,
    }));
    receipt
        .validate()
        .context("migration receipt exceeded bounded metadata limits")?;

    let rollout_config = codex_rollout::RolloutConfig::from_view(config);
    let recorder = RolloutRecorder::new(
        &rollout_config,
        RolloutRecorderParams::new(
            thread_id,
            /*forked_from_id*/ None,
            /*parent_thread_id*/ None,
            SessionSource::Internal(InternalSessionSource::MemoryConsolidation),
            Some(ThreadSource::Feature("rollout_migration".to_string())),
            "codex-cli-migration".to_string(),
            BaseInstructions::default(),
            Vec::new(),
        )
        .with_rollout_id(thread_id)
        // Keep this summary out of later legacy-migration passes. It is still canonical JSONL,
        // but its paginated contract makes repeated applies observe it as already materialized.
        .with_history_mode(ThreadHistoryMode::Paginated),
    )
    .await
    .context("failed to initialize migration receipt rollout")?;
    let item = RolloutItem::EventMsg(EventMsg::ItemCompleted(ItemCompletedEvent {
        thread_id,
        turn_id: receipt_id,
        item: TurnItem::Extension(ExtensionItem::ReceiptAttached(receipt)),
        started_at_ms: None,
        completed_at_ms: Utc::now().timestamp_millis(),
    }));
    recorder
        .record_canonical_items(&[item])
        .await
        .context("failed to append migration receipt")?;
    recorder
        .shutdown()
        .await
        .context("failed to flush migration receipt")?;
    Ok(())
}

pub(crate) fn rollout_id_for_watermark(
    watermark: &RolloutMigrationPreviewWatermark,
) -> Option<ThreadId> {
    let source_rollout_id = watermark.rollout_id?;
    let identity = format!("{}:{source_rollout_id}", watermark.created_at);
    let deterministic_id = Uuid::new_v5(&RECEIPT_NAMESPACE, identity.as_bytes());
    ThreadId::from_string(&deterministic_id.to_string()).ok()
}

pub(crate) fn rollout_id_for_preview_digest(digest: &str) -> Option<ThreadId> {
    let deterministic_id = Uuid::new_v5(&RECEIPT_NAMESPACE, digest.as_bytes());
    ThreadId::from_string(&deterministic_id.to_string()).ok()
}
