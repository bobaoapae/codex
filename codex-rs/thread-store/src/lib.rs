//! Storage-neutral thread persistence interfaces.
//!
//! Application code should treat [`codex_protocol::ThreadId`] as the only durable thread handle.
//! Implementations are responsible for resolving that id to local rollout files, RPC requests, or
//! any other backing store.

mod error;
mod in_memory;
mod live_thread;
mod local;
mod projects;
mod queue_store;
mod receipt_append;
mod store;
mod thread_metadata_sync;
mod thread_sections;
mod types;

pub use codex_state::MAX_QUEUE_ITEMS;
pub use codex_state::ProjectSortKey;
pub use codex_state::QueuedUserSubmissionRecord;
pub use error::ThreadStoreError;
pub use error::ThreadStoreResult;
pub use in_memory::InMemoryThreadStore;
pub use in_memory::InMemoryThreadStoreCalls;
pub use live_thread::LiveThread;
pub use live_thread::LiveThreadInitGuard;
pub use local::FROZEN_PREVIEW_SCHEMA_VERSION;
pub use local::FrozenPreview;
pub use local::LocalThreadStore;
pub use local::LocalThreadStoreConfig;
pub use local::RolloutMigrationApplyReceipt;
pub use local::RolloutMigrationFailureReason;
pub use local::RolloutMigrationMode;
pub use local::RolloutMigrationOptions;
pub use local::RolloutMigrationOutcome;
pub use local::RolloutMigrationPreviewCounts;
pub use local::RolloutMigrationPreviewEntry;
pub use local::RolloutMigrationPreviewOptions;
pub use local::RolloutMigrationPreviewReport;
pub use local::RolloutMigrationPreviewRepresentation;
pub use local::RolloutMigrationPreviewStatus;
pub use local::RolloutMigrationPreviewThreadClass;
pub use local::RolloutMigrationPreviewWatermark;
pub use local::RolloutMigrationProgress;
pub use local::RolloutMigrationReport;
pub use local::RolloutMigrationStatus;
pub use projects::CreateProjectParams;
pub use projects::CreatedProject;
pub use projects::DeletedProject;
pub use projects::ListProjectsParams;
pub use projects::MoveProjectParams;
pub use projects::ProjectMoveOutcome;
pub use projects::StoredProject;
pub use projects::StoredProjectRoot;
pub use projects::StoredProjectsPage;
pub use projects::UpdateProjectParams;
pub use projects::UpdatedProject;
pub use queue_store::LocalQueueStore;
pub use queue_store::QueueStore;
pub use receipt_append::AppendReceiptOutcome;
pub use receipt_append::AppendReceiptParams;
pub use store::PersistContext;
pub use store::ThreadStore;
pub use store::ThreadStoreFuture;
pub use thread_sections::CreateThreadSectionParams;
pub use thread_sections::DeleteThreadSectionParams;
pub use thread_sections::ListThreadSectionsParams;
pub use thread_sections::RenameThreadSectionParams;
pub use thread_sections::StoredThreadSection;
pub use thread_sections::StoredThreadSectionsPage;
pub use types::AppendThreadItemsParams;
pub use types::ArchiveThreadParams;
pub use types::ArchiveThreadsParams;
pub use types::ClearableField;
pub use types::CreateThreadParams;
pub use types::DeleteThreadParams;
pub use types::DeleteThreadsParams;
pub use types::ExistingRecovery;
pub use types::ExtraConfig;
pub use types::ForkBoundary;
pub use types::GitInfoPatch;
pub use types::ItemPage;
pub use types::ItemSortKey;
pub use types::ListItemsParams;
pub use types::ListThreadsParams;
pub use types::ListTimelineParams;
pub use types::ListTurnsParams;
pub use types::LoadThreadHistoryParams;
pub use types::MoveThreadToSectionParams;
pub use types::PrepareForkParams;
pub use types::PreparedFork;
pub use types::PreparedRecovery;
pub use types::ReadThreadByRolloutPathParams;
pub use types::ReadThreadParams;
pub use types::RecoveryBlockReason;
pub use types::RecoveryCreateParams;
pub use types::RecoveryCreateResult;
pub use types::RecoveryEncryptedAgentMessageCandidate;
pub use types::RecoveryExcludedItem;
pub use types::RecoveryExclusionReason;
pub use types::RecoveryLimits;
pub use types::RecoveryPolicy;
pub use types::RecoveryPreview;
pub use types::RecoveryPreviewParams;
pub use types::RecoveryQuiescenceAttestation;
pub use types::RecoveryQuiescenceParams;
pub use types::RecoveryRetryTurnCandidate;
pub use types::RecoveryRolloutRecord;
pub use types::RecoveryRolloutScan;
pub use types::RecoveryToken;
pub use types::RecoveryTurnCompleteCandidate;
pub use types::RecoveryTurnState;
pub use types::RecoveryWatermark;
pub use types::ResumeThreadParams;
pub use types::RevertThreadParams;
pub use types::SearchTextRange;
pub use types::SearchThreadOccurrencesParams;
pub use types::SearchThreadsParams;
pub use types::SortDirection;
pub use types::StoredModelContext;
pub use types::StoredThread;
pub use types::StoredThreadHistory;
pub use types::StoredThreadItem;
pub use types::StoredThreadOccurrence;
pub use types::StoredThreadSearchResult;
pub use types::StoredTurn;
pub use types::StoredTurnError;
pub use types::StoredTurnItemsView;
pub use types::StoredTurnStatus;
pub use types::ThreadMetadataPatch;
pub use types::ThreadOccurrenceSearchPage;
pub use types::ThreadPage;
pub use types::ThreadPersistenceMetadata;
pub use types::ThreadRelationFilter;
pub use types::ThreadSearchPage;
pub use types::ThreadSortKey;
pub use types::TimelinePage;
pub use types::TombstoneThreadParams;
pub use types::TombstoneThreadsParams;
pub use types::TurnPage;
pub use types::UpdateThreadMetadataParams;

/// Scan one rollout into a bounded, offset-preserving recovery view.
///
/// Compressed rollouts are decoded into an anonymous temporary file, so returned byte offsets
/// address the logical JSONL representation rather than compressed bytes. The scanner never
/// modifies the source rollout and stops retaining decoded items once `limits` is exceeded.
pub fn scan_recovery_rollout(
    path: &std::path::Path,
    thread_id: codex_protocol::ThreadId,
    limits: RecoveryLimits,
) -> std::io::Result<RecoveryRolloutScan> {
    let scan = local::recovery_scan::scan_rollout(path, thread_id, limits)?;
    Ok(RecoveryRolloutScan {
        meta: scan.meta,
        records: scan
            .records
            .into_iter()
            .map(|record| RecoveryRolloutRecord {
                ordinal: record.ordinal,
                start_byte_offset: record.start_byte_offset,
                end_byte_offset: record.end_byte_offset,
                item: record.item,
            })
            .collect(),
        item_count: scan.item_count,
        buffer_limit_exceeded: scan.buffer_limit_exceeded,
        next_ordinal: scan.next_ordinal,
        end_byte_offset: scan.end_byte_offset,
    })
}
