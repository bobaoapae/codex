//! Helpers for the resumable rollout apply coordinator.

use chrono::DateTime;
use chrono::NaiveDateTime;
use codex_protocol::ThreadId;
use codex_state::WorkflowBackfillJournalClaim;
use codex_state::WorkflowBackfillJournalClaimRequest;
use codex_state::WorkflowBackfillJournalStatus;
use codex_state::WorkflowBackfillJournalUpdate;
use codex_state::WorkflowBackfillWatermark;
use codex_state::WorkflowStore;
use sha2::Digest;
use sha2::Sha256;
use std::collections::HashMap;
use std::collections::HashSet;
use std::path::Path;
use std::path::PathBuf;
use std::time::SystemTime;

use super::FrozenPreview;
use super::LocalThreadStore;
use super::RolloutMigrationFailureReason;
use super::RolloutMigrationOptions;
use super::RolloutMigrationOutcome;
use super::RolloutMigrationPreviewEntry;
use super::RolloutMigrationPreviewRepresentation;
use super::RolloutMigrationPreviewStatus;
use super::RolloutMigrationPreviewWatermark;
use super::RolloutMigrationStatus;
use super::apply_report::failed_outcome;
use super::apply_report::simple_outcome;
use super::apply_report::skipped_busy_outcome;
use super::migration_error;
use crate::ThreadStoreError;
use crate::ThreadStoreResult;
use crate::local::search_index;
use crate::local::search_index_projection;

const JOURNAL_LEASE_MS: i64 = 5 * 60 * 1_000;

pub(super) struct ApplyContext<'a> {
    pub(super) store: &'a LocalThreadStore,
    pub(super) workflow: &'a WorkflowStore,
    pub(super) owner_id: &'a str,
    pub(super) generation_id: i64,
    pub(super) legacy_names: &'a HashMap<ThreadId, String>,
    pub(super) preview_digest: &'a str,
    pub(super) options: &'a RolloutMigrationOptions,
    pub(super) limiter: &'a mut super::RolloutMigrationRateLimiter,
}

