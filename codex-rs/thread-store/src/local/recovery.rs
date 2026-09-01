//! Read-only detection and preparation of a sanitized recovery lineage.
//!
//! Recovery is deliberately a two-step operation. The first step scans the selected immutable
//! rollout and validates provider-attested candidates. The second step re-scans the rollout while
//! holding the source lifecycle and writer locks, validates the preview watermark, and returns a
//! fresh child identity plus copied history. No operation in this module rewrites or deletes the
//! source rollout.

use std::collections::BTreeMap;
use std::sync::Arc;

use codex_protocol::ThreadId;
use codex_protocol::models::AgentMessageInputContent;
use codex_protocol::models::ResponseItem;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::SessionMetaLine;
use codex_protocol::protocol::ThreadHistoryMode;
use codex_rollout::RolloutItem;
use tokio::sync::OwnedMutexGuard;
use tokio::sync::OwnedRwLockWriteGuard;

use super::LocalThreadStore;
use super::recovery_scan::RecoveryRecord;
use super::recovery_scan::scan_rollout;
use super::thread_rollout_resolver;
use super::writer_lock::WriterLockGuard;
use crate::ExistingRecovery;
use crate::PreparedRecovery;
use crate::RecoveryBlockReason;
use crate::RecoveryCreateParams;
use crate::RecoveryCreateResult;
use crate::RecoveryExcludedItem;
use crate::RecoveryExclusionReason;
use crate::RecoveryPolicy;
use crate::RecoveryPreview;
use crate::RecoveryPreviewParams;
use crate::RecoveryQuiescenceAttestation;
use crate::RecoveryQuiescenceParams;
use crate::RecoveryToken;
use crate::RecoveryTurnState;
use crate::RecoveryWatermark;
use crate::ThreadStoreError;
use crate::ThreadStoreResult;

/// Creates a read-only preview of a local provider's explicitly attested recovery candidates.
pub(super) async fn preview(
    store: &LocalThreadStore,
    params: RecoveryPreviewParams,
) -> ThreadStoreResult<RecoveryPreview> {
    let _lifecycle_guard = store
        .live_writer_locks
        .reserve_lifecycle(params.thread_id)
        .await;
    let _writer_guard = store.live_writer_locks.lock(params.thread_id).await;
    validate_limits(&params.policy)?;
    let _cross_process_writer_lock =
        prepare_source_access(store, params.thread_id, params.quiescence.as_ref()).await?;
    let source = scan_source(
        store,
        params.thread_id,
        params.include_archived,
        params.policy.limits,
    )
    .await?;
    validate_quiescence_watermark(&source, params.quiescence.as_ref())?;
    if params.has_live_descendants {
        return Ok(blocked_preview(
            &source,
            RecoveryBlockReason::LiveDescendants,
        ));
    }

    match analyze(&source, &params.policy) {
        Ok(analysis) => {
            let preview = preview_from_analysis(
                &source,
                params.include_archived,
                params.quiescence,
                &params.policy,
                analysis,
            );
            Ok(preview)
        }
        Err(reason) => Ok(blocked_preview(&source, reason)),
    }
}

