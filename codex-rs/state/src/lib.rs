//! SQLite-backed state for rollout metadata.
//!
//! This crate is intentionally small and focused: it extracts rollout metadata
//! from JSONL rollouts and mirrors it into a local SQLite database. Backfill
//! orchestration and rollout scanning live in `codex-core`.

const _: () = assert!(
    libsqlite3_sys::SQLITE_VERSION_NUMBER >= 3_051_003,
    "bundled SQLite must include the WAL-reset corruption fix",
);

mod audit;
mod extract;
pub mod log_db;
mod migrations;
mod model;
mod paths;
mod runtime;
mod sqlite;
mod telemetry;
mod workflow;

pub use model::CreatedProject;
pub use model::LogEntry;
pub use model::LogQuery;
pub use model::LogRow;
pub use model::Phase2JobClaimOutcome;
pub use model::Project;
pub use model::ProjectRoot;
pub use model::ProjectSortKey;
pub use model::ProjectsPage;
pub use model::QueuedUserSubmissionRecord;
pub use model::RolloutMigrationCursor;
pub use model::RolloutMigrationSkippedRollout;
pub use model::RolloutMigrationState;
/// Preferred entrypoint: owns configuration and metrics.
pub use runtime::StateRuntime;
pub use sqlite::SqliteConfig;

