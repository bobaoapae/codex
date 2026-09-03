//! Rollout reading, metadata projection, and generation backfill helpers.

use std::path::Path;
use std::time::SystemTime;

use chrono::DateTime;
use codex_protocol::ThreadId;
use codex_protocol::protocol::SessionMeta;
use codex_protocol::protocol::SessionSource;
use codex_state::SearchDocumentCreate;
use codex_state::SearchMetadata;
use codex_state::WorkflowBackfillJournalStatus;
use codex_state::WorkflowStore;
use codex_state::WorkflowThreadClass;
use serde_json::Value;
use tracing::warn;

use super::LocalThreadStore;
use super::helpers::rollout_path_is_archived;
use super::receipt_projection;
use super::receipt_projection::ReceiptProjectionCandidate;
use super::search_index::SearchProjectionCursor;
use super::search_index::SearchProjectionProgress;
use super::search_index_extractor::ExtractRecord;
use super::search_index_extractor::IndexedCandidate;
use super::search_index_extractor::deduplicate_candidates;
use super::search_index_extractor::extract_candidates;
use super::search_index_extractor::extract_receipt_candidate;
use crate::ThreadStoreError;
use crate::ThreadStoreResult;

const MAX_SOURCE_ID_BYTES: usize = 256;
const MAX_ERROR_BYTES: usize = 8_000;
const MAX_SOURCE_PATH_BYTES: usize = 4_096;

#[derive(Debug)]
pub(super) struct RolloutScan {
    pub(super) thread_id: ThreadId,
    pub(super) candidates: Vec<IndexedCandidate>,
    pub(super) receipts: Vec<ReceiptProjectionCandidate>,
    pub(super) next_cursor: SearchProjectionCursor,
    pub(super) parse_errors: usize,
}

pub(super) async fn project_rollout_into_generation(
    workflow: &WorkflowStore,
    rollout_path: &Path,
    rollout_id: ThreadId,
    generation_id: i64,
    cursor: SearchProjectionCursor,
    metadata: SearchMetadata,
) -> ThreadStoreResult<SearchProjectionProgress> {
    let scan = scan_rollout(rollout_path, cursor).await?;
    let mut indexed_documents = 0usize;
    for candidate in &scan.candidates {
        let ordinal =
            i64::try_from(candidate.ordinal).map_err(|error| ThreadStoreError::Internal {
                message: format!("candidate ordinal exceeds SQLite range: {error}"),
            })?;
        workflow
            .insert_search_document(&SearchDocumentCreate {
                generation_id,
                thread_id: scan.thread_id.to_string(),
                source_id: source_id(rollout_id, candidate),
                source_kind: candidate.source_kind,
                ordinal,
                content: candidate.content.clone(),
                metadata: document_metadata(&metadata, candidate),
            })
            .await
            .map_err(|error| ThreadStoreError::Internal {
                message: format!("failed to insert search document: {error}"),
            })?;
        indexed_documents += 1;
    }
    if let Err(error) = receipt_projection::project_receipts(workflow, &scan.receipts).await {
        let message = error.message();
        mark_source_dirty(workflow, rollout_id, rollout_path, cursor, &message).await;
        return Err(ThreadStoreError::Internal { message });
    }
    if scan.parse_errors > 0 {
        mark_source_dirty(
            workflow,
            rollout_id,
            rollout_path,
            cursor,
            &format!("{} rollout lines could not be decoded", scan.parse_errors),
        )
        .await;
    }
    Ok(SearchProjectionProgress {
        next_cursor: scan.next_cursor,
        indexed_documents,
        parse_errors: scan.parse_errors,
    })
}