/// Revalidates a preview token and prepares a fresh child identity with sanitized copied history.
pub(super) async fn create(
    store: &LocalThreadStore,
    params: RecoveryCreateParams,
) -> ThreadStoreResult<RecoveryCreateResult> {
    let source_thread_id = params.token.source_thread_id;
    let lifecycle_guard = store
        .live_writer_locks
        .lock_lifecycle(source_thread_id)
        .await;
    let writer_guard = store.live_writer_locks.lock(source_thread_id).await;

    if params.has_live_descendants {
        return Err(ThreadStoreError::Conflict {
            message: format!(
                "cannot recover thread {source_thread_id} while live descendants exist"
            ),
        });
    }

    validate_limits(&params.token.policy)?;
    if params.quiescence != params.token.quiescence {
        return Err(ThreadStoreError::Conflict {
            message: format!(
                "recovery quiescence attestation for thread {source_thread_id} changed after preview"
            ),
        });
    }
    let cross_process_writer_lock =
        prepare_source_access(store, source_thread_id, params.quiescence.as_ref()).await?;
    let source = scan_source(
        store,
        source_thread_id,
        params.token.include_archived,
        params.token.policy.limits,
    )
    .await?;
    validate_quiescence_watermark(&source, params.quiescence.as_ref())?;
    if source.watermark != params.token.watermark {
        return Err(ThreadStoreError::Conflict {
            message: format!("recovery source for thread {source_thread_id} changed after preview"),
        });
    }
    if source.rollout_id != params.token.watermark.rollout_id {
        return Err(ThreadStoreError::Conflict {
            message: format!(
                "recovery source rollout for thread {source_thread_id} changed after preview"
            ),
        });
    }

    let analysis =
        analyze(&source, &params.token.policy).map_err(|reason| ThreadStoreError::Conflict {
            message: format!(
                "recovery token for thread {source_thread_id} is no longer applicable: {reason:?}"
            ),
        })?;
    if params.token.token_id != params.token.recovered_thread_id {
        return Err(ThreadStoreError::Conflict {
            message: format!(
                "recovery token for thread {source_thread_id} has mismatched child identity"
            ),
        });
    }
    let recovered_thread_id = params.token.recovered_thread_id;
    if let Some(existing) =
        existing_recovery_child(store, source_thread_id, recovered_thread_id).await?
    {
        return Ok(RecoveryCreateResult::Existing(existing));
    }
    let reservation = RecoveryReservation {
        _lifecycle_guard: lifecycle_guard,
        _writer_guard: writer_guard,
        _cross_process_writer_lock: cross_process_writer_lock,
    };
    Ok(RecoveryCreateResult::Prepared(PreparedRecovery::new(
        source_thread_id,
        recovered_thread_id,
        source.rollout_id,
        source.watermark,
        Arc::new(analysis.retained_items),
        analysis.excluded_items,
        reservation,
    )))
}

/// The locks held by a prepared recovery keep the source stable until the caller starts or drops
/// the child. A manual `Debug` implementation avoids requiring Tokio's guard debug bounds to be
/// part of the public `PreparedRecovery` contract.
struct RecoveryReservation {
    _lifecycle_guard: OwnedRwLockWriteGuard<()>,
    _writer_guard: OwnedMutexGuard<()>,
    _cross_process_writer_lock: Option<WriterLockGuard>,
}

impl std::fmt::Debug for RecoveryReservation {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("RecoveryReservation")
            .finish_non_exhaustive()
    }
}

struct ScannedRollout {
    source_thread_id: ThreadId,
    rollout_id: ThreadId,
    meta: SessionMetaLine,
    records: Vec<RecoveryRecord>,
    item_count: usize,
    buffer_limit_exceeded: bool,
    watermark: RecoveryWatermark,
}

struct RecoveryAnalysis {
    excluded_items: Vec<RecoveryExcludedItem>,
    retained_items: Vec<RolloutItem>,
    retained_serialized_bytes: u64,
}

async fn scan_source(
    store: &LocalThreadStore,
    thread_id: ThreadId,
    include_archived: bool,
    limits: crate::RecoveryLimits,
) -> ThreadStoreResult<ScannedRollout> {
    let resolved = if include_archived {
        thread_rollout_resolver::resolve_current_including_archived(store, thread_id).await?
    } else {
        thread_rollout_resolver::resolve_current(store, thread_id).await?
    }
    .ok_or(ThreadStoreError::ThreadNotFound { thread_id })?;

    let rollout_id = resolved.rollout_id;
    let path = resolved.path;
    let scan = tokio::task::spawn_blocking(move || scan_rollout(path.as_path(), thread_id, limits))
        .await
        .map_err(|err| ThreadStoreError::Internal {
            message: format!("failed to join recovery rollout scan for {thread_id}: {err}"),
        })?
        .map_err(|err| ThreadStoreError::Internal {
            message: format!("failed to scan recovery rollout for {thread_id}: {err}"),
        })?;

    let meta = scan.meta.ok_or_else(|| ThreadStoreError::Internal {
        message: format!("recovery rollout for thread {thread_id} has no metadata"),
    })?;
    if meta.meta.id != thread_id {
        return Err(ThreadStoreError::InvalidRequest {
            message: format!("recovery rollout for thread {thread_id} belongs to another thread"),
        });
    }
    if meta.meta.history_mode != ThreadHistoryMode::Paginated {
        return Err(ThreadStoreError::InvalidRequest {
            message: format!("thread {thread_id} does not use paginated history"),
        });
    }

    Ok(ScannedRollout {
        source_thread_id: thread_id,
        rollout_id,
        meta,
        records: scan.records,
        item_count: scan.item_count,
        buffer_limit_exceeded: scan.buffer_limit_exceeded,
        watermark: RecoveryWatermark {
            rollout_id,
            end_ordinal_exclusive: scan.next_ordinal,
            end_byte_offset: scan.end_byte_offset,
        },
    })
}