pub use audit::ThreadStateAuditRow;
pub use audit::read_thread_state_audit_rows;
/// Low-level storage engine: useful for focused tests.
///
/// Most consumers should prefer [`StateRuntime`].
pub use extract::apply_rollout_item;
pub use extract::rollout_item_affects_thread_metadata;
pub use model::Anchor;
pub use model::BackfillState;
pub use model::BackfillStats;
pub use model::BackfillStatus;
pub use model::DEFAULT_THREAD_ARTIFACT_READ_CHUNK_BYTES;
pub use model::DirectionalThreadSpawnEdge;
pub use model::DirectionalThreadSpawnEdgeStatus;
pub use model::ExtractionOutcome;
pub use model::MAX_THREAD_ARTIFACT_ID_BYTES;
pub use model::MAX_THREAD_ARTIFACT_IDENTITY_KEY_BYTES;
pub use model::MAX_THREAD_ARTIFACT_LIST_LIMIT;
pub use model::MAX_THREAD_ARTIFACT_PAYLOAD_BYTES;
pub use model::MAX_THREAD_ARTIFACT_READ_CHUNK_BYTES;
pub use model::MAX_THREAD_ARTIFACT_TYPE_BYTES;
pub use model::SortDirection;
pub use model::SortKey;
pub use model::Stage1JobClaim;
pub use model::Stage1JobClaimOutcome;
pub use model::Stage1Output;
pub use model::Stage1StartupClaimParams;
pub use model::ThreadArtifact;
pub use model::ThreadArtifactAttachmentOutcome;
pub use model::ThreadArtifactPage;
pub use model::ThreadArtifactReadEncoding;
pub use model::ThreadArtifactReadPage;
pub use model::ThreadArtifactReadResult;
pub use model::ThreadArtifactRemovalOutcome;
pub use model::ThreadGoal;
pub use model::ThreadGoalStatus;
pub use model::ThreadMetadata;
pub use model::ThreadMetadataBuilder;
pub use model::ThreadRelationFilter;
pub use model::ThreadSection;
pub use model::ThreadSectionAppearance;
pub use model::ThreadSectionsPage;
pub use model::ThreadsPage;
pub use runtime::ApprovedPlanGoalClaim;
pub use runtime::ExternalAgentConfigImportDetailsRecord;
pub use runtime::ExternalAgentConfigImportFailureRecord;
pub use runtime::ExternalAgentConfigImportHistoryRecord;
pub use runtime::ExternalAgentConfigImportSuccessRecord;
pub use runtime::GoalAccountingMode;
pub use runtime::GoalAccountingOutcome;
pub use runtime::GoalStore;
pub use runtime::GoalUpdate;
pub use runtime::MemoryStore;
pub use runtime::RemoteControlEnrollmentRecord;
pub use runtime::RuntimeDbBackup;
pub use runtime::SqliteIntegrityCheck;
pub use runtime::SqliteQueueStore;
pub use runtime::ThreadFilterOptions;
pub use runtime::backup_runtime_db_for_fresh_start;
pub use runtime::is_sqlite_corruption_error;
pub use runtime::open_thread_history_db;
pub use runtime::runtime_db_path_for_corruption_error;
pub use runtime::sqlite_error_detail_is_corruption;
pub use runtime::sqlite_error_detail_is_lock;
pub use runtime::sqlite_integrity_check;
pub use sqlite::RuntimeDbPath;
pub use telemetry::DbTelemetry;
pub use telemetry::DbTelemetryHandle;
pub use telemetry::install_process_db_telemetry;
pub use telemetry::record_backfill_gate;
pub use telemetry::record_fallback;
pub use workflow::DEFAULT_WORKFLOW_MAILBOX_CAPACITY;
pub use workflow::FleetMemberResult;
pub use workflow::FleetMemberResultOutcome;
pub use workflow::FleetOperation;
pub use workflow::FleetOperationKind;
pub use workflow::FleetOperationSnapshot;
pub use workflow::FleetOperationStatus;
pub use workflow::FleetRootState;
pub use workflow::FleetState;
pub use workflow::LiveSearchDocumentCreate;
pub use workflow::MAX_FORK_CONTEXT_ENTRIES;
pub use workflow::SearchCursor;
pub use workflow::SearchDocument;
pub use workflow::SearchDocumentCreate;
pub use workflow::SearchDocumentMetadata;
pub use workflow::SearchFilter;
pub use workflow::SearchGeneration;
pub use workflow::SearchMetadata;
pub use workflow::SearchPage;
pub use workflow::SearchRequest;
pub use workflow::SearchSourceKind;
pub use workflow::WorkflowBackfillBeginRequest;
pub use workflow::WorkflowBackfillClaim;
pub use workflow::WorkflowBackfillError;
pub use workflow::WorkflowBackfillFinalizeRequest;
pub use workflow::WorkflowBackfillIncrementalState;
pub use workflow::WorkflowBackfillJournalClaim;
pub use workflow::WorkflowBackfillJournalClaimRequest;
pub use workflow::WorkflowBackfillJournalCreate;
pub use workflow::WorkflowBackfillJournalEntry;
pub use workflow::WorkflowBackfillJournalStatus;
pub use workflow::WorkflowBackfillJournalUpdate;
pub use workflow::WorkflowBackfillResumeRequest;
pub use workflow::WorkflowBackfillState;
pub use workflow::WorkflowBackfillStatus;
pub use workflow::WorkflowBackfillWatermark;
pub use workflow::WorkflowCheckpoint;
pub use workflow::WorkflowCheckpointCreate;
pub use workflow::WorkflowForkContextEntry;
pub use workflow::WorkflowForkContextOrigin;
pub use workflow::WorkflowForkMetrics;
pub use workflow::WorkflowForkMetricsCreate;
pub use workflow::WorkflowForkTurns;
pub use workflow::WorkflowLeaseAcquireRequest;
pub use workflow::WorkflowLeaseAuthority;
pub use workflow::WorkflowLeaseConflict;
pub use workflow::WorkflowLeaseError;
pub use workflow::WorkflowLeaseExtendRequest;
pub use workflow::WorkflowLeaseMode;
pub use workflow::WorkflowLeaseOverride;
pub use workflow::WorkflowLeaseOverrideCreate;
pub use workflow::WorkflowLeaseOverrideUse;
pub use workflow::WorkflowLeasePath;
pub use workflow::WorkflowLeaseReleaseRequest;
pub use workflow::WorkflowLeaseState;
pub use workflow::WorkflowMailboxAckRequest;
pub use workflow::WorkflowMailboxChannel;
pub use workflow::WorkflowMailboxClaim;
pub use workflow::WorkflowMailboxClaimRequest;
pub use workflow::WorkflowMailboxError;
pub use workflow::WorkflowMailboxListRequest;
pub use workflow::WorkflowMailboxMessage;
pub use workflow::WorkflowMailboxMessageCreate;
pub use workflow::WorkflowMailboxState;
pub use workflow::WorkflowPathLease;
pub use workflow::WorkflowReceipt;
pub use workflow::WorkflowReceiptCreate;
pub use workflow::WorkflowReceiptCursor;
pub use workflow::WorkflowReceiptExportSelection;
pub use workflow::WorkflowReceiptFilter;
pub use workflow::WorkflowReceiptListRequest;
pub use workflow::WorkflowReceiptPage;
pub use workflow::WorkflowReceiptReference;
pub use workflow::WorkflowReceiptTag;
pub use workflow::WorkflowRun;
pub use workflow::WorkflowRunCreate;
pub use workflow::WorkflowRunCursor;
pub use workflow::WorkflowRunListFilter;
pub use workflow::WorkflowRunListRequest;
pub use workflow::WorkflowRunPage;
pub use workflow::WorkflowRunParamsDigest;
pub use workflow::WorkflowRunTransitionOutcome;
pub use workflow::WorkflowStore;
pub use workflow::WorkflowTerminalObservation;
pub use workflow::WorkflowTerminalProcessState;
pub use workflow::WorkflowThreadClass;

