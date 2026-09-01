//! Normalized path-lease values, fencing requests, and override records.

use anyhow::Result;
use anyhow::bail;
use serde::Deserialize;
use serde::Serialize;
use std::fmt;
use std::path::Path;

use super::types::*;

pub(super) const MAX_LEASE_PATHS: usize = 128;
pub(super) const MAX_LEASE_OVERRIDE_OWNERS: usize = 128;
pub(super) const MAX_LEASE_REASON_BYTES: usize = 1_024;
pub(super) const MAX_LEASE_DURATION_MS: i64 = 86_400_000;

/// A path accepted by the lease layer after normalization by its caller.
/// `display` is preserved for diagnostics; `comparison_key` is used for
/// case/separator-insensitive component comparisons.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkflowLeasePath {
    pub display: String,
    pub comparison_key: String,
}

impl WorkflowLeasePath {
    pub fn new(display: impl Into<String>, comparison_key: impl Into<String>) -> Result<Self> {
        let path = Self {
            display: display.into(),
            comparison_key: comparison_key.into(),
        };
        validate_lease_path(&path)?;
        Ok(path)
    }

    pub(super) fn components(&self) -> Vec<&str> {
        self.comparison_key
            .split(['/', '\\'])
            .filter(|component| !component.is_empty())
            .collect()
    }
}

/// Access requested for one path lease.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum WorkflowLeaseMode {
    Read,
    Write,
}

impl WorkflowLeaseMode {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Read => "read",
            Self::Write => "write",
        }
    }

    pub(super) fn from_str(value: &str) -> Result<Self> {
        match value {
            "read" => Ok(Self::Read),
            "write" => Ok(Self::Write),
            _ => bail!("unknown lease mode: {value}"),
        }
    }
}

/// Durable lease lifecycle state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum WorkflowLeaseState {
    Active,
    Released,
    Expired,
    Recoverable,
}

impl WorkflowLeaseState {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Active => "active",
            Self::Released => "released",
            Self::Expired => "expired",
            Self::Recoverable => "recoverable",
        }
    }

    pub(super) fn from_str(value: &str) -> Result<Self> {
        match value {
            "active" => Ok(Self::Active),
            "released" => Ok(Self::Released),
            "expired" => Ok(Self::Expired),
            "recoverable" => Ok(Self::Recoverable),
            _ => bail!("unknown lease state: {value}"),
        }
    }
}

/// Request to acquire one or more normalized paths atomically.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkflowLeaseAcquireRequest {
    pub root_run_id: String,
    pub owner_run_id: String,
    pub environment_id: Option<String>,
    pub paths: Vec<WorkflowLeasePath>,
    pub mode: WorkflowLeaseMode,
    pub lease_duration_ms: i64,
    pub authority: WorkflowLeaseAuthority,
}

/// Authority used for a path acquisition. Root overrides are explicit and
/// carry their own one-shot proof instead of an opaque force flag.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WorkflowLeaseAuthority {
    Owner,
    RootOverride(WorkflowLeaseOverrideUse),
}

/// One durable path lease returned after acquisition or a read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkflowPathLease {
    pub lease_id: String,
    pub token: String,
    pub root_run_id: String,
    pub owner_run_id: String,
    pub environment_id: Option<String>,
    pub path: WorkflowLeasePath,
    pub mode: WorkflowLeaseMode,
    pub generation: i64,
    pub expires_at_ms: Option<i64>,
    pub state: WorkflowLeaseState,
    pub issued_at_ms: i64,
    pub released_at_ms: Option<i64>,
    pub override_receipt_id: Option<String>,
}

/// Fencing data required to release a lease.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkflowLeaseReleaseRequest {
    pub lease_id: String,
    pub token: String,
    pub generation: i64,
}

/// One active conflict found while acquiring a path set.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkflowLeaseConflict {
    pub lease_id: String,
    pub owner_run_id: String,
    pub path: WorkflowLeasePath,
    pub mode: WorkflowLeaseMode,
}

/// Root-authorized, one-shot override for an exact conflict set.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkflowLeaseOverrideCreate {
    pub root_run_id: String,
    pub paths: Vec<WorkflowLeasePath>,
    pub conflict_owner_run_ids: Vec<String>,
    pub operation_digest: String,
    pub reason: String,
    pub receipt_id: String,
}

/// Proof used to consume an override during acquisition.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkflowLeaseOverrideUse {
    pub override_id: String,
    pub token: String,
    pub generation: i64,
    pub operation_digest: String,
    pub paths: Vec<WorkflowLeasePath>,
    pub conflict_owner_run_ids: Vec<String>,
}

