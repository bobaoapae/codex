//! Durable workflow coordination state and rebuildable search projections.
//!
//! Workflow state is kept in its own SQLite database so that coordination and
//! search failures cannot corrupt or block the primary thread metadata store.
//! Rollouts remain the canonical history; this store only owns bounded
//! coordination records and projections derived from that history.

mod backfill;
mod backfill_helpers;
mod backfill_incremental;
mod backfill_types;
mod checkpoints;
mod fleet;
mod fleet_helpers;
mod fleet_types;
mod fork_metrics;
mod fork_metrics_types;
mod lease;
mod lease_helpers;
mod lease_types;
mod mailbox;
mod mailbox_recovery;
mod mailbox_types;
mod receipts;
mod run_types;
mod runs;
mod search;
mod search_types;
mod store;
mod terminal;
mod types;

pub use backfill_types::WorkflowBackfillBeginRequest;
pub use backfill_types::WorkflowBackfillClaim;
pub use backfill_types::WorkflowBackfillError;
pub use backfill_types::WorkflowBackfillFinalizeRequest;
pub use backfill_types::WorkflowBackfillIncrementalState;
pub use backfill_types::WorkflowBackfillJournalClaim;
pub use backfill_types::WorkflowBackfillJournalClaimRequest;
pub use backfill_types::WorkflowBackfillJournalCreate;
pub use backfill_types::WorkflowBackfillJournalEntry;
pub use backfill_types::WorkflowBackfillJournalStatus;
pub use backfill_types::WorkflowBackfillJournalUpdate;
pub use backfill_types::WorkflowBackfillResumeRequest;
pub use backfill_types::WorkflowBackfillState;
pub use backfill_types::WorkflowBackfillStatus;
pub use backfill_types::WorkflowBackfillWatermark;
pub use fleet_types::FleetMemberResult;
pub use fleet_types::FleetMemberResultOutcome;
pub use fleet_types::FleetOperation;
pub use fleet_types::FleetOperationKind;
pub use fleet_types::FleetOperationSnapshot;
pub use fleet_types::FleetOperationStatus;
pub use fleet_types::FleetRootState;
pub use fleet_types::FleetState;
pub use fork_metrics_types::MAX_FORK_CONTEXT_ENTRIES;
pub use fork_metrics_types::WorkflowForkContextEntry;
pub use fork_metrics_types::WorkflowForkContextOrigin;
pub use fork_metrics_types::WorkflowForkMetrics;
pub use fork_metrics_types::WorkflowForkMetricsCreate;
pub use fork_metrics_types::WorkflowForkTurns;
pub use lease_types::WorkflowLeaseAcquireRequest;
pub use lease_types::WorkflowLeaseAuthority;
pub use lease_types::WorkflowLeaseConflict;
pub use lease_types::WorkflowLeaseError;
pub use lease_types::WorkflowLeaseExtendRequest;
pub use lease_types::WorkflowLeaseMode;
pub use lease_types::WorkflowLeaseOverride;
pub use lease_types::WorkflowLeaseOverrideCreate;
pub use lease_types::WorkflowLeaseOverrideUse;
pub use lease_types::WorkflowLeasePath;
pub use lease_types::WorkflowLeaseReleaseRequest;
pub use lease_types::WorkflowLeaseState;
pub use lease_types::WorkflowPathLease;
pub use mailbox_types::DEFAULT_WORKFLOW_MAILBOX_CAPACITY;
pub use mailbox_types::WorkflowMailboxAckRequest;
pub use mailbox_types::WorkflowMailboxChannel;
pub use mailbox_types::WorkflowMailboxClaim;
pub use mailbox_types::WorkflowMailboxClaimRequest;
pub use mailbox_types::WorkflowMailboxError;
pub use mailbox_types::WorkflowMailboxListRequest;
pub use mailbox_types::WorkflowMailboxMessage;
pub use mailbox_types::WorkflowMailboxMessageCreate;
pub use mailbox_types::WorkflowMailboxState;
pub use receipts::WorkflowReceipt;
pub use receipts::WorkflowReceiptCreate;
pub use receipts::WorkflowReceiptCursor;
pub use receipts::WorkflowReceiptExportSelection;
pub use receipts::WorkflowReceiptFilter;
pub use receipts::WorkflowReceiptListRequest;
pub use receipts::WorkflowReceiptPage;
pub use receipts::WorkflowReceiptReference;
pub use receipts::WorkflowReceiptTag;
pub use run_types::WorkflowCheckpoint;
pub use run_types::WorkflowCheckpointCreate;
pub use run_types::WorkflowRun;
pub use run_types::WorkflowRunCreate;
pub use run_types::WorkflowRunCursor;
pub use run_types::WorkflowRunListFilter;
pub use run_types::WorkflowRunListRequest;
pub use run_types::WorkflowRunPage;
pub use run_types::WorkflowRunParamsDigest;
pub use run_types::WorkflowRunTransitionOutcome;
pub use search_types::LiveSearchDocumentCreate;
pub use search_types::SearchCursor;
pub use search_types::SearchDocument;
pub use search_types::SearchDocumentCreate;
pub use search_types::SearchDocumentMetadata;
pub use search_types::SearchFilter;
pub use search_types::SearchGeneration;
pub use search_types::SearchMetadata;
pub use search_types::SearchPage;
pub use search_types::SearchRequest;
pub use search_types::SearchSourceKind;
pub use store::WorkflowStore;
pub use terminal::WorkflowTerminalObservation;
pub use terminal::WorkflowTerminalProcessState;
pub use types::WorkflowThreadClass;

#[cfg(test)]
#[path = "tests.rs"]
mod tests;

#[cfg(test)]
#[path = "mailbox_tests.rs"]
mod mailbox_tests;

#[cfg(test)]
#[path = "lease_tests.rs"]
mod lease_tests;

#[cfg(test)]
#[path = "backfill_tests.rs"]
mod backfill_tests;

#[cfg(test)]
#[path = "terminal_tests.rs"]
mod terminal_tests;

#[cfg(test)]
#[path = "fork_metrics_tests.rs"]
mod fork_metrics_tests;