/// Returns a source writer reservation and, for a loaded thread, verifies the caller's quiescence
/// attestation while flushing the local recorder. A closed source is guarded by the existing
/// cross-process writer lock; an active writer from another process therefore fails closed.
async fn prepare_source_access(
    store: &LocalThreadStore,
    thread_id: ThreadId,
    quiescence: Option<&RecoveryQuiescenceAttestation>,
) -> ThreadStoreResult<Option<WriterLockGuard>> {
    let Some(attestation) = quiescence else {
        if store.live_recorders.lock().await.contains_key(&thread_id) {
            return Err(ThreadStoreError::Conflict {
                message: format!(
                    "thread {thread_id} has an active local writer; provide an idle quiescence attestation"
                ),
            });
        }
        return store.writer_lock_coordinator.acquire(thread_id).map(Some);
    };

    if attestation.thread_id != thread_id || attestation.turn_state != RecoveryTurnState::Idle {
        return Err(ThreadStoreError::InvalidRequest {
            message: format!(
                "recovery quiescence attestation for thread {thread_id} is not an idle local proof"
            ),
        });
    }
    let Some((recorder, rollout_id)) = store
        .live_recorders
        .lock()
        .await
        .get(&thread_id)
        .map(|entry| (entry.recorder.clone(), entry.rollout_id))
    else {
        return Err(ThreadStoreError::Conflict {
            message: format!(
                "recovery quiescence attestation for thread {thread_id} has no local writer"
            ),
        });
    };
    if rollout_id != attestation.rollout_id {
        return Err(ThreadStoreError::Conflict {
            message: format!(
                "recovery quiescence attestation for thread {thread_id} names another rollout"
            ),
        });
    }
    recorder
        .flush()
        .await
        .map_err(|error| ThreadStoreError::Internal {
            message: format!("failed to flush local recovery writer for {thread_id}: {error}"),
        })?;
    Ok(None)
}

fn validate_quiescence_watermark(
    source: &ScannedRollout,
    quiescence: Option<&RecoveryQuiescenceAttestation>,
) -> ThreadStoreResult<()> {
    let Some(attestation) = quiescence else {
        return Ok(());
    };
    if attestation.thread_id != source.source_thread_id
        || attestation.rollout_id != source.rollout_id
        || attestation.watermark != source.watermark
        || attestation.turn_state != RecoveryTurnState::Idle
    {
        return Err(ThreadStoreError::Conflict {
            message: format!(
                "recovery quiescence attestation for thread {} is stale",
                source.source_thread_id
            ),
        });
    }
    Ok(())
}

async fn existing_recovery_child(
    store: &LocalThreadStore,
    source_thread_id: ThreadId,
    recovered_thread_id: ThreadId,
) -> ThreadStoreResult<Option<ExistingRecovery>> {
    let Some(resolved) =
        thread_rollout_resolver::resolve_current_including_archived(store, recovered_thread_id)
            .await?
    else {
        return Ok(None);
    };
    let metadata = codex_rollout::read_session_meta_line(resolved.path.as_path())
        .await
        .map_err(|error| ThreadStoreError::Internal {
            message: format!(
                "failed to inspect existing recovery child {recovered_thread_id}: {error}"
            ),
        })?
        .meta;
    let is_recovery_child = metadata.id == recovered_thread_id
        && metadata.forked_from_id == Some(source_thread_id)
        && metadata
            .thread_source
            .as_ref()
            .is_some_and(|source| source.as_str() == "recovery");
    if !is_recovery_child {
        return Err(ThreadStoreError::Conflict {
            message: format!(
                "deterministic recovery child id {recovered_thread_id} is already used by another thread"
            ),
        });
    }
    Ok(Some(ExistingRecovery {
        source_thread_id,
        recovered_thread_id,
    }))
}

