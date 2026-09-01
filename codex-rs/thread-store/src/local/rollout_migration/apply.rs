//! Resumable historical rollout apply coordinator.
//!
//! Preview owns discovery. This module consumes that frozen report, records a
//! fenced workflow journal, delegates canonicalization to the existing
//! migration state machine, and builds an unpublished FTS generation.

use codex_state::WorkflowBackfillBeginRequest;
use codex_state::WorkflowBackfillFinalizeRequest;
use codex_state::WorkflowBackfillStatus;

use super::LocalThreadStore;
use super::RolloutMigrationMode;
use super::RolloutMigrationOptions;
use super::RolloutMigrationPaths;
use super::RolloutMigrationProgress;
use super::RolloutMigrationReport;
use super::RolloutMigrationStatus;
use super::migration_error;
use crate::ThreadStoreError;
use crate::ThreadStoreResult;

use super::apply_report;
use super::apply_support;
use super::apply_support::attest_completed_frozen_sources;
use super::apply_support::attest_frozen_sources;
use super::apply_support::generation_for_apply;
use super::apply_support::include_entry;
use super::apply_support::legacy_names;
use super::apply_support::process_entry;
use super::apply_support::source_stat;
use super::apply_support::workflow_watermark;

const APPLY_OWNER_PREFIX: &str = "rollout-migration";
const JOURNAL_LEASE_MS: i64 = 5 * 60 * 1_000;