/// Persisted root override and its one-shot consumption state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkflowLeaseOverride {
    pub override_id: String,
    pub token: String,
    pub root_run_id: String,
    pub paths: Vec<WorkflowLeasePath>,
    pub conflict_owner_run_ids: Vec<String>,
    pub operation_digest: String,
    pub reason: String,
    pub receipt_id: String,
    pub generation: i64,
    pub created_at_ms: i64,
    pub consumed_at_ms: Option<i64>,
}

/// Stable typed path-lease errors.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WorkflowLeaseError {
    Conflict {
        conflicts: Vec<WorkflowLeaseConflict>,
    },
    Missing {
        lease_id: String,
    },
    Stale {
        lease_id: String,
    },
    OverrideMismatch {
        override_id: String,
    },
}

impl fmt::Display for WorkflowLeaseError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Conflict { conflicts } => {
                write!(
                    formatter,
                    "path lease conflicts with {} active lease(s)",
                    conflicts.len()
                )
            }
            Self::Missing { lease_id } => write!(formatter, "path lease {lease_id} does not exist"),
            Self::Stale { lease_id } => write!(formatter, "path lease {lease_id} is stale"),
            Self::OverrideMismatch { override_id } => {
                write!(
                    formatter,
                    "path lease override {override_id} is invalid or already consumed"
                )
            }
        }
    }
}

impl std::error::Error for WorkflowLeaseError {}

pub(super) fn canonical_paths(paths: &[WorkflowLeasePath]) -> Result<Vec<WorkflowLeasePath>> {
    if paths.is_empty() {
        bail!("path lease request must contain at least one path");
    }
    if paths.len() > MAX_LEASE_PATHS {
        bail!("path lease request exceeds {MAX_LEASE_PATHS} paths");
    }
    let mut canonical = paths.to_vec();
    for path in &canonical {
        validate_lease_path(path)?;
    }
    canonical.sort_by(|left, right| {
        left.comparison_key
            .cmp(&right.comparison_key)
            .then_with(|| left.display.cmp(&right.display))
    });
    let mut deduplicated = Vec::with_capacity(canonical.len());
    for path in canonical {
        if deduplicated
            .last()
            .is_some_and(|previous: &WorkflowLeasePath| {
                previous.comparison_key == path.comparison_key
            })
        {
            continue;
        }
        deduplicated.push(path);
    }
    Ok(deduplicated)
}

pub(super) fn canonical_owner_ids(owner_ids: &[String]) -> Result<Vec<String>> {
    if owner_ids.is_empty() {
        bail!("path lease override must name at least one conflict owner");
    }
    if owner_ids.len() > MAX_LEASE_OVERRIDE_OWNERS {
        bail!("path lease override exceeds {MAX_LEASE_OVERRIDE_OWNERS} conflict owners");
    }
    let mut owners = owner_ids.to_vec();
    for owner in &owners {
        validate_text(owner, MAX_ID_BYTES, "path lease conflict owner")?;
        if owner.contains('\0') {
            bail!("path lease conflict owner must not contain NUL");
        }
    }
    owners.sort();
    owners.dedup();
    Ok(owners)
}

pub(super) fn paths_match(left: &WorkflowLeasePath, right: &WorkflowLeasePath) -> bool {
    let left = left.components();
    let right = right.components();
    is_component_prefix(&left, &right) || is_component_prefix(&right, &left)
}

fn is_component_prefix(prefix: &[&str], value: &[&str]) -> bool {
    prefix.len() <= value.len() && prefix.iter().zip(value).all(|(left, right)| left == right)
}

pub(super) fn validate_lease_path(path: &WorkflowLeasePath) -> Result<()> {
    validate_absolute_path(&path.display, "lease display path")?;
    validate_absolute_path(&path.comparison_key, "lease comparison key")?;
    for component in path.components() {
        if matches!(component, "." | "..") {
            bail!("lease path must already be normalized");
        }
    }
    for component in path
        .display
        .split(['/', '\\'])
        .filter(|component| !component.is_empty())
    {
        if matches!(component, "." | "..") {
            bail!("lease path must already be normalized");
        }
    }
    Ok(())
}

fn validate_absolute_path(value: &str, name: &str) -> Result<()> {
    validate_text(value, MAX_PATH_BYTES, name)?;
    if value.contains('\0') {
        bail!("{name} must not contain NUL");
    }
    let bytes = value.as_bytes();
    let windows_drive = bytes.len() >= 3 && bytes[1] == b':' && matches!(bytes[2], b'/' | b'\\');
    if !(Path::new(value).is_absolute()
        || value.starts_with('/')
        || value.starts_with("\\\\")
        || windows_drive)
    {
        bail!("{name} must be absolute");
    }
    Ok(())
}