/// Flushes a locally loaded writer and returns a bounded attestation for an idle turn boundary.
pub(super) async fn attest_quiescence(
    store: &LocalThreadStore,
    params: RecoveryQuiescenceParams,
) -> ThreadStoreResult<RecoveryQuiescenceAttestation> {
    let _lifecycle_guard = store
        .live_writer_locks
        .reserve_lifecycle(params.thread_id)
        .await;
    let _writer_guard = store.live_writer_locks.lock(params.thread_id).await;
    if params.turn_state != RecoveryTurnState::Idle {
        return Err(ThreadStoreError::InvalidRequest {
            message: format!(
                "cannot attest recovery quiescence for thread {} while its turn is not idle",
                params.thread_id
            ),
        });
    }
    let Some((recorder, rollout_id)) = store
        .live_recorders
        .lock()
        .await
        .get(&params.thread_id)
        .map(|entry| (entry.recorder.clone(), entry.rollout_id))
    else {
        return Err(ThreadStoreError::Conflict {
            message: format!(
                "thread {} has no locally controlled writer to attest",
                params.thread_id
            ),
        });
    };
    recorder
        .flush()
        .await
        .map_err(|error| ThreadStoreError::Internal {
            message: format!(
                "failed to flush local recovery writer for {}: {error}",
                params.thread_id
            ),
        })?;
    let source = scan_source(
        store,
        params.thread_id,
        /*include_archived*/ true,
        crate::RecoveryLimits::default(),
    )
    .await?;
    if source.buffer_limit_exceeded || source.rollout_id != rollout_id {
        return Err(ThreadStoreError::InvalidRequest {
            message: format!(
                "thread {} cannot produce a bounded recovery quiescence proof",
                params.thread_id
            ),
        });
    }
    Ok(RecoveryQuiescenceAttestation {
        thread_id: params.thread_id,
        rollout_id,
        watermark: source.watermark,
        turn_state: RecoveryTurnState::Idle,
    })
}

fn validate_limits(policy: &RecoveryPolicy) -> ThreadStoreResult<()> {
    if policy.limits.max_items == 0
        || policy.limits.max_items > 100_000
        || policy.limits.max_serialized_bytes == 0
        || policy.limits.max_serialized_bytes > super::recovery_scan::MAX_BUFFER_BYTES
        || policy.encrypted_agent_messages.len() > 1_024
        || policy.contaminated_turn_completions.len() > 1_024
        || policy.retry_turns.len() > 1_024
        || policy.encrypted_agent_messages.iter().any(|candidate| {
            candidate.provider_id.len() > 128
                || candidate
                    .item_id
                    .as_ref()
                    .is_some_and(|item_id| item_id.len() > 256)
        })
        || policy
            .retry_turns
            .iter()
            .any(|retry| retry.turn_id.len() > 256 || retry.error_message.len() > 4_096)
        || policy
            .contaminated_turn_completions
            .iter()
            .any(|candidate| candidate.turn_id.len() > 256 || candidate.error_message.len() > 4_096)
    {
        return Err(ThreadStoreError::InvalidRequest {
            message: "recovery policy exceeds its bounded limits".to_string(),
        });
    }
    Ok(())
}