/// Run one apply pass from a single frozen preview report.
pub(super) async fn run(
    store: &LocalThreadStore,
    options: RolloutMigrationOptions,
    on_progress: &mut impl FnMut(RolloutMigrationProgress),
    limiter: &mut super::RolloutMigrationRateLimiter,
    paths: RolloutMigrationPaths,
) -> ThreadStoreResult<RolloutMigrationReport> {
    if options.mode != RolloutMigrationMode::Apply {
        return Err(ThreadStoreError::InvalidRequest {
            message: "resumable rollout apply requires Apply mode".to_string(),
        });
    }
    let _maintenance_guard =
        codex_rollout::try_acquire_rollout_maintenance_lock(&store.config.codex_home)
            .map_err(migration_error)?
            .ok_or_else(|| ThreadStoreError::Conflict {
                message: "rollout compression or another migration is already running".to_string(),
            })?;
    let Some(state_db) = store.state_db.as_ref() else {
        return Err(ThreadStoreError::Unsupported {
            operation: "resumable_rollout_apply",
        });
    };
    let workflow = state_db.workflow().clone();
    let RolloutMigrationPaths::Frozen(preview) = paths else {
        return Err(ThreadStoreError::InvalidRequest {
            message: "rollout apply requires a frozen preview".to_string(),
        });
    };
    preview
        .validate_frozen()
        .map_err(|message| ThreadStoreError::Conflict {
            message: format!("frozen rollout preview is stale (canRecover=false): {message}"),
        })?;
    let Some(preview_digest) = preview.provenance_digest.as_deref() else {
        return Err(ThreadStoreError::Conflict {
            message: "frozen rollout preview is missing its provenance digest".to_string(),
        });
    };
    let watermark = workflow_watermark(preview.watermark.as_ref());
    let state = workflow
        .get_backfill_coordinator_state()
        .await
        .map_err(migration_error)?;
    if state.status == WorkflowBackfillStatus::Complete {
        if state.watermark.as_ref() != watermark.as_ref() {
            return Err(ThreadStoreError::Conflict {
                message: "a different rollout backfill is already complete; drain incremental capture before applying again".to_string(),
            });
        }
        let journals = workflow
            .list_backfill_journal()
            .await
            .map_err(migration_error)?;
        let completed_entries = preview
            .entries
            .iter()
            .filter(|entry| include_entry(entry, &options))
            .collect::<Vec<_>>();
        if completed_entries.is_empty() {
            return Ok(RolloutMigrationReport::default());
        }
        if journals_match_preview(&completed_entries, &journals, preview_digest) {
            attest_completed_frozen_sources(store, &completed_entries, &journals).await?;
            return report_for_completed_entries(
                &preview,
                completed_entries,
                &workflow,
                on_progress,
            )
            .await;
        }
        return Err(ThreadStoreError::Conflict {
            message: "completed rollout backfill belongs to a different preview digest".to_string(),
        });
    }
    // Reattestation is complete before this exact path set is handed to the migration state
    // machine. No discovery path is available from this point onward.
    let known_paths = RolloutMigrationPaths::Known(attest_frozen_sources(store, &preview).await?);
    let RolloutMigrationPaths::Known(attested_paths) = known_paths else {
        return Err(ThreadStoreError::Internal {
            message: "frozen rollout attestation did not produce known paths".to_string(),
        });
    };
    let entries = preview
        .entries
        .iter()
        .zip(attested_paths)
        .filter(|(entry, _path)| include_entry(entry, &options))
        .collect::<Vec<_>>();
    let mut report = RolloutMigrationReport::default();
    if entries.is_empty() {
        return Ok(report);
    }
    let Some(watermark) = watermark else {
        for (entry, _path) in entries {
            report.outcomes.push(apply_report::failed_outcome(
                entry,
                super::RolloutMigrationFailureReason::InvalidSessionMetadata,
                "preview did not produce a valid rollout watermark",
            ));
        }
        return Ok(report);
    };

    let now_ms = chrono::Utc::now().timestamp_millis();
    if state.status == WorkflowBackfillStatus::Processing
        && state
            .lease_expires_at_ms
            .is_some_and(|expires_at_ms| expires_at_ms <= now_ms)
    {
        workflow
            .reclaim_expired_backfill(now_ms)
            .await
            .map_err(migration_error)?;
    }
    workflow
        .reclaim_expired_backfill_journal(now_ms)
        .await
        .map_err(migration_error)?;
    let owner_id = format!("{APPLY_OWNER_PREFIX}-{}-{}", std::process::id(), now_ms);
    let claim = workflow
        .begin_backfill(&WorkflowBackfillBeginRequest {
            watermark: watermark.clone(),
            owner_id: owner_id.clone(),
            lease_duration_ms: JOURNAL_LEASE_MS,
        })
        .await
        .map_err(|error| ThreadStoreError::Conflict {
            message: format!("unable to claim rollout backfill: {error}"),
        })?;

    let mut registered = Vec::with_capacity(entries.len());
    for (entry, path) in &entries {
        let rollout_id = entry.rollout_id.map_or_else(
            || format!("invalid-{}", registered.len()),
            |id| id.to_string(),
        );
        let (size, mtime) = source_stat(path).await;
        let journal = workflow
            .register_backfill_rollout(&codex_state::WorkflowBackfillJournalCreate {
                rollout_id: rollout_id.clone(),
                source_path: path.to_string_lossy().to_string(),
                source_size_bytes: size,
                source_mtime_ms: mtime,
            })
            .await
            .map_err(migration_error)?;
        registered.push((entry, rollout_id, Some(path.clone()), journal));
    }
    let journals = workflow
        .list_backfill_journal()
        .await
        .map_err(migration_error)?;
    let generation_id = generation_for_apply(&workflow, &watermark, &journals).await?;
    let entry_refs = entries
        .iter()
        .map(|(entry, _path)| *entry)
        .collect::<Vec<_>>();
    let legacy_names = legacy_names(store, &entry_refs).await;
    let total_paths = registered.len();
    let mut context = apply_support::ApplyContext {
        store,
        workflow: &workflow,
        owner_id: &owner_id,
        generation_id,
        legacy_names: &legacy_names,
        preview_digest,
        options: &options,
        limiter,
    };
    for (index, (entry, rollout_id, path, journal)) in registered.into_iter().enumerate() {
        let outcome =
            process_entry(&mut context, entry, rollout_id.as_str(), path, journal).await?;
        let outcome_status = Some(outcome.status);
        report.outcomes.push(outcome);
        on_progress(RolloutMigrationProgress {
            processed_paths: index + 1,
            total_paths,
            outcome_status,
        });
    }

    let can_publish = report.outcomes.iter().all(|outcome| {
        !matches!(
            outcome.status,
            RolloutMigrationStatus::Failed | RolloutMigrationStatus::SkippedBusy
        )
    });
    if can_publish {
        workflow
            .finalize_backfill(&WorkflowBackfillFinalizeRequest {
                owner_id,
                token: claim.token,
                generation: claim.generation,
            })
            .await
            .map_err(|error| ThreadStoreError::Conflict {
                message: format!("rollout backfill remains incomplete: {error}"),
            })?;
        if !workflow
            .publish_search_generation_atomic(generation_id)
            .await
            .map_err(migration_error)?
        {
            return Err(ThreadStoreError::Conflict {
                message: "rollout search generation was not building".to_string(),
            });
        }
        workflow
            .request_incremental_backfill(&watermark)
            .await
            .map_err(migration_error)?;
    } else {
        release_backfill_claim(&workflow, &owner_id, &claim.token, claim.generation).await?;
    }
    report.apply_receipt = Some(apply_report::apply_receipt(
        &preview,
        &report,
        Some(generation_id),
    ));
    Ok(report)
}

