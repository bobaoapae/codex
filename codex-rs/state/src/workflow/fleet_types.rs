//! Durable fleet coordination records and bounded member results.

use anyhow::Result;
use anyhow::bail;

use super::types::MAX_ID_BYTES;
use super::types::MAX_PAGE_SIZE;
use super::types::MAX_STATUS_BYTES;
use super::types::validate_text;

pub(super) const MAX_FLEET_MEMBERS: u32 = MAX_PAGE_SIZE;
pub(super) const MAX_FLEET_MEMBER_DEPTH: i64 = 1_024;
pub(super) const MAX_FLEET_MEMBER_ORDER: i64 = 1_000_000;
pub(super) const MAX_FLEET_ERROR_BYTES: usize = 1_024;

/// Lifecycle state of a fleet root.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FleetRootState {
    Active,
    Suspended,
    Closed,
    Failed,
}

impl FleetRootState {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Active => "active",
            Self::Suspended => "suspended",
            Self::Closed => "closed",
            Self::Failed => "failed",
        }
    }

    pub(super) fn from_str(value: &str) -> Result<Self> {
        match value {
            "active" => Ok(Self::Active),
            "suspended" => Ok(Self::Suspended),
            "closed" => Ok(Self::Closed),
            "failed" => Ok(Self::Failed),
            _ => bail!("unknown fleet root state: {value}"),
        }
    }
}

/// Exclusive operation requested for one fleet root.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FleetOperationKind {
    Suspend,
    Resume,
    Close,
}

impl FleetOperationKind {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Suspend => "suspend",
            Self::Resume => "resume",
            Self::Close => "close",
        }
    }

    pub(super) fn from_str(value: &str) -> Result<Self> {
        match value {
            "suspend" => Ok(Self::Suspend),
            "resume" => Ok(Self::Resume),
            "close" => Ok(Self::Close),
            _ => bail!("unknown fleet operation kind: {value}"),
        }
    }
}

/// Persisted status of an exclusive fleet operation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FleetOperationStatus {
    Running,
    Recoverable,
    Complete,
    Failed,
}

impl FleetOperationStatus {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Running => "running",
            Self::Recoverable => "recoverable",
            Self::Complete => "complete",
            Self::Failed => "failed",
        }
    }

    pub(super) fn from_str(value: &str) -> Result<Self> {
        match value {
            "running" => Ok(Self::Running),
            "recoverable" => Ok(Self::Recoverable),
            "complete" => Ok(Self::Complete),
            "failed" => Ok(Self::Failed),
            _ => bail!("unknown fleet operation status: {value}"),
        }
    }

    pub(super) const fn is_terminal(self) -> bool {
        matches!(self, Self::Complete | Self::Failed)
    }
}

/// Current durable state of one fleet root.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FleetState {
    pub root_run_id: String,
    pub state: FleetRootState,
    pub generation: i64,
    pub admissions_sealed: bool,
    pub active_operation_id: Option<String>,
    pub updated_at_ms: i64,
}

/// Exclusive operation record for one fleet root.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FleetOperation {
    pub operation_id: String,
    pub root_run_id: String,
    pub kind: FleetOperationKind,
    pub status: FleetOperationStatus,
    pub expected_generation: i64,
    pub new_generation: i64,
    pub expected_member_count: u32,
    pub result_count: u32,
    pub partial: bool,
    pub created_at_ms: i64,
    pub updated_at_ms: i64,
}

/// One persisted result for a fleet member.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FleetMemberResult {
    pub operation_id: String,
    pub member_id: String,
    pub thread_id: Option<String>,
    pub run_id: Option<String>,
    pub requested_state: String,
    pub previous_state: Option<String>,
    pub final_state: Option<String>,
    pub success: bool,
    /// Callers must provide a redacted, bounded error string.
    pub error: Option<String>,
    pub depth: i64,
    pub order_index: i64,
    pub updated_at_ms: i64,
}

/// Result of recording a member result.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FleetMemberResultOutcome {
    Recorded(FleetMemberResult),
    AlreadyRecorded(FleetMemberResult),
}

/// Operation plus bounded member results returned by status reads.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FleetOperationSnapshot {
    pub operation: FleetOperation,
    pub results: Vec<FleetMemberResult>,
}

pub(super) fn validate_fleet_root_id(root_run_id: &str) -> Result<()> {
    validate_text(root_run_id, MAX_ID_BYTES, "fleet root run id")
}

pub(super) fn validate_operation_id(operation_id: &str) -> Result<()> {
    validate_text(operation_id, MAX_ID_BYTES, "fleet operation id")
}

pub(super) fn validate_member_result(result: &FleetMemberResult) -> Result<()> {
    validate_operation_id(&result.operation_id)?;
    validate_text(&result.member_id, MAX_ID_BYTES, "fleet member id")?;
    validate_optional_id(result.thread_id.as_deref(), "fleet member thread id")?;
    validate_optional_id(result.run_id.as_deref(), "fleet member run id")?;
    validate_text(
        &result.requested_state,
        MAX_STATUS_BYTES,
        "fleet requested state",
    )?;
    validate_optional_status(result.previous_state.as_deref(), "fleet previous state")?;
    validate_optional_status(result.final_state.as_deref(), "fleet final state")?;
    if let Some(error) = result.error.as_deref() {
        if error.is_empty() {
            bail!("fleet member error must not be empty");
        }
        if error.len() > MAX_FLEET_ERROR_BYTES {
            bail!("fleet member error exceeds {MAX_FLEET_ERROR_BYTES} bytes");
        }
        if error.contains('\0') {
            bail!("fleet member error must not contain NUL");
        }
    }
    if result.depth < 0 || result.depth > MAX_FLEET_MEMBER_DEPTH {
        bail!("fleet member depth must be between 0 and {MAX_FLEET_MEMBER_DEPTH}");
    }
    if result.order_index < 0 || result.order_index > MAX_FLEET_MEMBER_ORDER {
        bail!("fleet member order must be between 0 and {MAX_FLEET_MEMBER_ORDER}");
    }
    Ok(())
}

fn validate_optional_id(value: Option<&str>, name: &str) -> Result<()> {
    if let Some(value) = value {
        validate_text(value, MAX_ID_BYTES, name)?;
    }
    Ok(())
}

fn validate_optional_status(value: Option<&str>, name: &str) -> Result<()> {
    if let Some(value) = value {
        validate_text(value, MAX_STATUS_BYTES, name)?;
    }
    Ok(())
}