/// Maximum number of pending user submissions permitted for one thread.
pub const MAX_QUEUE_ITEMS: usize = 100;

/// Stable UUIDv7 identifying the built-in pinned thread section.
pub const PINNED_THREAD_SECTION_ID: &str = "01984de2-8f74-7c91-a3b2-5c5e937cf318";

/// User-facing name of the built-in pinned thread section.
pub const PINNED_THREAD_SECTION_NAME: &str = "Pinned";

/// Environment variable for overriding the SQLite state database home directory.
pub const SQLITE_HOME_ENV: &str = "CODEX_SQLITE_HOME";

/// Errors encountered during DB operations. Tags: [stage]
pub const DB_ERROR_METRIC: &str = "codex.db.error";
/// Metrics on backfill process. Tags: [status]
pub const DB_METRIC_BACKFILL: &str = "codex.db.backfill";
/// Metrics on backfill duration. Tags: [status]
pub const DB_METRIC_BACKFILL_DURATION_MS: &str = "codex.db.backfill.duration_ms";
/// SQLite initialization attempts. Tags: [status, phase, db, error]
pub const DB_INIT_METRIC: &str = "codex.sqlite.init.count";
/// SQLite initialization latency. Tags: [status, phase, db, error]
pub const DB_INIT_DURATION_METRIC: &str = "codex.sqlite.init.duration_ms";
/// Rollout fallback attempts. Tags: [caller, reason]
pub const DB_FALLBACK_METRIC: &str = "codex.sqlite.fallback.count";
/// SQLite log batch write attempts. Tags: [status, error]
pub const LOG_WRITE_METRIC: &str = "codex.sqlite.logs.write.count";
/// SQLite log batch write latency. Tags: [status, error]
pub const LOG_WRITE_DURATION_METRIC: &str = "codex.sqlite.logs.write.duration_ms";
/// Estimated bytes in each SQLite log batch. Tags: [status, error]
pub const LOG_WRITE_BYTES_METRIC: &str = "codex.sqlite.logs.write.bytes";
/// Number of entries in each SQLite log batch. Tags: [status, error]
pub const LOG_WRITE_ENTRIES_METRIC: &str = "codex.sqlite.logs.write.entries";
/// Largest estimated entry size in each SQLite log batch. Tags: [status, error]
pub const LOG_WRITE_MAX_ENTRY_BYTES_METRIC: &str = "codex.sqlite.logs.write.max_entry_bytes";
/// SQLite log entries discarded before they can be queued. Tags: [reason]
pub const LOG_QUEUE_DROPPED_METRIC: &str = "codex.sqlite.logs.queue.dropped";