pub(super) async fn scan_rollout(
    path: &Path,
    cursor: SearchProjectionCursor,
) -> ThreadStoreResult<RolloutScan> {
    let meta = codex_rollout::read_session_meta_line(path)
        .await
        .map_err(|error| ThreadStoreError::Internal {
            message: format!("failed to read rollout session metadata: {error}"),
        })?
        .meta;
    let visible_from = meta
        .history_base
        .map_or(0, |base| base.end_ordinal_exclusive)
        .max(meta.subagent_history_start_ordinal.unwrap_or(0));
    let mut reader = codex_rollout::open_rollout_line_reader(path)
        .await
        .map_err(|error| ThreadStoreError::Internal {
            message: format!("failed to open rollout search reader: {error}"),
        })?;
    let mut next_offset = 0u64;
    let mut next_ordinal = meta
        .history_base
        .map_or(0, |base| base.end_ordinal_exclusive);
    let mut records = Vec::new();
    let mut receipt_candidates = Vec::new();
    let mut receipts = Vec::new();
    let mut parse_errors = 0usize;

    while let Some(line) = reader
        .next_line()
        .await
        .map_err(|error| ThreadStoreError::Internal {
            message: format!("failed to read rollout search line: {error}"),
        })?
    {
        let line_len = u64::try_from(line.len()).map_err(|_| ThreadStoreError::Internal {
            message: "rollout search line exceeds addressable memory".to_string(),
        })?;
        next_offset = next_offset
            .checked_add(line_len.saturating_add(1))
            .ok_or_else(|| ThreadStoreError::Internal {
                message: "rollout search byte offset overflow".to_string(),
            })?;
        if line.trim().is_empty() {
            continue;
        }
        let value = match serde_json::from_str::<Value>(&line) {
            Ok(value) => value,
            Err(_) => {
                parse_errors += 1;
                continue;
            }
        };
        let line_ordinal = value
            .get("ordinal")
            .and_then(Value::as_u64)
            .unwrap_or(next_ordinal);
        next_ordinal = line_ordinal
            .checked_add(1)
            .ok_or_else(|| ThreadStoreError::Internal {
                message: "rollout search ordinal overflow".to_string(),
            })?;
        let event_time_ms = value
            .get("timestamp")
            .and_then(Value::as_str)
            .and_then(parse_timestamp_ms);
        let in_cursor = next_offset <= cursor.byte_offset || line_ordinal < cursor.ordinal;
        if in_cursor {
            continue;
        }
        let receipt_candidate = (line_ordinal >= visible_from)
            .then(|| extract_receipt_candidate(&value, line_ordinal, event_time_ms));
        let line = match codex_rollout::decode_rollout_line(value) {
            Ok(line) => line,
            Err(_) => {
                parse_errors += 1;
                continue;
            }
        };
        if let Some(receipt) =
            receipt_projection::candidate_from_rollout_item(&line.item, meta.id, event_time_ms)
        {
            receipts.push(receipt);
        }
        if line_ordinal < visible_from {
            continue;
        }
        if let Some(Some(candidate)) = receipt_candidate {
            receipt_candidates.push(candidate);
        }
        records.push(ExtractRecord {
            ordinal: line_ordinal,
            event_time_ms,
            item: line.item,
        });
    }

    let mut candidates = extract_candidates(records);
    candidates.extend(receipt_candidates);
    Ok(RolloutScan {
        thread_id: meta.id,
        candidates: deduplicate_candidates(candidates),
        receipts,
        next_cursor: SearchProjectionCursor {
            byte_offset: next_offset,
            ordinal: next_ordinal,
        },
        parse_errors,
    })
}

pub(crate) async fn search_metadata(
    store: &LocalThreadStore,
    thread_id: ThreadId,
    rollout_path: &Path,
) -> ThreadStoreResult<SearchMetadata> {
    let meta = codex_rollout::read_session_meta_line(rollout_path)
        .await
        .map_err(|error| ThreadStoreError::Internal {
            message: format!("failed to read rollout search metadata: {error}"),
        })?
        .meta;
    let mut metadata = SearchMetadata {
        root_thread_id: meta.history_base.map(|base| base.thread_id.to_string()),
        project_id: None,
        cwd: Some(meta.cwd.to_string_lossy().to_string()),
        provider: meta.model_provider.clone(),
        thread_class: Some(thread_class_from_session(&meta)),
        outcome: None,
        archived: rollout_path_is_archived(store.config.codex_home.as_path(), rollout_path),
        event_time_ms: parse_timestamp_ms(&meta.timestamp),
    };
    let Some(state_db) = store.state_db().await else {
        return Ok(metadata);
    };
    if let Some(thread) =
        state_db
            .get_thread(thread_id)
            .await
            .map_err(|error| ThreadStoreError::Internal {
                message: format!("failed to load thread metadata for search projection: {error}"),
            })?
    {
        metadata.project_id = thread.project_id;
        metadata.cwd = Some(thread.cwd.to_string_lossy().to_string());
        metadata.provider = Some(thread.model_provider);
        metadata.archived = thread.archived_at.is_some();
    }
    if let Ok(runs) = state_db
        .workflow()
        .get_runs_by_thread_id(&thread_id.to_string())
        .await
        && let Some(run) = runs.into_iter().max_by_key(|run| run.updated_at_ms)
    {
        metadata.root_thread_id = run.root_thread_id.or(metadata.root_thread_id);
        metadata.thread_class = Some(run.thread_class);
        metadata.provider = run.provider.or(metadata.provider);
        metadata.cwd = run.cwd.or(metadata.cwd);
        metadata.outcome = run.outcome;
    }
    Ok(metadata)
}