pub(super) async fn process_entry(
    context: &mut ApplyContext<'_>,
    entry: &RolloutMigrationPreviewEntry,
    rollout_id: &str,
    path: Option<PathBuf>,
    journal: codex_state::WorkflowBackfillJournalEntry,
) -> ThreadStoreResult<RolloutMigrationOutcome> {
    let store = context.store;
    let workflow = context.workflow;
    let generation_id = context.generation_id;
    let legacy_names = context.legacy_names;
    let preview_digest = context.preview_digest;
    let options = context.options;
    let limiter = &mut *context.limiter;

    if journal.status == WorkflowBackfillJournalStatus::Complete && entry.pending_marker {
        if let Some(path) = path
            && let Some(outcome) = context
                .store
                .migrate_rollout_path_exact(
                    path,
                    context.options,
                    context.legacy_names,
                    context.limiter,
                )
                .await?
        {
            return Ok(outcome);
        }
        return Ok(failed_outcome(
            entry,
            RolloutMigrationFailureReason::InterruptedMigrationRecoveryFailed,
            "pending rollout migration did not produce an outcome",
        ));
    }
    if journal.status == WorkflowBackfillJournalStatus::Complete {
        return Ok(simple_outcome(
            entry,
            RolloutMigrationStatus::AlreadyPaginated,
        ));
    }
    if journal.status == WorkflowBackfillJournalStatus::SkippedPermanent {
        return Ok(simple_outcome(entry, RolloutMigrationStatus::SkippedEmpty));
    }
    let Some(claim) = context
        .workflow
        .claim_backfill_journal(&WorkflowBackfillJournalClaimRequest {
            rollout_id: rollout_id.to_string(),
            owner_id: context.owner_id.to_string(),
            lease_duration_ms: JOURNAL_LEASE_MS,
        })
        .await
        .map_err(migration_error)?
    else {
        let current = context
            .workflow
            .get_backfill_journal(rollout_id)
            .await
            .map_err(migration_error)?;
        return Ok(match current.map(|journal| journal.status) {
            Some(WorkflowBackfillJournalStatus::Complete) => {
                simple_outcome(entry, RolloutMigrationStatus::AlreadyPaginated)
            }
            Some(WorkflowBackfillJournalStatus::SkippedPermanent) => {
                simple_outcome(entry, RolloutMigrationStatus::SkippedEmpty)
            }
            Some(WorkflowBackfillJournalStatus::Failed) => failed_outcome(
                entry,
                RolloutMigrationFailureReason::Unknown,
                "rollout backfill journal contains a terminal failure",
            ),
            _ => skipped_busy_outcome(entry, "rollout backfill journal is owned by another worker"),
        });
    };

    if entry.status == RolloutMigrationPreviewStatus::Busy {
        finish_journal(
            workflow,
            &claim,
            rollout_id,
            entry.rollout_path.as_path(),
            JournalProgress {
                status: WorkflowBackfillJournalStatus::Recoverable,
                error: Some("rollout writer was busy during frozen preview".to_string()),
                generation_id,
                byte_offset: claim.entry.byte_offset,
                ordinal: claim.entry.rollout_ordinal,
                preview_digest: Some(preview_digest.to_string()),
            },
        )
        .await?;
        return Ok(skipped_busy_outcome(
            entry,
            "rollout writer was busy during frozen preview",
        ));
    }
    let empty_rollout = entry.thread_id.is_none()
        && entry.canonical_bytes == 0
        && entry.malformed_items == 0
        && entry.skipped_items == 0;
    if empty_rollout {
        finish_journal(
            workflow,
            &claim,
            rollout_id,
            entry.rollout_path.as_path(),
            JournalProgress {
                status: WorkflowBackfillJournalStatus::SkippedPermanent,
                error: None,
                generation_id,
                byte_offset: claim.entry.byte_offset,
                ordinal: claim.entry.rollout_ordinal,
                preview_digest: Some(preview_digest.to_string()),
            },
        )
        .await?;
        return Ok(simple_outcome(entry, RolloutMigrationStatus::SkippedEmpty));
    }
    let Some(path) = path else {
        finish_journal(
            workflow,
            &claim,
            rollout_id,
            entry.rollout_path.as_path(),
            JournalProgress {
                status: WorkflowBackfillJournalStatus::Failed,
                error: Some("frozen rollout source is no longer available".to_string()),
                generation_id,
                byte_offset: claim.entry.byte_offset,
                ordinal: claim.entry.rollout_ordinal,
                preview_digest: Some(preview_digest.to_string()),
            },
        )
        .await?;
        return Ok(failed_outcome(
            entry,
            RolloutMigrationFailureReason::RolloutReadFailed,
            "frozen rollout source is no longer available",
        ));
    };
    let Some(thread_id) = entry.thread_id else {
        finish_journal(
            workflow,
            &claim,
            rollout_id,
            path.as_path(),
            JournalProgress {
                status: WorkflowBackfillJournalStatus::Failed,
                error: Some("rollout contains no valid session metadata".to_string()),
                generation_id,
                byte_offset: claim.entry.byte_offset,
                ordinal: claim.entry.rollout_ordinal,
                preview_digest: Some(preview_digest.to_string()),
            },
        )
        .await?;
        return Ok(failed_outcome(
            entry,
            RolloutMigrationFailureReason::InvalidSessionMetadata,
            "rollout contains no valid session metadata",
        ));
    };

    let migration = store
        .migrate_rollout_path_exact(path, options, legacy_names, limiter)
        .await?;
    let Some(outcome) = migration else {
        finish_journal(
            workflow,
            &claim,
            rollout_id,
            entry.rollout_path.as_path(),
            JournalProgress {
                status: WorkflowBackfillJournalStatus::Failed,
                error: Some("rollout migration did not produce an outcome".to_string()),
                generation_id,
                byte_offset: claim.entry.byte_offset,
                ordinal: claim.entry.rollout_ordinal,
                preview_digest: Some(preview_digest.to_string()),
            },
        )
        .await?;
        return Ok(failed_outcome(
            entry,
            RolloutMigrationFailureReason::Unknown,
            "rollout migration did not produce an outcome",
        ));
    };
    if outcome.status == RolloutMigrationStatus::SkippedBusy {
        finish_journal(
            workflow,
            &claim,
            rollout_id,
            outcome.rollout_path.as_path(),
            JournalProgress {
                status: WorkflowBackfillJournalStatus::Recoverable,
                error: Some(
                    outcome
                        .message
                        .clone()
                        .unwrap_or_else(|| "rollout writer was busy".to_string()),
                ),
                generation_id,
                byte_offset: claim.entry.byte_offset,
                ordinal: claim.entry.rollout_ordinal,
                preview_digest: Some(preview_digest.to_string()),
            },
        )
        .await?;
        return Ok(outcome);
    }
    if outcome.status == RolloutMigrationStatus::Failed {
        finish_journal(
            workflow,
            &claim,
            rollout_id,
            outcome.rollout_path.as_path(),
            JournalProgress {
                status: WorkflowBackfillJournalStatus::Failed,
                error: Some(
                    outcome
                        .message
                        .clone()
                        .unwrap_or_else(|| "rollout migration failed".to_string()),
                ),
                generation_id,
                byte_offset: claim.entry.byte_offset,
                ordinal: claim.entry.rollout_ordinal,
                preview_digest: Some(preview_digest.to_string()),
            },
        )
        .await?;
        return Ok(outcome);
    }

    let metadata =
        search_index_projection::search_metadata(store, thread_id, &outcome.rollout_path).await?;
    let cursor = search_index::SearchProjectionCursor {
        byte_offset: u64::try_from(claim.entry.byte_offset).unwrap_or_default(),
        ordinal: u64::try_from(claim.entry.rollout_ordinal).unwrap_or_default(),
    };
    let progress = search_index::project_rollout_into_generation(
        workflow,
        &outcome.rollout_path,
        entry.rollout_id.unwrap_or(thread_id),
        generation_id,
        cursor,
        metadata,
    )
    .await?;
    let status = if progress.parse_errors == 0 {
        WorkflowBackfillJournalStatus::Complete
    } else {
        WorkflowBackfillJournalStatus::Failed
    };
    finish_journal(
        workflow,
        &claim,
        rollout_id,
        outcome.rollout_path.as_path(),
        JournalProgress {
            status,
            error: (progress.parse_errors > 0).then(|| {
                format!(
                    "{} rollout records could not be indexed",
                    progress.parse_errors
                )
            }),
            generation_id,
            byte_offset: i64::try_from(progress.next_cursor.byte_offset).unwrap_or(i64::MAX),
            ordinal: i64::try_from(progress.next_cursor.ordinal).unwrap_or(i64::MAX),
            preview_digest: Some(preview_digest.to_string()),
        },
    )
    .await?;
    if progress.parse_errors > 0 {
        return Ok(failed_outcome(
            entry,
            RolloutMigrationFailureReason::RolloutReadFailed,
            format!(
                "{} rollout records could not be indexed",
                progress.parse_errors
            ),
        ));
    }
    Ok(outcome)
}

