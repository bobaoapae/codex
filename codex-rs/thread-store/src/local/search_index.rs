//! Best-effort projection of durable rollouts into the local FTS index.
//!
//! The JSONL rollout is always written first.  Search is a rebuildable view,
//! so an index/database failure is recorded as dirty and never turns a
//! successful rollout append into a failed thread operation.

use codex_protocol::ThreadId;
use codex_state::LiveSearchDocumentCreate;
use codex_state::SearchMetadata;
use codex_state::WorkflowStore;
use std::path::Path;
use std::path::PathBuf;
use tracing::warn;

use super::LocalThreadStore;
use super::receipt_projection;
use super::search_index_projection;
use crate::ThreadStoreResult;

/// Logical position in a plain or decompressed rollout stream.
///
/// For `.zst` rollouts the byte offset is in the decompressed JSONL stream,
/// which is the same representation consumed by the existing line reader.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct SearchProjectionCursor {
    pub byte_offset: u64,
    pub ordinal: u64,
}

/// Result of one resumable projection pass.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SearchProjectionProgress {
    pub next_cursor: SearchProjectionCursor,
    pub indexed_documents: usize,
    pub parse_errors: usize,
}

/// In-memory cursor for an active local writer.  It is deliberately only an
/// optimization: a process restart starts from zero and rebuilds the overlay.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct LiveSearchProjectionState {
    pub path: PathBuf,
    pub cursor: SearchProjectionCursor,
}

/// Project the durable suffix of a live rollout into the bounded overlay.
///
/// This function is intentionally best effort.  It logs/indexes valid
/// records, marks the source dirty on a parse or database failure, and returns
/// `Ok(())` so the canonical rollout operation remains successful.
pub(super) async fn project_live_rollout(
    store: &LocalThreadStore,
    thread_id: ThreadId,
    rollout_id: ThreadId,
    rollout_path: &Path,
) {
    let Some(state_db) = store.state_db().await else {
        return;
    };
    let workflow = state_db.workflow().clone();
    let fts_available = match workflow.fts5_available().await {
        Ok(available) => available,
        Err(error) => {
            search_index_projection::mark_source_dirty(
                &workflow,
                rollout_id,
                rollout_path,
                SearchProjectionCursor::default(),
                &error.to_string(),
            )
            .await;
            warn!(
                %thread_id,
                rollout_path = %rollout_path.display(),
                error = %error,
                "failed to check search index availability"
            );
            return;
        }
    };
    let previous_state = {
        let cursors = store.search_index_cursors.lock().await;
        cursors.get(&thread_id).cloned()
    };
    let cursor = previous_state
        .as_ref()
        .filter(|state| state.path == rollout_path)
        .map_or_else(SearchProjectionCursor::default, |state| state.cursor);
    if fts_available
        && previous_state
            .as_ref()
            .is_none_or(|state| state.path != rollout_path)
        && let Err(error) = search_index_projection::clear_live_rollout(&workflow, thread_id).await
    {
        search_index_projection::mark_source_dirty(
            &workflow,
            rollout_id,
            rollout_path,
            cursor,
            &error.to_string(),
        )
        .await;
        warn!(
            %thread_id,
            rollout_path = %rollout_path.display(),
            error = %error,
            "failed to clear stale live search overlay"
        );
        return;
    }
    let scan = match search_index_projection::scan_rollout(rollout_path, cursor).await {
        Ok(scan) => scan,
        Err(error) => {
            search_index_projection::mark_source_dirty(
                &workflow,
                rollout_id,
                rollout_path,
                cursor,
                &error.to_string(),
            )
            .await;
            warn!(
                %thread_id,
                rollout_path = %rollout_path.display(),
                error = %error,
                "failed to scan live rollout for search projection"
            );
            return;
        }
    };

    if let Err(error) = receipt_projection::project_receipts(&workflow, &scan.receipts).await {
        let message = error.message();
        search_index_projection::mark_source_dirty(
            &workflow,
            rollout_id,
            rollout_path,
            cursor,
            &message,
        )
        .await;
        warn!(
            %thread_id,
            rollout_path = %rollout_path.display(),
            error = message,
            "failed to project live rollout receipts"
        );
        return;
    }

    if !fts_available {
        {
            let mut cursors = store.search_index_cursors.lock().await;
            cursors.insert(
                thread_id,
                LiveSearchProjectionState {
                    path: rollout_path.to_path_buf(),
                    cursor: scan.next_cursor,
                },
            );
        }
        if scan.parse_errors > 0 {
            search_index_projection::mark_source_dirty(
                &workflow,
                rollout_id,
                rollout_path,
                cursor,
                &format!("{} rollout lines could not be decoded", scan.parse_errors),
            )
            .await;
        }
        return;
    }

    let metadata =
        match search_index_projection::search_metadata(store, thread_id, rollout_path).await {
            Ok(metadata) => metadata,
            Err(error) => {
                search_index_projection::mark_source_dirty(
                    &workflow,
                    rollout_id,
                    rollout_path,
                    cursor,
                    &error.to_string(),
                )
                .await;
                warn!(
                    %thread_id,
                    rollout_path = %rollout_path.display(),
                    error = %error,
                    "failed to load search projection metadata"
                );
                return;
            }
        };

    let mut indexed_documents = 0usize;
    for candidate in &scan.candidates {
        let input = LiveSearchDocumentCreate {
            thread_id: thread_id.to_string(),
            source_id: search_index_projection::source_id(rollout_id, candidate),
            source_kind: candidate.source_kind,
            ordinal: match i64::try_from(candidate.ordinal) {
                Ok(ordinal) => ordinal,
                Err(error) => {
                    search_index_projection::mark_source_dirty(
                        &workflow,
                        rollout_id,
                        rollout_path,
                        cursor,
                        &format!("candidate ordinal exceeds SQLite range: {error}"),
                    )
                    .await;
                    return;
                }
            },
            content: candidate.content.clone(),
            metadata: search_index_projection::document_metadata(&metadata, candidate),
        };
        if let Err(error) = workflow.upsert_live_search_document(&input).await {
            search_index_projection::mark_source_dirty(
                &workflow,
                rollout_id,
                rollout_path,
                cursor,
                &error.to_string(),
            )
            .await;
            warn!(
                %thread_id,
                rollout_path = %rollout_path.display(),
                source_kind = %candidate.source_kind,
                error = %error,
                "failed to update live search overlay"
            );
            return;
        }
        indexed_documents += 1;
    }

    {
        let mut cursors = store.search_index_cursors.lock().await;
        cursors.insert(
            thread_id,
            LiveSearchProjectionState {
                path: rollout_path.to_path_buf(),
                cursor: scan.next_cursor,
            },
        );
    }
    if scan.parse_errors > 0 {
        search_index_projection::mark_source_dirty(
            &workflow,
            rollout_id,
            rollout_path,
            cursor,
            &format!("{} rollout lines could not be decoded", scan.parse_errors),
        )
        .await;
    }
    tracing::trace!(
        %thread_id,
        rollout_path = %rollout_path.display(),
        indexed_documents,
        parse_errors = scan.parse_errors,
        "projected live rollout search overlay"
    );
}

