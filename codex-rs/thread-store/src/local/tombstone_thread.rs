//! Logical deletion of local threads.
//!
//! Tombstoning keeps every canonical rollout representation and only removes
//! the thread from state-backed visibility projections. Reference checks and
//! writer locks are shared with the hard-delete path so an external fork is
//! never orphaned while it is still addressable.

use super::LocalThreadStore;
use super::delete_thread::ThreadRollouts;
use super::delete_thread::ensure_no_external_references;
use super::delete_thread::scan_reference_index;
use crate::ThreadStoreError;
use crate::ThreadStoreResult;
use crate::TombstoneThreadsParams;
use codex_protocol::ThreadId;

pub(super) async fn tombstone_threads(
    store: &LocalThreadStore,
    params: TombstoneThreadsParams,
) -> ThreadStoreResult<()> {
    if params.thread_ids.is_empty() {
        return Ok(());
    }
    let Some(state_db) = store.state_db().await else {
        return Err(ThreadStoreError::Unsupported {
            operation: "thread/tombstone",
        });
    };

    let mut thread_ids = params.thread_ids;
    thread_ids.sort_unstable_by_key(ToString::to_string);
    thread_ids.dedup();
    let mut pending_ids = Vec::with_capacity(thread_ids.len());
    for thread_id in thread_ids {
        if !state_db
            .is_thread_tombstoned(thread_id)
            .await
            .map_err(|err| ThreadStoreError::Internal {
                message: format!("failed to read tombstone state for {thread_id}: {err}"),
            })?
        {
            pending_ids.push(thread_id);
        }
    }
    if pending_ids.is_empty() {
        return Ok(());
    }

    let mut lifecycle_guards = Vec::with_capacity(pending_ids.len());
    for thread_id in &pending_ids {
        lifecycle_guards.push(store.live_writer_locks.lock_lifecycle(*thread_id).await);
    }
    let mut live_writer_guards = Vec::with_capacity(pending_ids.len());
    for thread_id in &pending_ids {
        live_writer_guards.push(store.live_writer_locks.lock(*thread_id).await);
    }
    let _writer_guards = store.acquire_writer_locks(&pending_ids).await?;

    let reference_index = scan_reference_index(store).await?;
    let thread_rollouts = pending_ids
        .iter()
        .map(|thread_id| ThreadRollouts::from_index(&reference_index, *thread_id))
        .collect::<Vec<_>>();
    ensure_no_external_references(&reference_index, thread_rollouts.as_slice())?;

    let mut state_ids = Vec::with_capacity(pending_ids.len());
    for (thread_id, rollouts) in pending_ids.iter().zip(&thread_rollouts) {
        if state_db
            .thread_exists(*thread_id)
            .await
            .map_err(|err| ThreadStoreError::Internal {
                message: format!("failed to read thread state for {thread_id}: {err}"),
            })?
        {
            state_ids.push(*thread_id);
        } else if rollouts.has_rollout() {
            return Err(ThreadStoreError::InvalidRequest {
                message: "cannot tombstone a rollout without state metadata".to_string(),
            });
        }
    }
    let changed = state_db
        .tombstone_threads(state_ids.as_slice())
        .await
        .map_err(|err| ThreadStoreError::Internal {
            message: format!("failed to tombstone threads: {err}"),
        })?;
    if changed != state_ids.len() as u64 {
        return Err(ThreadStoreError::InvalidRequest {
            message: "cannot tombstone a thread without state metadata".to_string(),
        });
    }
    for thread_id in pending_ids {
        codex_rollout::remove_thread_name_entries(store.config.codex_home.as_path(), thread_id)
            .await
            .map_err(|err| ThreadStoreError::Internal {
                message: format!("failed to hide thread name {thread_id}: {err}"),
            })?;
    }
    drop(live_writer_guards);
    drop(lifecycle_guards);
    Ok(())
}

pub(super) async fn tombstone_thread(
    store: &LocalThreadStore,
    thread_id: ThreadId,
) -> ThreadStoreResult<()> {
    tombstone_threads(
        store,
        TombstoneThreadsParams {
            thread_ids: vec![thread_id],
        },
    )
    .await
}

#[cfg(test)]
#[path = "tombstone_thread_tests.rs"]
mod tests;