struct JournalProgress {
    status: WorkflowBackfillJournalStatus,
    error: Option<String>,
    generation_id: i64,
    byte_offset: i64,
    ordinal: i64,
    preview_digest: Option<String>,
}

async fn finish_journal(
    workflow: &WorkflowStore,
    claim: &WorkflowBackfillJournalClaim,
    rollout_id: &str,
    path: &Path,
    progress: JournalProgress,
) -> ThreadStoreResult<()> {
    let (source_size_bytes, source_mtime_ms) = source_stat(path).await;
    let post_source_content_digest = if progress.status.is_terminal() {
        Some(logical_content_digest(path).await?)
    } else {
        None
    };
    workflow
        .update_backfill_journal(&WorkflowBackfillJournalUpdate {
            rollout_id: rollout_id.to_string(),
            owner_id: claim.owner_id.clone(),
            token: claim.token.clone(),
            generation: claim.generation,
            source_path: path.to_string_lossy().to_string(),
            byte_offset: progress.byte_offset,
            rollout_ordinal: progress.ordinal,
            status: progress.status,
            error: progress.error,
            generation_id: Some(progress.generation_id),
            cursor_json: Some(
                serde_json::json!({
                    "byteOffset": progress.byte_offset,
                    "ordinal": progress.ordinal,
                    "previewDigest": progress.preview_digest,
                    "postSourceSizeBytes": source_size_bytes
                        .and_then(|size| u64::try_from(size).ok()),
                    "postSourceMtimeMs": source_mtime_ms,
                    "postSourceContentDigest": post_source_content_digest,
                })
                .to_string(),
            ),
            source_size_bytes,
            source_mtime_ms,
            lease_duration_ms: JOURNAL_LEASE_MS,
        })
        .await
        .map_err(migration_error)?;
    Ok(())
}

