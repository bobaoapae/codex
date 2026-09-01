//! Read-only discovery and sizing for legacy rollout migration.

use super::line_parser::parse_legacy_rollout_value;
use super::preview_types::RolloutMigrationPreviewEntry;
use super::preview_types::RolloutMigrationPreviewOptions;
use super::preview_types::RolloutMigrationPreviewReport;
use super::preview_types::RolloutMigrationPreviewRepresentation;
use super::preview_types::RolloutMigrationPreviewStatus;
use super::preview_types::RolloutMigrationPreviewThreadClass;
use super::preview_types::RolloutMigrationPreviewWatermark;
use crate::ThreadStoreError;
use crate::ThreadStoreResult;
use crate::local::LocalThreadStore;
use crate::local::search_index_extractor::ExtractRecord;
use crate::local::search_index_extractor::deduplicate_candidates;
use crate::local::search_index_extractor::extract_candidates;
use crate::local::search_index_extractor::extract_receipt_candidate;
use chrono::DateTime;
use codex_protocol::ThreadId;
use codex_protocol::protocol::SessionMeta;
use codex_protocol::protocol::SessionSource;
use codex_protocol::protocol::ThreadHistoryMode;
use codex_protocol::protocol::ThreadSource;
use codex_rollout::RolloutItem;
use serde_json::Value;
use sha2::Digest;
use sha2::Sha256;
use std::collections::BTreeMap;
use std::collections::HashSet;
use std::fs::OpenOptions;
use std::io;
use std::io::Read;
use std::io::SeekFrom;
use std::path::Path;
use std::path::PathBuf;
use std::time::SystemTime;
use tokio::io::AsyncReadExt;
use tokio::io::AsyncSeekExt;

const MAX_ROLLOUT_LINE_BYTES: usize = 16 * 1024 * 1024;
const WRITER_LOCK_DIRECTORY: &str = "thread-writer-locks";
const MIGRATION_JOURNAL_DIRECTORY: &str = "rollout-migrations";

struct PhysicalRollout {
    path: PathBuf,
    plain_path: PathBuf,
    rollout_id: Option<ThreadId>,
    filename_created_at: Option<String>,
    source_size_bytes: Option<u64>,
    source_mtime_ms: Option<i64>,
    representation: RolloutMigrationPreviewRepresentation,
    plain_bytes: u64,
    zst_bytes: u64,
}

struct ScannedRollout {
    metadata: Option<SessionMeta>,
    source_content_digest: String,
    canonical_bytes: u64,
    indexable_items: usize,
    excluded_items: usize,
    malformed_items: usize,
    trailing_partial_items: usize,
    skipped_items: usize,
}

pub(super) async fn run(
    store: &LocalThreadStore,
    options: RolloutMigrationPreviewOptions,
) -> ThreadStoreResult<RolloutMigrationPreviewReport> {
    validate_rate(options.max_mib_per_second)?;
    let sources = discover_sources(store.config.codex_home.as_path()).await?;
    run_sources(store, options, sources).await
}

/// Build a preview from a caller-provided path set.
///
/// Startup already owns a path snapshot and may use this helper to retain its existing retry
/// behavior. Manual apply never calls this function: it consumes the digest-bound preview that
/// the caller supplied instead of taking another filesystem snapshot.
pub(super) async fn run_for_paths(
    store: &LocalThreadStore,
    options: RolloutMigrationPreviewOptions,
    paths: Vec<PathBuf>,
) -> ThreadStoreResult<RolloutMigrationPreviewReport> {
    validate_rate(options.max_mib_per_second)?;
    let mut resolved = Vec::with_capacity(paths.len());
    for path in paths {
        if tokio::fs::try_exists(&path).await.map_err(io_error)? {
            resolved.push(path);
        } else if let Some(current_path) =
            super::find_current_rollout_path(&store.config.codex_home, &path).await?
        {
            resolved.push(current_path);
        }
    }
    let sources = sources_from_paths(resolved).await?;
    run_sources(store, options, sources).await
}

