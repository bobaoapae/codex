//! CLI-facing rollout migration reports.
//!
//! The thread store owns the read-only preview and apply result types. This module combines them
//! into one stable command report so JSON and human output expose the same frozen accounting.

use std::path::Path;
use std::time::Duration;

use codex_thread_store::FrozenPreview;
use codex_thread_store::RolloutMigrationApplyReceipt;
use codex_thread_store::RolloutMigrationOutcome;
use codex_thread_store::RolloutMigrationPreviewEntry;
use codex_thread_store::RolloutMigrationPreviewReport;
use codex_thread_store::RolloutMigrationReport;
use codex_thread_store::RolloutMigrationStatus;
use serde::Serialize;

const MAX_EXCEPTION_DETAILS: usize = 20;

/// Complete output of `codex migrate-rollouts`.
///
/// Preview fields are retained for apply runs as the frozen, read-only accounting captured before
/// the state-backed coordinator starts. Apply-only fields remain optional so the default preview
/// remains a report and cannot be mistaken for a persisted operation.
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct CliMigrationReport {
    #[serde(flatten)]
    pub(crate) preview: RolloutMigrationPreviewReport,
    pub(crate) mode: &'static str,
    pub(crate) max_mib_per_second: Option<u64>,
    pub(crate) duration_ms: u64,
    pub(crate) apply_status: String,
    pub(crate) generation_id: Option<i64>,
    pub(crate) permanent_skips: usize,
    /// Alias retained at the top level for shell/JSON consumers that do not inspect `counts`.
    pub(crate) eligible: usize,
    pub(crate) skipped: usize,
    pub(crate) skips: usize,
    pub(crate) busy: usize,
    pub(crate) invalid: usize,
    pub(crate) malformed: usize,
    pub(crate) pending: usize,
    pub(crate) skipped_internal_receipts: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) outcomes: Option<Vec<RolloutMigrationOutcome>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) apply_receipt: Option<RolloutMigrationApplyReceipt>,
    /// Preview-only receipt-shaped metadata. It is deliberately never written to local storage.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) receipt: Option<RolloutMigrationApplyReceipt>,
}

/// Load and validate a durable preview report supplied to `--apply`.
pub(crate) async fn load_frozen_preview(path: &Path) -> anyhow::Result<FrozenPreview> {
    let bytes = tokio::fs::read(path).await.map_err(|error| {
        anyhow::anyhow!("failed to read preview report {}: {error}", path.display())
    })?;
    let value: serde_json::Value = serde_json::from_slice(&bytes).map_err(|error| {
        anyhow::anyhow!(
            "preview report {} is not valid JSON: {error}",
            path.display()
        )
    })?;
    if value.get("mode").and_then(serde_json::Value::as_str) != Some("preview") {
        anyhow::bail!(
            "preview report {} must be produced by a read-only preview",
            path.display()
        );
    }
    if value.get("applyStatus").and_then(serde_json::Value::as_str) != Some("preview") {
        anyhow::bail!(
            "preview report {} has a non-preview apply status",
            path.display()
        );
    }
    let preview: FrozenPreview = serde_json::from_value(value).map_err(|error| {
        anyhow::anyhow!(
            "preview report {} has invalid frozen data: {error}",
            path.display()
        )
    })?;
    preview
        .validate_frozen()
        .map_err(|error| anyhow::anyhow!("preview report {} is stale: {error}", path.display()))?;
    Ok(preview)
}

/// Save a machine-readable preview for a later explicit apply.
pub(crate) async fn save_json_report(
    report: &CliMigrationReport,
    path: &Path,
) -> anyhow::Result<()> {
    let bytes = serde_json::to_vec_pretty(report)?;
    tokio::fs::write(path, bytes).await.map_err(|error| {
        anyhow::anyhow!("failed to save preview report {}: {error}", path.display())
    })
}

