//! Typed backfill coordination values and fenced lifecycle errors.

use anyhow::Result;
use anyhow::bail;
use serde::Deserialize;
use serde::Serialize;
use std::fmt;

use super::types::*;

pub(super) const MAX_BACKFILL_ERROR_BYTES: usize = 1_024;
pub(super) const MAX_BACKFILL_SOURCE_PATH_BYTES: usize = 4_096;
pub(super) const MAX_BACKFILL_CURSOR_BYTES: usize = 65_536;
pub(super) const MAX_BACKFILL_LEASE_MS: i64 = 86_400_000;

/// Frozen creation-order watermark used by one historical backfill run.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkflowBackfillWatermark {
    pub created_at_ms: i64,
    pub rollout_id: String,
}

impl WorkflowBackfillWatermark {
    pub fn new(created_at_ms: i64, rollout_id: impl Into<String>) -> Result<Self> {
        let watermark = Self {
            created_at_ms,
            rollout_id: rollout_id.into(),
        };
        validate_watermark(&watermark)?;
        Ok(watermark)
    }
}

/// Coordinator lifecycle for the frozen historical pass.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum WorkflowBackfillStatus {
    Pending,
    Processing,
    Complete,
    Recoverable,
    Failed,
}

impl WorkflowBackfillStatus {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Processing => "processing",
            Self::Complete => "complete",
            Self::Recoverable => "recoverable",
            Self::Failed => "failed",
        }
    }

    pub(super) fn from_str(value: &str) -> Result<Self> {
        match value {
            "pending" => Ok(Self::Pending),
            "processing" => Ok(Self::Processing),
            "complete" => Ok(Self::Complete),
            "recoverable" => Ok(Self::Recoverable),
            "failed" => Ok(Self::Failed),
            _ => bail!("unknown backfill status: {value}"),
        }
    }

    pub const fn is_blocking_finalize(self) -> bool {
        matches!(
            self,
            Self::Pending | Self::Processing | Self::Recoverable | Self::Failed
        )
    }
}

/// Per-rollout journal lifecycle.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum WorkflowBackfillJournalStatus {
    Pending,
    Processing,
    Complete,
    SkippedPermanent,
    Recoverable,
    Failed,
}

impl WorkflowBackfillJournalStatus {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Processing => "processing",
            Self::Complete => "complete",
            Self::SkippedPermanent => "skippedPermanent",
            Self::Recoverable => "recoverable",
            Self::Failed => "failed",
        }
    }

    pub(super) fn from_str(value: &str) -> Result<Self> {
        match value {
            "pending" => Ok(Self::Pending),
            "processing" => Ok(Self::Processing),
            "complete" => Ok(Self::Complete),
            "skippedPermanent" => Ok(Self::SkippedPermanent),
            "recoverable" => Ok(Self::Recoverable),
            "failed" => Ok(Self::Failed),
            _ => bail!("unknown backfill journal status: {value}"),
        }
    }

    pub const fn is_terminal(self) -> bool {
        matches!(self, Self::Complete | Self::SkippedPermanent)
    }
}

/// Request that freezes a historical watermark and claims the coordinator.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkflowBackfillBeginRequest {
    pub watermark: WorkflowBackfillWatermark,
    pub owner_id: String,
    pub lease_duration_ms: i64,
}

/// Resume a recoverable coordinator using its exact fence.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkflowBackfillResumeRequest {
    pub owner_id: String,
    pub token: String,
    pub generation: i64,
    pub lease_duration_ms: i64,
}

/// Fenced coordinator claim returned by begin/resume.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkflowBackfillClaim {
    pub watermark: WorkflowBackfillWatermark,
    pub owner_id: String,
    pub token: String,
    pub lease_id: String,
    pub generation: i64,
    pub lease_expires_at_ms: i64,
}

/// Durable coordinator state and its current fence.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkflowBackfillState {
    pub status: WorkflowBackfillStatus,
    pub watermark: Option<WorkflowBackfillWatermark>,
    pub last_success_at_ms: Option<i64>,
    pub updated_at_ms: i64,
    pub error: Option<String>,
    pub owner_id: Option<String>,
    pub owner_token: Option<String>,
    pub lease_id: Option<String>,
    pub lease_expires_at_ms: Option<i64>,
    pub generation: i64,
    pub generation_id: Option<i64>,
    pub cursor_json: Option<String>,
    pub source_size_bytes: Option<i64>,
    pub source_mtime_ms: Option<i64>,
}

/// New or renamed rollout source registered in the journal.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkflowBackfillJournalCreate {
    pub rollout_id: String,
    pub source_path: String,
    pub source_size_bytes: Option<i64>,
    pub source_mtime_ms: Option<i64>,
}