fn analyze(
    source: &ScannedRollout,
    policy: &RecoveryPolicy,
) -> Result<RecoveryAnalysis, RecoveryBlockReason> {
    if source.buffer_limit_exceeded {
        return Err(RecoveryBlockReason::ContextTooLarge);
    }
    if policy.encrypted_agent_messages.is_empty() || policy.contaminated_turn_completions.is_empty()
    {
        return Err(RecoveryBlockReason::AmbiguousCandidates);
    }
    if source.item_count == 0 {
        return Err(RecoveryBlockReason::AmbiguousCandidates);
    }

    let mut excluded: BTreeMap<usize, RecoveryExclusionReason> = BTreeMap::new();
    let mut candidate_ordinals = BTreeMap::new();
    for candidate in &policy.encrypted_agent_messages {
        if candidate.provider_id.trim().is_empty()
            || candidate_ordinals
                .insert(candidate.rollout_ordinal, ())
                .is_some()
        {
            return Err(RecoveryBlockReason::AmbiguousCandidates);
        }
        let Some((index, record)) = source
            .records
            .iter()
            .enumerate()
            .find(|(_, record)| record.ordinal == candidate.rollout_ordinal)
        else {
            return Err(RecoveryBlockReason::AmbiguousCandidates);
        };
        let Some(item_id) = encrypted_agent_message_id(&record.item) else {
            return Err(RecoveryBlockReason::AmbiguousCandidates);
        };
        if candidate
            .item_id
            .as_deref()
            .is_some_and(|expected| item_id.as_deref() != Some(expected))
        {
            return Err(RecoveryBlockReason::AmbiguousCandidates);
        }
        if excluded
            .insert(
                index,
                RecoveryExclusionReason::InvalidEncryptedAgentMessage {
                    provider_id: candidate.provider_id.clone(),
                },
            )
            .is_some()
        {
            return Err(RecoveryBlockReason::AmbiguousCandidates);
        }
    }

    let maximum_encrypted_ordinal = candidate_ordinals.keys().copied().max().unwrap_or(0);
    for candidate in &policy.contaminated_turn_completions {
        if candidate.turn_id.trim().is_empty()
            || candidate.error_message.is_empty()
            || candidate.rollout_ordinal <= maximum_encrypted_ordinal
            || candidate_ordinals
                .insert(candidate.rollout_ordinal, ())
                .is_some()
        {
            return Err(RecoveryBlockReason::AmbiguousCandidates);
        }
        let Some((index, record)) = source
            .records
            .iter()
            .enumerate()
            .find(|(_, record)| record.ordinal == candidate.rollout_ordinal)
        else {
            return Err(RecoveryBlockReason::AmbiguousCandidates);
        };
        if !turn_complete_matches(
            record,
            candidate.turn_id.as_str(),
            candidate.error_message.as_str(),
        ) {
            return Err(RecoveryBlockReason::AmbiguousCandidates);
        }
        if excluded
            .insert(
                index,
                RecoveryExclusionReason::ContaminatedTurnComplete {
                    turn_id: candidate.turn_id.clone(),
                },
            )
            .is_some()
        {
            return Err(RecoveryBlockReason::AmbiguousCandidates);
        }
    }

    let maximum_invalid_ordinal = candidate_ordinals.keys().copied().max().unwrap_or(0);
    let mut retry_turn_ids = BTreeMap::new();
    for retry in &policy.retry_turns {
        if retry.turn_id.trim().is_empty()
            || retry.error_message.is_empty()
            || retry_turn_ids.insert(retry.turn_id.as_str(), ()).is_some()
        {
            return Err(RecoveryBlockReason::AmbiguousCandidates);
        }
        let completions: Vec<(usize, &RecoveryRecord)> = source
            .records
            .iter()
            .enumerate()
            .filter(|(_, record)| {
                turn_complete_matches(record, retry.turn_id.as_str(), retry.error_message.as_str())
            })
            .collect();
        let [(completion_index, completion)] = completions.as_slice() else {
            return Err(RecoveryBlockReason::AmbiguousCandidates);
        };
        if completion.ordinal <= maximum_invalid_ordinal {
            return Err(RecoveryBlockReason::AmbiguousCandidates);
        }

        let starts: Vec<(usize, &RecoveryRecord)> = source
            .records
            .iter()
            .enumerate()
            .filter(|(index, record)| {
                *index < *completion_index && turn_started_matches(record, retry.turn_id.as_str())
            })
            .collect();
        let [(start_index, start)] = starts.as_slice() else {
            return Err(RecoveryBlockReason::AmbiguousCandidates);
        };
        if start.ordinal <= maximum_invalid_ordinal
            || (*start_index..=*completion_index).any(|index| excluded.contains_key(&index))
        {
            return Err(RecoveryBlockReason::AmbiguousCandidates);
        }

        for index in *start_index..=*completion_index {
            if excluded
                .insert(
                    index,
                    RecoveryExclusionReason::RetryTurn {
                        turn_id: retry.turn_id.clone(),
                    },
                )
                .is_some()
            {
                return Err(RecoveryBlockReason::AmbiguousCandidates);
            }
        }
    }

    let mut retained_items = Vec::with_capacity(source.records.len() - excluded.len());
    let mut retained_serialized_bytes = 0_u64;
    let mut excluded_items = Vec::with_capacity(excluded.len());
    for (index, record) in source.records.iter().enumerate() {
        let Some(reason) = excluded.get(&index) else {
            retained_items.push(record.item.clone());
            retained_serialized_bytes = retained_serialized_bytes.saturating_add(
                record
                    .end_byte_offset
                    .saturating_sub(record.start_byte_offset),
            );
            continue;
        };
        excluded_items.push(RecoveryExcludedItem {
            rollout_ordinal: record.ordinal,
            item_id: response_item_id(&record.item),
            reason: reason.clone(),
        });
    }

    if retained_items.len() > policy.limits.max_items
        || retained_serialized_bytes > policy.limits.max_serialized_bytes
    {
        return Err(RecoveryBlockReason::ContextTooLarge);
    }

    Ok(RecoveryAnalysis {
        excluded_items,
        retained_items,
        retained_serialized_bytes,
    })
}