impl CliMigrationReport {
    pub(crate) fn for_preview(
        preview: RolloutMigrationPreviewReport,
        max_mib_per_second: Option<u64>,
        elapsed: Duration,
    ) -> Self {
        let counts = &preview.counts;
        let permanent_skips = preview_permanent_skips(&preview);
        let receipt = RolloutMigrationApplyReceipt {
            schema_version: 1,
            kind: "rollout.migration.preview".to_string(),
            subject: format!(
                "rollout-migration:{}",
                preview.provenance_digest.as_deref().unwrap_or("unknown")
            ),
            status: "informational".to_string(),
            watermark: preview.watermark.clone(),
            generation_id: None,
            completed: 0,
            skipped_permanent: permanent_skips,
            recoverable: counts.busy,
            failed: 0,
        };
        Self {
            eligible: counts.eligible,
            skipped: counts.skipped,
            skips: counts.skipped,
            busy: counts.busy,
            invalid: counts.invalid,
            malformed: counts.malformed,
            pending: preview.pending_markers,
            skipped_internal_receipts: preview.skipped_internal_receipts,
            mode: "preview",
            max_mib_per_second,
            duration_ms: elapsed.as_millis().try_into().unwrap_or(u64::MAX),
            apply_status: "preview".to_string(),
            generation_id: None,
            permanent_skips,
            outcomes: None,
            apply_receipt: None,
            receipt: Some(receipt),
            preview,
        }
    }

    pub(crate) fn for_apply(
        preview: RolloutMigrationPreviewReport,
        apply: RolloutMigrationReport,
        max_mib_per_second: Option<u64>,
        elapsed: Duration,
    ) -> Self {
        let counts = &preview.counts;
        let apply_receipt = apply
            .apply_receipt
            .as_ref()
            .map(|receipt| effective_apply_receipt(&preview, receipt));
        let apply_status = apply_receipt
            .as_ref()
            .map(|receipt| receipt.status.clone())
            .unwrap_or_else(|| "complete".to_string());
        let generation_id = apply_receipt
            .as_ref()
            .and_then(|receipt| receipt.generation_id);
        let permanent_skips = apply
            .outcomes
            .iter()
            .filter(|outcome| outcome.status == RolloutMigrationStatus::SkippedEmpty)
            .count();
        Self {
            eligible: counts.eligible,
            skipped: counts.skipped,
            skips: counts.skipped,
            busy: counts.busy,
            invalid: counts.invalid,
            malformed: counts.malformed,
            pending: preview.pending_markers,
            skipped_internal_receipts: preview.skipped_internal_receipts,
            mode: "apply",
            max_mib_per_second,
            duration_ms: elapsed.as_millis().try_into().unwrap_or(u64::MAX),
            apply_status,
            generation_id,
            permanent_skips,
            outcomes: Some(apply.outcomes),
            apply_receipt,
            receipt: None,
            preview,
        }
    }
}

/// Pending journal markers make a pass recoverable even if every selected source was processed.
/// Keep the receipt status conservative so no partial apply is presented as a pass.
pub(crate) fn effective_apply_receipt(
    preview: &RolloutMigrationPreviewReport,
    apply_receipt: &RolloutMigrationApplyReceipt,
) -> RolloutMigrationApplyReceipt {
    let mut receipt = apply_receipt.clone();
    if receipt.status == "complete" && preview.pending_markers > 0 {
        receipt.status = "recoverable".to_string();
        receipt.recoverable = receipt.recoverable.max(preview.pending_markers);
    }
    receipt
}

pub(crate) fn write_report(
    report: &CliMigrationReport,
    json: bool,
    verbose: bool,
    thread_storage: Option<(u64, u64)>,
) -> anyhow::Result<()> {
    if json {
        println!("{}", serde_json::to_string_pretty(report)?);
    } else {
        print_human_report(report, verbose, thread_storage);
    }
    Ok(())
}