/// Project one physical rollout into an unpublished immutable generation.
///
/// The helper is resumable by the returned logical byte/ordinal cursor and
/// reads both plain and compressed rollouts through the canonical rollout
/// reader.  Unlike the live overlay path, failures are returned so the
/// backfill coordinator can retain its journal cursor.
pub(crate) async fn project_rollout_into_generation(
    workflow: &WorkflowStore,
    rollout_path: &Path,
    rollout_id: ThreadId,
    generation_id: i64,
    cursor: SearchProjectionCursor,
    metadata: SearchMetadata,
) -> ThreadStoreResult<SearchProjectionProgress> {
    search_index_projection::project_rollout_into_generation(
        workflow,
        rollout_path,
        rollout_id,
        generation_id,
        cursor,
        metadata,
    )
    .await
}

/// Forget the process-local optimization for a closed live writer.
pub(super) async fn forget_live_rollout(store: &LocalThreadStore, thread_id: ThreadId) {
    store.search_index_cursors.lock().await.remove(&thread_id);
}

/// Rebuild the live overlay metadata after a project/provider/archive update.
///
/// Metadata changes do not append a searchable rollout record, so the normal
/// suffix cursor would not observe them.  Resetting this process-local cursor
/// and replaying the canonical file keeps the overlay bounded and makes the
/// update best effort just like a normal append.
pub(super) async fn refresh_live_metadata(store: &LocalThreadStore, thread_id: ThreadId) {
    let Ok((recorder, rollout_id, _history_mode)) =
        super::live_writer::live_writer_parts(store, thread_id).await
    else {
        return;
    };
    let rollout_path = recorder.rollout_path().to_path_buf();
    forget_live_rollout(store, thread_id).await;
    project_live_rollout(store, thread_id, rollout_id, rollout_path.as_path()).await;
}

#[cfg(test)]
#[path = "search_index_tests.rs"]
mod tests;
