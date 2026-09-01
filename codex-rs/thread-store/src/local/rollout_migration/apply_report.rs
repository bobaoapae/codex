//! Report and receipt projections for an apply pass.

use super::RolloutMigrationApplyReceipt;
use super::RolloutMigrationFailureReason;
use super::RolloutMigrationOutcome;
use super::RolloutMigrationPreviewEntry;
use super::RolloutMigrationPreviewReport;
use super::RolloutMigrationReport;
use super::RolloutMigrationStatus;

pub(super) fn simple_outcome(
    entry: &RolloutMigrationPreviewEntry,
    status: RolloutMigrationStatus,
) -> RolloutMigrationOutcome {
    RolloutMigrationOutcome {
        thread_id: entry.thread_id,
        rollout_path: entry.rollout_path.clone(),
        status,
        failure_reason: None,
        bytes_processed: 0,
        message: None,
    }
}

pub(super) fn skipped_busy_outcome(
    entry: &RolloutMigrationPreviewEntry,
    message: &str,
) -> RolloutMigrationOutcome {
    RolloutMigrationOutcome {
        thread_id: entry.thread_id,
        rollout_path: entry.rollout_path.clone(),
        status: RolloutMigrationStatus::SkippedBusy,
        failure_reason: None,
        bytes_processed: 0,
        message: Some(message.to_string()),
    }
}

pub(super) fn failed_outcome(
    entry: &RolloutMigrationPreviewEntry,
    reason: RolloutMigrationFailureReason,
    message: impl Into<String>,
) -> RolloutMigrationOutcome {
    RolloutMigrationOutcome {
        thread_id: entry.thread_id,
        rollout_path: entry.rollout_path.clone(),
        status: RolloutMigrationStatus::Failed,
        failure_reason: Some(reason),
        bytes_processed: 0,
        message: Some(message.into()),
    }
}

pub(super) fn apply_receipt(
    preview: &RolloutMigrationPreviewReport,
    report: &RolloutMigrationReport,
    generation_id: Option<i64>,
) -> RolloutMigrationApplyReceipt {
    let completed = report
        .outcomes
        .iter()
        .filter(|outcome| {
            matches!(
                outcome.status,
                RolloutMigrationStatus::Migrated | RolloutMigrationStatus::AlreadyPaginated
            )
        })
        .count();
    let skipped_permanent = report
        .outcomes
        .iter()
        .filter(|outcome| outcome.status == RolloutMigrationStatus::SkippedEmpty)
        .count();
    let recoverable = report
        .outcomes
        .iter()
        .filter(|outcome| outcome.status == RolloutMigrationStatus::SkippedBusy)
        .count();
    let failed = report
        .outcomes
        .iter()
        .filter(|outcome| outcome.status == RolloutMigrationStatus::Failed)
        .count();
    let status = if failed != 0 {
        "failed"
    } else if recoverable != 0 {
        "recoverable"
    } else {
        "complete"
    };
    RolloutMigrationApplyReceipt {
        schema_version: 1,
        kind: "rollout.migration.apply".to_string(),
        subject: format!(
            "rollout-migration:{}",
            preview.provenance_digest.as_deref().unwrap_or("unknown")
        ),
        status: status.to_string(),
        watermark: preview.watermark.clone(),
        generation_id,
        completed,
        skipped_permanent,
        recoverable,
        failed,
    }
}