pub(super) async fn generation_for_apply(
    workflow: &WorkflowStore,
    watermark: &WorkflowBackfillWatermark,
    journals: &[codex_state::WorkflowBackfillJournalEntry],
) -> ThreadStoreResult<i64> {
    let mut ids = journals
        .iter()
        .filter_map(|journal| journal.generation_id)
        .collect::<HashSet<_>>();
    if ids.len() == 1 {
        let Some(generation_id) = ids.drain().next() else {
            return Err(migration_error(
                "journal generation identity disappeared while selecting a search generation",
            ));
        };
        let state = sqlx::query_scalar::<_, String>(
            "SELECT state FROM workflow_search_generations WHERE generation_id = ?",
        )
        .bind(generation_id)
        .fetch_optional(workflow.pool())
        .await
        .map_err(migration_error)?;
        if state.as_deref() == Some("building") {
            return Ok(generation_id);
        }
    }
    let source_watermark = serde_json::to_string(watermark).map_err(migration_error)?;
    workflow
        .begin_search_generation_with_watermark(Some(&source_watermark))
        .await
        .map(|generation| generation.generation_id)
        .map_err(migration_error)
}

pub(super) async fn legacy_names(
    store: &LocalThreadStore,
    entries: &[&RolloutMigrationPreviewEntry],
) -> HashMap<ThreadId, String> {
    let ids = entries.iter().filter_map(|entry| entry.thread_id).collect();
    codex_rollout::find_thread_names_by_ids(&store.config.codex_home, &ids)
        .await
        .unwrap_or_default()
}

/// Reattest every source in a frozen preview and return the exact physical paths to use.
///
/// Resolution is deliberately limited to the path captured in the report and its paired plain
/// or compressed representation. It never walks the rollout roots or performs a fresh ID-based
/// discovery pass. A representation transition is accepted only when the report's digest-bound
/// identity allows it and the logical plain path plus rollout ID still match.
pub(super) async fn attest_frozen_sources(
    store: &LocalThreadStore,
    preview: &FrozenPreview,
) -> ThreadStoreResult<Vec<PathBuf>> {
    let mut paths = Vec::with_capacity(preview.entries.len());
    for entry in &preview.entries {
        let Some(path) = resolve_frozen_source_path(store, entry).await? else {
            return Err(stale_preview_error(format!(
                "source is missing: {}",
                entry.rollout_path.display()
            )));
        };
        attest_frozen_source(entry, path.as_path()).await?;
        paths.push(path);
    }

    let current_watermark = watermark_for_entries(preview, paths.as_slice());
    if current_watermark != preview.watermark {
        return Err(stale_preview_error(
            "frozen rollout watermark no longer matches the attested sources",
        ));
    }
    Ok(paths)
}