async fn run_sources(
    store: &LocalThreadStore,
    options: RolloutMigrationPreviewOptions,
    sources: Vec<PhysicalRollout>,
) -> ThreadStoreResult<RolloutMigrationPreviewReport> {
    let (pending_thread_ids, pending_markers) =
        pending_markers(store.config.codex_home.as_path()).await?;
    let mut report = RolloutMigrationPreviewReport {
        pending_markers,
        ..Default::default()
    };

    for source in &sources {
        let entry = scan_source(store, source, &pending_thread_ids, &options.thread_ids).await?;
        accumulate(&mut report, &entry);
        report.entries.push(entry);
    }

    report.watermark = sources
        .iter()
        .zip(report.entries.iter())
        .rev()
        .find(|(_, entry)| entry.status != RolloutMigrationPreviewStatus::SkippedInternalReceipt)
        .map(|(source, _entry)| RolloutMigrationPreviewWatermark {
            created_at: source.filename_created_at.clone().unwrap_or_default(),
            rollout_id: source.rollout_id,
        });
    report.estimated_duration_ms = estimate_duration_ms(
        report.plain_bytes.saturating_add(report.zst_bytes),
        options.max_mib_per_second,
    );
    report.freeze().map_err(io_error)
}

async fn discover_sources(root: &Path) -> ThreadStoreResult<Vec<PhysicalRollout>> {
    let mut physical_paths = Vec::new();
    for directory in [
        root.join(codex_rollout::SESSIONS_SUBDIR),
        root.join(codex_rollout::ARCHIVED_SESSIONS_SUBDIR),
    ] {
        collect_physical_paths(&directory, &mut physical_paths).await?;
    }

    let mut selected = BTreeMap::<PathBuf, PathBuf>::new();
    for path in physical_paths {
        let compressed = is_compressed(&path);
        let plain_path = codex_rollout::plain_rollout_path(&path);
        match selected.entry(plain_path) {
            std::collections::btree_map::Entry::Vacant(entry) => {
                entry.insert(path);
            }
            std::collections::btree_map::Entry::Occupied(mut entry)
                if !compressed && is_compressed(entry.get()) =>
            {
                entry.insert(path);
            }
            std::collections::btree_map::Entry::Occupied(_) => {}
        }
    }

    sources_from_selected_paths(selected).await
}

async fn sources_from_paths(paths: Vec<PathBuf>) -> ThreadStoreResult<Vec<PhysicalRollout>> {
    let mut selected = BTreeMap::<PathBuf, PathBuf>::new();
    for path in paths {
        let plain_path = codex_rollout::plain_rollout_path(&path);
        let compressed = is_compressed(&path);
        match selected.entry(plain_path) {
            std::collections::btree_map::Entry::Vacant(entry) => {
                entry.insert(path);
            }
            std::collections::btree_map::Entry::Occupied(mut entry)
                if !compressed && is_compressed(entry.get()) =>
            {
                entry.insert(path);
            }
            std::collections::btree_map::Entry::Occupied(_) => {}
        }
    }
    sources_from_selected_paths(selected).await
}

async fn sources_from_selected_paths(
    selected: BTreeMap<PathBuf, PathBuf>,
) -> ThreadStoreResult<Vec<PhysicalRollout>> {
    let mut sources = Vec::with_capacity(selected.len());
    for (plain_path, path) in selected {
        let metadata = tokio::fs::metadata(&path).await.map_err(io_error)?;
        let bytes = metadata.len();
        let compressed = is_compressed(&path);
        sources.push(PhysicalRollout {
            rollout_id: codex_rollout::rollout_id_from_path(&path),
            filename_created_at: filename_created_at(&path),
            source_size_bytes: Some(bytes),
            source_mtime_ms: modified_at_ms(&metadata),
            representation: if compressed {
                RolloutMigrationPreviewRepresentation::Zstd
            } else {
                RolloutMigrationPreviewRepresentation::Plain
            },
            plain_bytes: if !compressed { bytes } else { 0 },
            zst_bytes: if compressed { bytes } else { 0 },
            path,
            plain_path,
        });
    }
    sources.sort_by(|left, right| {
        left.filename_created_at
            .cmp(&right.filename_created_at)
            .then_with(|| {
                left.rollout_id
                    .map(|id| id.to_string())
                    .cmp(&right.rollout_id.map(|id| id.to_string()))
            })
            .then_with(|| left.path.cmp(&right.path))
    });
    Ok(sources)
}