pub(super) async fn clear_live_rollout(
    workflow: &WorkflowStore,
    thread_id: ThreadId,
) -> Result<(), sqlx::Error> {
    let mut transaction = workflow.pool().begin().await?;
    let deleted = sqlx::query("DELETE FROM workflow_search_live_documents WHERE thread_id = ?")
        .bind(thread_id.to_string())
        .execute(&mut *transaction)
        .await?
        .rows_affected();
    if deleted > 0 {
        sqlx::query(
            "UPDATE workflow_search_live_state
             SET live_epoch = live_epoch + 1, updated_at_ms = ? WHERE id = 1",
        )
        .bind(chrono::Utc::now().timestamp_millis())
        .execute(&mut *transaction)
        .await?;
    }
    transaction.commit().await
}

pub(super) fn document_metadata(
    metadata: &SearchMetadata,
    candidate: &IndexedCandidate,
) -> SearchMetadata {
    let mut metadata = metadata.clone();
    metadata.event_time_ms = candidate.event_time_ms.or(metadata.event_time_ms);
    metadata
}

fn thread_class_from_session(meta: &SessionMeta) -> WorkflowThreadClass {
    if meta.source.is_internal() {
        WorkflowThreadClass::Internal
    } else if meta.source.is_non_root_agent() || meta.parent_thread_id.is_some() {
        WorkflowThreadClass::SubAgent
    } else if matches!(meta.source, SessionSource::Exec) {
        WorkflowThreadClass::LegacyExec
    } else {
        WorkflowThreadClass::Interactive
    }
}

pub(super) fn source_id(rollout_id: ThreadId, candidate: &IndexedCandidate) -> String {
    let prefix = format!("{}:{}:", rollout_id, candidate.source_kind.as_str());
    let key_budget = MAX_SOURCE_ID_BYTES.saturating_sub(prefix.len());
    let key = candidate.source_key.replace('\0', " ");
    format!(
        "{}{key}",
        prefix,
        key = truncate_utf8(key.as_str(), key_budget)
    )
}

fn truncate_utf8(value: &str, max_bytes: usize) -> &str {
    if value.len() <= max_bytes {
        return value;
    }
    let mut end = max_bytes;
    while end > 0 && !value.is_char_boundary(end) {
        end -= 1;
    }
    &value[..end]
}

pub(super) fn parse_timestamp_ms(value: &str) -> Option<i64> {
    DateTime::parse_from_rfc3339(value)
        .ok()
        .map(|timestamp| timestamp.timestamp_millis())
}

pub(super) async fn mark_source_dirty(
    workflow: &WorkflowStore,
    rollout_id: ThreadId,
    rollout_path: &Path,
    cursor: SearchProjectionCursor,
    error: &str,
) {
    let source_path = truncate_utf8(
        rollout_path.to_string_lossy().as_ref(),
        MAX_SOURCE_PATH_BYTES,
    )
    .to_string();
    let error_json = serde_json::json!({
        "message": truncate_utf8(error, MAX_ERROR_BYTES),
        "byteOffset": cursor.byte_offset,
        "ordinal": cursor.ordinal,
    })
    .to_string();
    let (source_size_bytes, source_mtime_ms) = source_stat(rollout_path).await;
    let now_ms = chrono::Utc::now().timestamp_millis();
    let result = sqlx::query(
        "INSERT INTO workflow_backfill_journal
            (rollout_id, source_path, byte_offset, rollout_ordinal, status,
             error_json, updated_at_ms, generation_id, source_size_bytes, source_mtime_ms)
         VALUES (?, ?, ?, ?, ?, ?, ?, NULL, ?, ?)
         ON CONFLICT(rollout_id) DO UPDATE SET
            source_path = excluded.source_path,
            byte_offset = excluded.byte_offset,
            rollout_ordinal = excluded.rollout_ordinal,
            status = excluded.status,
            error_json = excluded.error_json,
            updated_at_ms = excluded.updated_at_ms,
            source_size_bytes = excluded.source_size_bytes,
            source_mtime_ms = excluded.source_mtime_ms
         WHERE workflow_backfill_journal.status
            NOT IN ('processing', 'complete', 'skippedPermanent')",
    )
    .bind(rollout_id.to_string())
    .bind(source_path)
    .bind(i64::try_from(cursor.byte_offset).unwrap_or(i64::MAX))
    .bind(i64::try_from(cursor.ordinal).unwrap_or(i64::MAX))
    .bind(WorkflowBackfillJournalStatus::Recoverable.as_str())
    .bind(error_json)
    .bind(now_ms)
    .bind(source_size_bytes)
    .bind(source_mtime_ms)
    .execute(workflow.pool())
    .await;
    if let Err(mark_error) = result {
        warn!(
            rollout_id = %rollout_id,
            rollout_path = %rollout_path.display(),
            error = %mark_error,
            "failed to mark search source dirty"
        );
    }
}

async fn source_stat(path: &Path) -> (Option<i64>, Option<i64>) {
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