/// One journal entry, including its current CAS fence.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkflowBackfillJournalEntry {
    pub journal_id: i64,
    pub rollout_id: String,
    pub source_path: String,
    pub byte_offset: i64,
    pub rollout_ordinal: i64,
    pub status: WorkflowBackfillJournalStatus,
    pub error: Option<String>,
    pub updated_at_ms: i64,
    pub owner_id: Option<String>,
    pub owner_token: Option<String>,
    pub lease_id: Option<String>,
    pub lease_expires_at_ms: Option<i64>,
    pub generation: i64,
    pub generation_id: Option<i64>,
    pub cursor_json: Option<String>,
    pub source_size_bytes: Option<i64>,
    pub source_mtime_ms: Option<i64>,
}

/// Claim request for one rollout journal row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkflowBackfillJournalClaimRequest {
    pub rollout_id: String,
    pub owner_id: String,
    pub lease_duration_ms: i64,
}

/// Fenced journal claim returned by a successful claim.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkflowBackfillJournalClaim {
    pub entry: WorkflowBackfillJournalEntry,
    pub owner_id: String,
    pub token: String,
    pub generation: i64,
    pub lease_expires_at_ms: i64,
}

/// CAS update for journal progress or a terminal outcome.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkflowBackfillJournalUpdate {
    pub rollout_id: String,
    pub owner_id: String,
    pub token: String,
    pub generation: i64,
    pub source_path: String,
    pub byte_offset: i64,
    pub rollout_ordinal: i64,
    pub status: WorkflowBackfillJournalStatus,
    pub error: Option<String>,
    pub generation_id: Option<i64>,
    pub cursor_json: Option<String>,
    pub source_size_bytes: Option<i64>,
    pub source_mtime_ms: Option<i64>,
    pub lease_duration_ms: i64,
}

/// Fence used to finalize the historical coordinator.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkflowBackfillFinalizeRequest {
    pub owner_id: String,
    pub token: String,
    pub generation: i64,
}

/// Explicit incremental-capture state separate from the frozen pass.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkflowBackfillIncrementalState {
    pub status: WorkflowBackfillStatus,
    pub watermark: Option<WorkflowBackfillWatermark>,
    pub updated_at_ms: i64,
    pub error: Option<String>,
    pub owner_id: Option<String>,
    pub owner_token: Option<String>,
    pub lease_id: Option<String>,
    pub generation: i64,
}

/// Stable errors for coordinator CAS and finalization.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WorkflowBackfillError {
    Busy,
    MissingJournal {
        rollout_id: String,
    },
    MissingClaim,
    Stale,
    PendingWork {
        pending: u32,
        processing: u32,
        recoverable: u32,
        failed: u32,
    },
}

impl fmt::Display for WorkflowBackfillError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Busy => formatter.write_str("backfill coordinator is already owned"),
            Self::MissingJournal { rollout_id } => {
                write!(formatter, "backfill journal has no rollout {rollout_id}")
            }
            Self::MissingClaim => formatter.write_str("backfill claim is missing"),
            Self::Stale => formatter.write_str("backfill owner, token, or generation is stale"),
            Self::PendingWork {
                pending,
                processing,
                recoverable,
                failed,
            } => write!(
                formatter,
                "backfill cannot finalize with pending={pending}, processing={processing}, recoverable={recoverable}, failed={failed}"
            ),
        }
    }
}

impl std::error::Error for WorkflowBackfillError {}

pub(super) fn validate_watermark(watermark: &WorkflowBackfillWatermark) -> Result<()> {
    validate_nonnegative_i64(watermark.created_at_ms, "backfill watermark timestamp")?;
    validate_text(
        &watermark.rollout_id,
        MAX_ID_BYTES,
        "backfill watermark rollout id",
    )
}

pub(super) fn validate_owner(owner_id: &str) -> Result<()> {
    validate_text(owner_id, MAX_ID_BYTES, "backfill owner id")
}

pub(super) fn validate_token(token: &str) -> Result<()> {
    validate_text(token, MAX_ID_BYTES, "backfill token")
}

pub(super) fn validate_lease_duration(duration_ms: i64) -> Result<()> {
    if !(1..=MAX_BACKFILL_LEASE_MS).contains(&duration_ms) {
        bail!("backfill lease must be between 1 and {MAX_BACKFILL_LEASE_MS} milliseconds");
    }
    Ok(())
}

pub(super) fn validate_source_path(path: &str) -> Result<()> {
    validate_text(path, MAX_BACKFILL_SOURCE_PATH_BYTES, "backfill source path")?;
    if path.contains('\0') {
        bail!("backfill source path must not contain NUL");
    }
    Ok(())
}

pub(super) fn validate_error(error: Option<&str>) -> Result<()> {
    if let Some(error) = error {
        validate_text(error, MAX_BACKFILL_ERROR_BYTES, "backfill error")?;
        if error.contains('\0') {
            bail!("backfill error must not contain NUL");
        }
    }
    Ok(())
}

pub(super) fn validate_cursor(cursor: Option<&str>) -> Result<()> {
    if let Some(cursor) = cursor {
        validate_text(cursor, MAX_BACKFILL_CURSOR_BYTES, "backfill cursor")?;
    }
    Ok(())
}