fn journals_match_preview(
    entries: &[&super::RolloutMigrationPreviewEntry],
    journals: &[codex_state::WorkflowBackfillJournalEntry],
    preview_digest: &str,
) -> bool {
    entries.iter().enumerate().all(|(index, entry)| {
        let rollout_id = entry
            .rollout_id
            .map_or_else(|| format!("invalid-{index}"), |id| id.to_string());
        journals.iter().any(|journal| {
            journal.rollout_id == rollout_id
                && journal.status.is_terminal()
                && journal
                    .cursor_json
                    .as_deref()
                    .and_then(|cursor| serde_json::from_str::<serde_json::Value>(cursor).ok())
                    .and_then(|cursor| {
                        cursor
                            .get("previewDigest")
                            .and_then(serde_json::Value::as_str)
                            .map(str::to_owned)
                    })
                    .as_deref()
                    == Some(preview_digest)
        })
    })
}

async fn report_for_completed_entries(
    preview: &super::RolloutMigrationPreviewReport,
    entries: Vec<&super::RolloutMigrationPreviewEntry>,
    workflow: &codex_state::WorkflowStore,
    on_progress: &mut impl FnMut(RolloutMigrationProgress),
) -> ThreadStoreResult<RolloutMigrationReport> {
    let journals = workflow
        .list_backfill_journal()
        .await
        .map_err(migration_error)?;
    let total_paths = entries.len();
    let mut report = RolloutMigrationReport::default();
    for (index, entry) in entries.into_iter().enumerate() {
        let rollout_id = entry
            .rollout_id
            .map_or_else(|| format!("invalid-{index}"), |id| id.to_string());
        let status = journals
            .iter()
            .find(|journal| journal.rollout_id == rollout_id)
            .map(|journal| journal.status);
        let outcome = match status {
            Some(codex_state::WorkflowBackfillJournalStatus::Complete) => {
                apply_report::simple_outcome(entry, RolloutMigrationStatus::AlreadyPaginated)
            }
            Some(codex_state::WorkflowBackfillJournalStatus::SkippedPermanent) => {
                apply_report::simple_outcome(entry, RolloutMigrationStatus::SkippedEmpty)
            }
            _ => apply_report::failed_outcome(
                entry,
                super::RolloutMigrationFailureReason::InterruptedMigrationRecoveryFailed,
                "completed rollout backfill journal is unavailable",
            ),
        };
        let outcome_status = Some(outcome.status);
        report.outcomes.push(outcome);
        on_progress(RolloutMigrationProgress {
            processed_paths: index + 1,
            total_paths,
            outcome_status,
        });
    }
    let generation_id = journals
        .iter()
        .filter_map(|journal| journal.generation_id)
        .next();
    report.apply_receipt = Some(apply_report::apply_receipt(preview, &report, generation_id));
    Ok(report)
}

async fn release_backfill_claim(
    workflow: &codex_state::WorkflowStore,
    owner_id: &str,
    token: &str,
    generation: i64,
) -> ThreadStoreResult<()> {
    sqlx::query(
        "UPDATE workflow_backfill_state
         SET status = 'recoverable', owner_id = NULL, owner_token = NULL,
             lease_id = NULL, lease_expires_at_ms = NULL,
             updated_at_ms = ?, generation = generation + 1
         WHERE id = 1 AND status = 'processing' AND owner_id = ?
           AND owner_token = ? AND generation = ?",
    )
    .bind(chrono::Utc::now().timestamp_millis())
    .bind(owner_id)
    .bind(token)
    .bind(generation)
    .execute(workflow.pool())
    .await
    .map_err(migration_error)?;
    Ok(())
}

#[cfg(test)]
#[path = "apply_tests.rs"]
mod tests;