async fn collect_physical_paths(
    directory: &Path,
    paths: &mut Vec<PathBuf>,
) -> ThreadStoreResult<()> {
    let mut pending = vec![directory.to_path_buf()];
    while let Some(directory) = pending.pop() {
        let mut entries = match tokio::fs::read_dir(&directory).await {
            Ok(entries) => entries,
            Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
            Err(error) => return Err(io_error(error)),
        };
        while let Some(entry) = entries.next_entry().await.map_err(io_error)? {
            let file_type = entry.file_type().await.map_err(io_error)?;
            if file_type.is_dir() {
                pending.push(entry.path());
                continue;
            }
            if !file_type.is_file() {
                continue;
            }
            let name = entry.file_name();
            let Some(name) = name.to_str() else {
                continue;
            };
            if name.starts_with("rollout-")
                && (name.ends_with(".jsonl") || name.ends_with(".jsonl.zst"))
            {
                paths.push(entry.path());
            }
        }
    }
    Ok(())
}

async fn pending_markers(root: &Path) -> ThreadStoreResult<(HashSet<ThreadId>, usize)> {
    let directory = root.join(MIGRATION_JOURNAL_DIRECTORY);
    let mut entries = match tokio::fs::read_dir(directory).await {
        Ok(entries) => entries,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok((HashSet::new(), 0)),
        Err(error) => return Err(io_error(error)),
    };
    let mut ids = HashSet::new();
    let mut count = 0;
    while let Some(entry) = entries.next_entry().await.map_err(io_error)? {
        if !entry.file_type().await.map_err(io_error)?.is_file() {
            continue;
        }
        let Some(name) = entry.file_name().to_str().map(str::to_string) else {
            continue;
        };
        let Some(id) = name.strip_suffix(".pending") else {
            continue;
        };
        count += 1;
        if let Ok(id) = ThreadId::from_string(id) {
            ids.insert(id);
        }
    }
    Ok((ids, count))
}

async fn scan_source(
    store: &LocalThreadStore,
    source: &PhysicalRollout,
    pending_thread_ids: &HashSet<ThreadId>,
    selected_thread_ids: &[ThreadId],
) -> ThreadStoreResult<RolloutMigrationPreviewEntry> {
    let scan = scan_rollout(&source.path).await?;
    let metadata = scan.metadata.as_ref();
    let thread_id = metadata.map(|metadata| metadata.id);
    // An empty rollout has no SessionMeta yet, but its filename still carries the writer's
    // thread identity. Recover that identity only for lock/selection checks when the scan found
    // no records at all; a malformed or retired-only file must remain unidentified and fail
    // closed in the report and apply journal.
    let rollout_thread_id = thread_id.or_else(|| {
        (scan.canonical_bytes == 0 && scan.malformed_items == 0 && scan.skipped_items == 0)
            .then(|| super::thread_id_from_rollout_filename(&source.path))
            .flatten()
    });
    let selected = selected_thread_ids.is_empty()
        || rollout_thread_id.is_some_and(|thread_id| selected_thread_ids.contains(&thread_id));
    let busy = if let Some(thread_id) = rollout_thread_id {
        writer_is_busy(&store.config.codex_home, thread_id).await
    } else {
        false
    };
    let class = metadata.map(classify_session);
    let is_migration_receipt = metadata.is_some_and(super::is_migration_receipt_session_meta);
    let status = match metadata {
        Some(_) if is_migration_receipt => RolloutMigrationPreviewStatus::SkippedInternalReceipt,
        None if busy => RolloutMigrationPreviewStatus::Busy,
        None if scan.malformed_items > 0 => RolloutMigrationPreviewStatus::Malformed,
        None => RolloutMigrationPreviewStatus::Invalid,
        Some(_) if source.rollout_id.is_none() => RolloutMigrationPreviewStatus::Invalid,
        Some(_) if !selected => RolloutMigrationPreviewStatus::Skipped,
        Some(_) if busy => RolloutMigrationPreviewStatus::Busy,
        Some(metadata) if metadata.history_mode == ThreadHistoryMode::Paginated => {
            RolloutMigrationPreviewStatus::AlreadyPaginated
        }
        Some(_) => RolloutMigrationPreviewStatus::Eligible,
    };
    let pending_marker =
        rollout_thread_id.is_some_and(|thread_id| pending_thread_ids.contains(&thread_id));
    let estimated_temp_space_bytes = if status == RolloutMigrationPreviewStatus::Eligible {
        scan.canonical_bytes
    } else {
        Default::default()
    };
    let message = if is_migration_receipt {
        Some("internal migration receipt is excluded from migration".to_string())
    } else if busy {
        Some("active rollout writer lock was observed".to_string())
    } else if scan.malformed_items > 0 {
        Some(format!(
            "{} malformed rollout records were skipped",
            scan.malformed_items
        ))
    } else if !selected {
        Some("excluded by the requested thread selection".to_string())
    } else if metadata.is_none() {
        Some("rollout contains no valid session metadata".to_string())
    } else {
        None
    };

    Ok(RolloutMigrationPreviewEntry {
        rollout_path: source.path.clone(),
        plain_path: source.plain_path.clone(),
        source_size_bytes: source.source_size_bytes,
        source_mtime_ms: source.source_mtime_ms,
        source_content_digest: Some(scan.source_content_digest.clone()),
        representation: source.representation,
        representation_transition_allowed: source.rollout_id.is_some()
            && source.filename_created_at.is_some(),
        rollout_id: source.rollout_id,
        thread_id,
        class,
        source: metadata.map(|metadata| metadata.source.clone()),
        thread_source: metadata.and_then(|metadata| metadata.thread_source.clone()),
        history_mode: metadata.map(|metadata| metadata.history_mode),
        forked_from_id: metadata.and_then(|metadata| metadata.forked_from_id),
        history_base: metadata.and_then(|metadata| metadata.history_base),
        status,
        plain_bytes: if is_migration_receipt {
            0
        } else {
            source.plain_bytes
        },
        zst_bytes: if is_migration_receipt {
            0
        } else {
            source.zst_bytes
        },
        canonical_bytes: if is_migration_receipt {
            0
        } else {
            scan.canonical_bytes
        },
        estimated_temp_space_bytes,
        indexable_allowlisted_items: if is_migration_receipt {
            0
        } else {
            scan.indexable_items
        },
        excluded_items: if is_migration_receipt {
            0
        } else {
            scan.excluded_items
        },
        malformed_items: if is_migration_receipt {
            0
        } else {
            scan.malformed_items
        },
        trailing_partial_items: if is_migration_receipt {
            0
        } else {
            scan.trailing_partial_items
        },
        skipped_items: if is_migration_receipt {
            0
        } else {
            scan.skipped_items
        },
        pending_marker: pending_marker && !is_migration_receipt,
        busy: busy && !is_migration_receipt,
        message,
    })
}