/// Reattest sources after a completed apply using the post-migration identity retained in each
/// terminal journal row. This keeps exact retries idempotent without allowing a later source edit
/// to hide behind the physical rewrite performed by the first apply.
pub(super) async fn attest_completed_frozen_sources(
    store: &LocalThreadStore,
    entries: &[&RolloutMigrationPreviewEntry],
    journals: &[codex_state::WorkflowBackfillJournalEntry],
) -> ThreadStoreResult<()> {
    for (index, entry) in entries.iter().enumerate() {
        let Some(path) = resolve_frozen_source_path(store, entry).await? else {
            return Err(stale_preview_error(format!(
                "completed source is missing: {}",
                entry.rollout_path.display()
            )));
        };
        attest_source_identity(entry, path.as_path()).await?;
        let rollout_id = entry
            .rollout_id
            .map_or_else(|| format!("invalid-{index}"), |id| id.to_string());
        let Some(journal) = journals
            .iter()
            .find(|journal| journal.rollout_id == rollout_id)
        else {
            return Err(stale_preview_error(format!(
                "completed source journal is missing: {}",
                entry.rollout_path.display()
            )));
        };
        let Some(cursor) = journal
            .cursor_json
            .as_deref()
            .and_then(|cursor| serde_json::from_str::<serde_json::Value>(cursor).ok())
        else {
            return Err(stale_preview_error(format!(
                "completed source journal has no post-apply identity: {}",
                entry.rollout_path.display()
            )));
        };
        let Some(expected_size) = cursor
            .get("postSourceSizeBytes")
            .and_then(serde_json::Value::as_u64)
        else {
            return Err(stale_preview_error(format!(
                "completed source journal has no post-apply size: {}",
                entry.rollout_path.display()
            )));
        };
        let Some(expected_mtime) = cursor
            .get("postSourceMtimeMs")
            .and_then(serde_json::Value::as_i64)
        else {
            return Err(stale_preview_error(format!(
                "completed source journal has no post-apply modified time: {}",
                entry.rollout_path.display()
            )));
        };
        let Some(expected_digest) = cursor
            .get("postSourceContentDigest")
            .and_then(serde_json::Value::as_str)
        else {
            return Err(stale_preview_error(format!(
                "completed source journal has no post-apply content identity: {}",
                entry.rollout_path.display()
            )));
        };
        let metadata = tokio::fs::metadata(&path).await.map_err(|error| {
            stale_preview_error(format!("completed source metadata is unavailable: {error}"))
        })?;
        let actual_mtime = modified_at_ms(&metadata);
        if metadata.len() != expected_size || actual_mtime != Some(expected_mtime) {
            return Err(stale_preview_error(format!(
                "completed source size or modified time changed: {}",
                path.display()
            )));
        }
        if logical_content_digest(&path).await? != expected_digest {
            return Err(stale_preview_error(format!(
                "completed source content changed: {}",
                path.display()
            )));
        }
    }
    Ok(())
}

async fn resolve_frozen_source_path(
    store: &LocalThreadStore,
    entry: &RolloutMigrationPreviewEntry,
) -> ThreadStoreResult<Option<PathBuf>> {
    if !is_within_codex_home(store, entry.rollout_path.as_path())
        || !is_within_codex_home(store, entry.plain_path.as_path())
    {
        return Err(stale_preview_error(format!(
            "frozen source path is outside the configured Codex home: {}",
            entry.rollout_path.display()
        )));
    }
    let expected_metadata = tokio::fs::metadata(&entry.rollout_path).await;
    if let Ok(metadata) = expected_metadata {
        if !path_matches_identity(entry, entry.rollout_path.as_path(), &metadata) {
            return Err(stale_preview_error(format!(
                "captured source identity changed: {}",
                entry.rollout_path.display()
            )));
        }
        return Ok(Some(entry.rollout_path.clone()));
    }
    let mut candidates = Vec::new();
    if entry.representation_transition_allowed {
        candidates.push(match entry.representation {
            RolloutMigrationPreviewRepresentation::Plain => {
                entry.plain_path.with_extension("jsonl.zst")
            }
            RolloutMigrationPreviewRepresentation::Zstd => entry.plain_path.clone(),
        });
    }
    for path in candidates {
        let Ok(metadata) = tokio::fs::metadata(&path).await else {
            continue;
        };
        if !path_matches_identity(entry, path.as_path(), &metadata) {
            continue;
        }
        return Ok(Some(path));
    }
    Ok(None)
}

fn is_within_codex_home(store: &LocalThreadStore, path: &Path) -> bool {
    path.strip_prefix(&store.config.codex_home).is_ok()
}