fn print_human_report(
    report: &CliMigrationReport,
    verbose: bool,
    thread_storage: Option<(u64, u64)>,
) {
    let completion = if report.mode == "preview" {
        "Preview complete"
    } else {
        "Migration complete"
    };
    println!(
        "{completion} in {}.",
        super::format_elapsed(Duration::from_millis(report.duration_ms))
    );
    println!("Mode: {}.", report.mode);
    match report.preview.watermark.as_ref() {
        Some(watermark) => println!(
            "Frozen watermark: created_at={}, rollout_id={}",
            watermark.created_at,
            watermark
                .rollout_id
                .map_or_else(|| "none".to_string(), |id| id.to_string())
        ),
        None => println!("Frozen watermark: none"),
    }
    println!(
        "Preview provenance digest: {}.",
        report
            .preview
            .provenance_digest
            .as_deref()
            .unwrap_or("missing")
    );
    let counts = &report.preview.counts;
    println!(
        "Thread classes: interactive={}, sub_agent={}, transient_job={}, internal={}, legacy_exec={}",
        counts.interactive,
        counts.sub_agent,
        counts.transient_job,
        counts.internal,
        counts.legacy_exec,
    );
    println!(
        "Rollouts: {} eligible, {} skipped, {} busy, {} invalid, {} malformed, {} pending.",
        report.eligible,
        report.skipped,
        report.busy,
        report.invalid,
        report.malformed,
        report.pending,
    );
    println!(
        "Skipped internal migration receipts: {}.",
        report.skipped_internal_receipts
    );
    println!(
        "Bytes: plain={}, zst={}, canonical={}, temporary-space-estimate={}",
        super::format_bytes(report.preview.plain_bytes),
        super::format_bytes(report.preview.zst_bytes),
        super::format_bytes(report.preview.canonical_bytes),
        super::format_bytes(report.preview.estimated_temp_space_bytes),
    );
    println!(
        "Items: indexable={}, excluded={}, malformed={}, trailing_partial={}, skipped={}",
        report.preview.indexable_allowlisted_items,
        report.preview.excluded_items,
        report.preview.malformed_items,
        report.preview.trailing_partial_items,
        report.preview.skipped_items,
    );
    let rate = report
        .max_mib_per_second
        .map_or_else(|| "unlimited".to_string(), |rate| format!("{rate} MiB/s"));
    let estimate = report
        .preview
        .estimated_duration_ms
        .map_or_else(|| "unavailable".to_string(), format_duration_ms);
    println!(
        "Rate: {rate}; estimated duration: {estimate}; generation: {}; apply status: {}; permanent skips: {}.",
        report
            .generation_id
            .map_or_else(|| "none".to_string(), |id| id.to_string()),
        report.apply_status,
        report.permanent_skips,
    );
    if let Some((before, after)) = thread_storage {
        println!(
            "Disk used for thread storage: {} -> {}",
            super::format_bytes(before),
            super::format_bytes(after)
        );
    }
    if report.mode == "preview" {
        println!("Receipt: informational preview metadata only; nothing was persisted.");
        println!(
            "Save the JSON report with `codex migrate-rollouts --json > rollout-migration-preview.json`, then run `codex migrate-rollouts --apply --preview-report rollout-migration-preview.json`."
        );
    }

    let Some(outcomes) = report.outcomes.as_ref() else {
        if verbose {
            for entry in &report.preview.entries {
                print_preview_entry(entry);
            }
        }
        return;
    };
    if verbose {
        for outcome in outcomes {
            print_outcome(outcome);
        }
        return;
    }
    let exceptions = outcomes.iter().filter(|outcome| {
        matches!(
            outcome.status,
            RolloutMigrationStatus::SkippedBusy | RolloutMigrationStatus::Failed
        )
    });
    let exception_count = exceptions.clone().count();
    if exception_count == 0 {
        return;
    }
    println!();
    for outcome in exceptions.take(MAX_EXCEPTION_DETAILS) {
        print_outcome(outcome);
    }
    if exception_count > MAX_EXCEPTION_DETAILS {
        println!(
            "... and {} more; rerun with --json for the complete report.",
            exception_count - MAX_EXCEPTION_DETAILS
        );
    }
}

fn print_preview_entry(entry: &RolloutMigrationPreviewEntry) {
    let status = format!("{:?}", entry.status).to_ascii_lowercase();
    let class = entry.class.map_or_else(
        || "unknown".to_string(),
        |class| format!("{class:?}").to_ascii_lowercase(),
    );
    let thread_id = entry
        .thread_id
        .map_or_else(|| "unknown".to_string(), |thread_id| thread_id.to_string());
    match &entry.message {
        Some(message) => println!("{status}\t{class}\t{thread_id}\t{message}"),
        None => println!("{status}\t{class}\t{thread_id}"),
    }
}

fn print_outcome(outcome: &RolloutMigrationOutcome) {
    let status = format!("{:?}", outcome.status).to_ascii_lowercase();
    let thread_id = outcome
        .thread_id
        .map_or_else(|| "unknown".to_string(), |thread_id| thread_id.to_string());
    match &outcome.message {
        Some(message) => println!("{status}\t{thread_id}\t{message}"),
        None => println!("{status}\t{thread_id}"),
    }
}

fn preview_permanent_skips(report: &RolloutMigrationPreviewReport) -> usize {
    report
        .entries
        .iter()
        .filter(|entry| {
            matches!(
                entry.status,
                codex_thread_store::RolloutMigrationPreviewStatus::Invalid
                    | codex_thread_store::RolloutMigrationPreviewStatus::Malformed
                    | codex_thread_store::RolloutMigrationPreviewStatus::Skipped
            )
        })
        .count()
}

fn format_duration_ms(duration_ms: u64) -> String {
    if duration_ms < 1_000 {
        return format!("{duration_ms}ms");
    }
    super::format_elapsed(Duration::from_millis(duration_ms))
}