async fn scan_rollout(path: &Path) -> ThreadStoreResult<ScannedRollout> {
    let mut reader = codex_rollout::open_rollout_line_reader(path)
        .await
        .map_err(io_error)?;
    let mut metadata = None;
    let mut records = Vec::new();
    let mut receipt_candidates = Vec::new();
    let mut canonical_bytes = 0_u64;
    let mut malformed_items = 0_usize;
    let mut skipped_items = 0_usize;
    let mut parsed_items = 0_usize;
    let mut next_ordinal = 0_u64;
    let mut last_line_malformed = false;
    let mut content_hasher = Sha256::new();

    while let Some(line) = reader.next_line().await.map_err(io_error)? {
        content_hasher.update(line.as_bytes());
        content_hasher.update([b'\n']);
        if line.len() > MAX_ROLLOUT_LINE_BYTES {
            malformed_items += 1;
            last_line_malformed = true;
            continue;
        }
        let value = match serde_json::from_str::<Value>(&line) {
            Ok(value) => value,
            Err(_) => {
                malformed_items += 1;
                last_line_malformed = true;
                continue;
            }
        };
        let parsed = match parse_legacy_rollout_value(value.clone()) {
            Ok(Some(parsed)) => parsed,
            Ok(None) => {
                skipped_items += 1;
                last_line_malformed = false;
                continue;
            }
            Err(_) => {
                malformed_items += 1;
                last_line_malformed = true;
                continue;
            }
        };
        last_line_malformed = false;
        let ordinal = parsed.ordinal.unwrap_or(next_ordinal);
        next_ordinal = ordinal.saturating_add(1);
        if let RolloutItem::SessionMeta(session_meta) = &parsed.item
            && metadata.is_none()
        {
            metadata = Some(session_meta.meta.clone());
        }
        canonical_bytes = canonical_bytes.saturating_add(
            u64::try_from(serde_json::to_vec(&parsed).map_err(io_error)?.len())
                .unwrap_or(u64::MAX)
                .saturating_add(1),
        );
        if let Some(candidate) =
            extract_receipt_candidate(&value, ordinal, parse_timestamp_ms(&parsed.timestamp))
        {
            receipt_candidates.push((ordinal, candidate));
        }
        records.push(ExtractRecord {
            ordinal,
            event_time_ms: parse_timestamp_ms(&parsed.timestamp),
            item: parsed.item,
        });
        parsed_items += 1;
    }

    let visible_from = metadata
        .as_ref()
        .map(|metadata| {
            metadata
                .history_base
                .map_or(0, |base| base.end_ordinal_exclusive)
                .max(metadata.subagent_history_start_ordinal.unwrap_or(0))
        })
        .unwrap_or(0);
    records.retain(|record| record.ordinal >= visible_from);
    receipt_candidates.retain(|(ordinal, _)| *ordinal >= visible_from);
    let mut candidates = extract_candidates(records);
    candidates.extend(
        receipt_candidates
            .into_iter()
            .map(|(_, candidate)| candidate),
    );
    let indexable_items = deduplicate_candidates(candidates).len();
    let excluded_items = malformed_items
        .saturating_add(skipped_items)
        .saturating_add(parsed_items.saturating_sub(indexable_items));
    Ok(ScannedRollout {
        metadata,
        source_content_digest: format!("sha256:{:x}", content_hasher.finalize()),
        canonical_bytes,
        indexable_items,
        excluded_items,
        malformed_items,
        trailing_partial_items: usize::from(last_line_malformed && !has_final_newline(path).await?),
        skipped_items,
    })
}