fn path_matches_identity(
    entry: &RolloutMigrationPreviewEntry,
    path: &Path,
    metadata: &std::fs::Metadata,
) -> bool {
    metadata.is_file()
        && codex_rollout::plain_rollout_path(path) == entry.plain_path
        && codex_rollout::rollout_id_from_path(path) == entry.rollout_id
}

async fn attest_frozen_source(
    entry: &RolloutMigrationPreviewEntry,
    path: &Path,
) -> ThreadStoreResult<()> {
    attest_source_identity(entry, path).await?;
    let metadata = tokio::fs::metadata(path)
        .await
        .map_err(|error| stale_preview_error(format!("source metadata is unavailable: {error}")))?;
    let current_representation = representation_for_path(path);
    let representation_changed = current_representation != entry.representation;
    let path_changed = path != entry.rollout_path;
    let current_size = metadata.len();
    let current_mtime = modified_at_ms(&metadata);
    if !representation_changed && !path_changed {
        if entry.source_size_bytes != Some(current_size) || entry.source_mtime_ms != current_mtime {
            return Err(stale_preview_error(format!(
                "source size or modified time changed: {}",
                path.display()
            )));
        }
    } else if current_mtime.is_none() {
        // A permitted representation transition can legitimately change both physical size and
        // modified time. The content hash below prevents this exception from accepting edited
        // content merely because the rollout ID and path still match.
        return Err(stale_preview_error(format!(
            "source modified time is unavailable during representation transition: {}",
            path.display()
        )));
    }
    let Some(expected_digest) = entry.source_content_digest.as_deref() else {
        return Err(stale_preview_error(format!(
            "source content identity is unavailable: {}",
            path.display()
        )));
    };
    let actual_digest = logical_content_digest(path).await?;
    if actual_digest != expected_digest {
        return Err(stale_preview_error(format!(
            "source content changed: {}",
            path.display()
        )));
    }
    Ok(())
}

async fn attest_source_identity(
    entry: &RolloutMigrationPreviewEntry,
    path: &Path,
) -> ThreadStoreResult<()> {
    let metadata = tokio::fs::metadata(path)
        .await
        .map_err(|error| stale_preview_error(format!("source metadata is unavailable: {error}")))?;
    if !metadata.is_file() {
        return Err(stale_preview_error(format!(
            "source is not a regular file: {}",
            path.display()
        )));
    }
    if codex_rollout::plain_rollout_path(path) != entry.plain_path {
        return Err(stale_preview_error(format!(
            "source logical path changed: {}",
            path.display()
        )));
    }
    if codex_rollout::rollout_id_from_path(path) != entry.rollout_id {
        return Err(stale_preview_error(format!(
            "source rollout identity changed: {}",
            path.display()
        )));
    }
    if let Some(expected_thread_id) = entry.thread_id {
        let metadata = codex_rollout::read_session_meta_line(path)
            .await
            .map_err(|error| {
                stale_preview_error(format!(
                    "source session metadata is unavailable for {path:?}: {error}"
                ))
            })?;
        if metadata.meta.id != expected_thread_id {
            return Err(stale_preview_error(format!(
                "source thread identity changed: {}",
                path.display()
            )));
        }
    }
    let current_representation = representation_for_path(path);
    let representation_changed = current_representation != entry.representation;
    let path_changed = path != entry.rollout_path;
    if (representation_changed || path_changed) && !entry.representation_transition_allowed {
        return Err(stale_preview_error(format!(
            "source representation or path changed without preview permission: {}",
            path.display()
        )));
    }
    Ok(())
}

async fn logical_content_digest(path: &Path) -> ThreadStoreResult<String> {
    let mut reader = codex_rollout::open_rollout_line_reader(path)
        .await
        .map_err(|error| stale_preview_error(format!("source cannot be read: {error}")))?;
    let mut hasher = Sha256::new();
    while let Some(line) = reader
        .next_line()
        .await
        .map_err(|error| stale_preview_error(format!("source cannot be read: {error}")))?
    {
        hasher.update(line.as_bytes());
        hasher.update([b'\n']);
    }
    Ok(format!("sha256:{:x}", hasher.finalize()))
}