fn preview_from_analysis(
    source: &ScannedRollout,
    include_archived: bool,
    quiescence: Option<RecoveryQuiescenceAttestation>,
    policy: &RecoveryPolicy,
    analysis: RecoveryAnalysis,
) -> RecoveryPreview {
    let recovered_thread_id = ThreadId::new();
    let token = RecoveryToken {
        token_id: recovered_thread_id,
        recovered_thread_id,
        source_thread_id: source.source_thread_id,
        watermark: source.watermark,
        include_archived,
        quiescence,
        policy: policy.clone(),
    };
    RecoveryPreview {
        source_thread_id: source.source_thread_id,
        source_rollout_id: source.rollout_id,
        watermark: source.watermark,
        source_model_provider: source.meta.meta.model_provider.clone(),
        source_item_count: source.item_count,
        source_serialized_bytes: source.watermark.end_byte_offset,
        retained_item_count: analysis.retained_items.len(),
        retained_serialized_bytes: analysis.retained_serialized_bytes,
        excluded_items: analysis.excluded_items,
        can_recover: true,
        blocked_reason: None,
        token: Some(token),
    }
}

fn blocked_preview(source: &ScannedRollout, reason: RecoveryBlockReason) -> RecoveryPreview {
    RecoveryPreview {
        source_thread_id: source.source_thread_id,
        source_rollout_id: source.rollout_id,
        watermark: source.watermark,
        source_model_provider: source.meta.meta.model_provider.clone(),
        source_item_count: source.item_count,
        source_serialized_bytes: source.watermark.end_byte_offset,
        retained_item_count: 0,
        retained_serialized_bytes: 0,
        excluded_items: Vec::new(),
        can_recover: false,
        blocked_reason: Some(reason),
        token: None,
    }
}

fn encrypted_agent_message_id(item: &RolloutItem) -> Option<Option<String>> {
    let RolloutItem::ResponseItem(envelope) = item else {
        return None;
    };
    let ResponseItem::AgentMessage { id, content, .. } = &envelope.item else {
        return None;
    };
    if !content
        .iter()
        .any(|part| matches!(part, AgentMessageInputContent::EncryptedContent { .. }))
    {
        return None;
    }
    Some(id.as_ref().map(ToString::to_string))
}

fn response_item_id(item: &RolloutItem) -> Option<String> {
    let RolloutItem::ResponseItem(envelope) = item else {
        return None;
    };
    envelope.item.id().map(ToString::to_string)
}

fn turn_started_matches(record: &RecoveryRecord, turn_id: &str) -> bool {
    matches!(
        &record.item,
        RolloutItem::EventMsg(EventMsg::TurnStarted(event)) if event.turn_id == turn_id
    )
}

fn turn_complete_matches(record: &RecoveryRecord, turn_id: &str, error_message: &str) -> bool {
    matches!(
        &record.item,
        RolloutItem::EventMsg(EventMsg::TurnComplete(event))
            if event.turn_id == turn_id
                && event
                    .error
                    .as_ref()
                    .is_some_and(|error| error.message == error_message)
    )
}

#[cfg(test)]
#[path = "recovery_tests.rs"]
mod tests;