async fn has_final_newline(path: &Path) -> ThreadStoreResult<bool> {
    if is_compressed(path) {
        let path = path.to_path_buf();
        return tokio::task::spawn_blocking(move || {
            let input = std::fs::File::open(path).map_err(io_error)?;
            let mut decoder = zstd::stream::read::Decoder::new(input).map_err(io_error)?;
            let mut buffer = [0_u8; 64 * 1024];
            let mut last = None;
            loop {
                let count = decoder.read(&mut buffer).map_err(io_error)?;
                if count == 0 {
                    break;
                }
                last = Some(buffer[count - 1]);
            }
            Ok(last == Some(b'\n'))
        })
        .await
        .map_err(io_error)?;
    }

    let mut file = tokio::fs::File::open(path).await.map_err(io_error)?;
    let length = file.metadata().await.map_err(io_error)?.len();
    if length == 0 {
        return Ok(false);
    }
    file.seek(SeekFrom::End(-1)).await.map_err(io_error)?;
    let mut byte = [0_u8; 1];
    file.read_exact(&mut byte).await.map_err(io_error)?;
    Ok(byte[0] == b'\n')
}

async fn writer_is_busy(codex_home: &Path, thread_id: ThreadId) -> bool {
    let path = codex_home
        .join(WRITER_LOCK_DIRECTORY)
        .join(format!("{thread_id}.lock"));
    tokio::task::spawn_blocking(move || {
        let file = OpenOptions::new().read(true).write(true).open(path);
        let Ok(file) = file else {
            return false;
        };
        match file.try_lock() {
            Ok(()) => false,
            Err(std::fs::TryLockError::WouldBlock) => true,
            Err(std::fs::TryLockError::Error(_)) => false,
        }
    })
    .await
    .unwrap_or(false)
}

fn classify_session(metadata: &SessionMeta) -> RolloutMigrationPreviewThreadClass {
    if matches!(metadata.source, SessionSource::Exec) {
        // Historical `exec` rollouts are never reclassified heuristically as transient jobs.
        return RolloutMigrationPreviewThreadClass::LegacyExec;
    }
    if metadata.source.is_internal() {
        return RolloutMigrationPreviewThreadClass::Internal;
    }
    if metadata.source.is_non_root_agent()
        || matches!(metadata.thread_source, Some(ThreadSource::Subagent))
    {
        return RolloutMigrationPreviewThreadClass::SubAgent;
    }
    if let Some(ThreadSource::Feature(feature)) = metadata.thread_source.as_ref()
        && matches!(
            feature.as_str(),
            "transient_job" | "transient-job" | "transientJob"
        )
    {
        return RolloutMigrationPreviewThreadClass::TransientJob;
    }
    RolloutMigrationPreviewThreadClass::Interactive
}