fn representation_for_path(path: &Path) -> RolloutMigrationPreviewRepresentation {
    if path
        .file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| name.ends_with(".jsonl.zst"))
    {
        RolloutMigrationPreviewRepresentation::Zstd
    } else {
        RolloutMigrationPreviewRepresentation::Plain
    }
}

fn modified_at_ms(metadata: &std::fs::Metadata) -> Option<i64> {
    metadata
        .modified()
        .ok()
        .and_then(|time| time.duration_since(SystemTime::UNIX_EPOCH).ok())
        .and_then(|duration| i64::try_from(duration.as_millis()).ok())
}

fn watermark_for_entries(
    preview: &FrozenPreview,
    paths: &[PathBuf],
) -> Option<RolloutMigrationPreviewWatermark> {
    preview
        .entries
        .iter()
        .zip(paths)
        .filter(|(entry, _path)| {
            entry.status != RolloutMigrationPreviewStatus::SkippedInternalReceipt
        })
        .max_by(|(left, left_path), (right, right_path)| {
            filename_created_at(&left.plain_path)
                .cmp(&filename_created_at(&right.plain_path))
                .then_with(|| {
                    left.rollout_id
                        .map(|id| id.to_string())
                        .cmp(&right.rollout_id.map(|id| id.to_string()))
                })
                .then_with(|| left_path.cmp(right_path))
        })
        .map(|(entry, path)| RolloutMigrationPreviewWatermark {
            created_at: filename_created_at(&codex_rollout::plain_rollout_path(path))
                .unwrap_or_default(),
            rollout_id: entry.rollout_id,
        })
}

fn filename_created_at(path: &Path) -> Option<String> {
    let name = path.file_name()?.to_str()?;
    let name = name.strip_suffix(".zst").unwrap_or(name);
    let name = name.strip_prefix("rollout-")?;
    let timestamp = name.get(..19)?;
    (name.get(19..20) == Some("-")).then(|| timestamp.to_string())
}

fn stale_preview_error(message: impl Into<String>) -> ThreadStoreError {
    ThreadStoreError::Conflict {
        message: format!(
            "frozen rollout preview is stale (canRecover=false): {}",
            message.into()
        ),
    }
}

pub(super) async fn source_stat(path: &Path) -> (Option<i64>, Option<i64>) {
    let Ok(metadata) = tokio::fs::metadata(path).await else {
        return (None, None);
    };
    let size = i64::try_from(metadata.len()).ok();
    let modified = metadata
        .modified()
        .ok()
        .and_then(|time| time.duration_since(SystemTime::UNIX_EPOCH).ok())
        .and_then(|duration| i64::try_from(duration.as_millis()).ok());
    (size, modified)
}

pub(super) fn workflow_watermark(
    watermark: Option<&super::RolloutMigrationPreviewWatermark>,
) -> Option<WorkflowBackfillWatermark> {
    let watermark = watermark?;
    let rollout_id = watermark.rollout_id?.to_string();
    let created_at_ms = NaiveDateTime::parse_from_str(&watermark.created_at, "%Y-%m-%dT%H-%M-%S")
        .ok()
        .map(|time| time.and_utc().timestamp_millis())
        .or_else(|| {
            DateTime::parse_from_rfc3339(&watermark.created_at)
                .ok()
                .map(|time| time.timestamp_millis())
        })?;
    WorkflowBackfillWatermark::new(created_at_ms, rollout_id).ok()
}

pub(super) fn include_entry(
    entry: &RolloutMigrationPreviewEntry,
    options: &RolloutMigrationOptions,
) -> bool {
    if entry.status == RolloutMigrationPreviewStatus::SkippedInternalReceipt {
        return false;
    }
    // A `Skipped` entry was outside the explicit thread selection used by the preview. It must
    // never become eligible merely because apply was invoked without repeating that filter.
    if entry.status == RolloutMigrationPreviewStatus::Skipped {
        return false;
    }
    options.thread_ids.is_empty()
        || entry
            .thread_id
            .is_some_and(|thread_id| options.thread_ids.contains(&thread_id))
}