fn filename_created_at(path: &Path) -> Option<String> {
    let name = path.file_name()?.to_str()?;
    let name = name.strip_suffix(".zst").unwrap_or(name);
    let name = name.strip_prefix("rollout-")?;
    let timestamp = name.get(..19)?;
    (name.get(19..20) == Some("-")).then(|| timestamp.to_string())
}

fn modified_at_ms(metadata: &std::fs::Metadata) -> Option<i64> {
    metadata
        .modified()
        .ok()
        .and_then(|time| time.duration_since(SystemTime::UNIX_EPOCH).ok())
        .and_then(|duration| i64::try_from(duration.as_millis()).ok())
}

fn is_compressed(path: &Path) -> bool {
    path.file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| name.ends_with(".jsonl.zst"))
}

fn parse_timestamp_ms(value: &str) -> Option<i64> {
    DateTime::parse_from_rfc3339(value)
        .ok()
        .map(|timestamp| timestamp.timestamp_millis())
}

fn validate_rate(rate: Option<u64>) -> ThreadStoreResult<()> {
    if rate == Some(0) {
        return Err(ThreadStoreError::InvalidRequest {
            message: "--max-mib-per-second must be a positive supported integer".to_string(),
        });
    }
    Ok(())
}

fn estimate_duration_ms(bytes: u64, rate: Option<u64>) -> Option<u64> {
    let bytes_per_second = rate?.checked_mul(1024 * 1024)?;
    let numerator = bytes.checked_mul(1_000)?;
    Some(numerator.div_ceil(bytes_per_second))
}

fn accumulate(report: &mut RolloutMigrationPreviewReport, entry: &RolloutMigrationPreviewEntry) {
    if entry.status == RolloutMigrationPreviewStatus::SkippedInternalReceipt {
        report.skipped_internal_receipts = report.skipped_internal_receipts.saturating_add(1);
        report.counts.skipped_internal_receipts =
            report.counts.skipped_internal_receipts.saturating_add(1);
        return;
    }
    report.plain_bytes = report.plain_bytes.saturating_add(entry.plain_bytes);
    report.zst_bytes = report.zst_bytes.saturating_add(entry.zst_bytes);
    report.canonical_bytes = report.canonical_bytes.saturating_add(entry.canonical_bytes);
    report.estimated_temp_space_bytes = report
        .estimated_temp_space_bytes
        .saturating_add(entry.estimated_temp_space_bytes);
    report.indexable_allowlisted_items = report
        .indexable_allowlisted_items
        .saturating_add(entry.indexable_allowlisted_items);
    report.excluded_items = report.excluded_items.saturating_add(entry.excluded_items);
    report.malformed_items = report.malformed_items.saturating_add(entry.malformed_items);
    report.trailing_partial_items = report
        .trailing_partial_items
        .saturating_add(entry.trailing_partial_items);
    report.skipped_items = report.skipped_items.saturating_add(entry.skipped_items);
    if let Some(class) = entry.class {
        match class {
            RolloutMigrationPreviewThreadClass::Interactive => report.counts.interactive += 1,
            RolloutMigrationPreviewThreadClass::SubAgent => report.counts.sub_agent += 1,
            RolloutMigrationPreviewThreadClass::TransientJob => report.counts.transient_job += 1,
            RolloutMigrationPreviewThreadClass::Internal => report.counts.internal += 1,
            RolloutMigrationPreviewThreadClass::LegacyExec => report.counts.legacy_exec += 1,
        }
    }
    match entry.status {
        RolloutMigrationPreviewStatus::Eligible => report.counts.eligible += 1,
        RolloutMigrationPreviewStatus::AlreadyPaginated
        | RolloutMigrationPreviewStatus::Skipped => report.counts.skipped += 1,
        RolloutMigrationPreviewStatus::Busy => report.counts.busy += 1,
        RolloutMigrationPreviewStatus::Invalid => report.counts.invalid += 1,
        RolloutMigrationPreviewStatus::Malformed => report.counts.malformed += 1,
        RolloutMigrationPreviewStatus::SkippedInternalReceipt => unreachable!(),
    }
}

fn io_error(error: impl std::fmt::Display) -> ThreadStoreError {
    ThreadStoreError::Internal {
        message: format!("rollout migration preview failed: {error}"),
    }
}

#[cfg(test)]
#[path = "preview_tests.rs"]
mod tests;
