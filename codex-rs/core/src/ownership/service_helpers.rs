use super::NormalizedLeasePath;
use super::OwnershipError;
use codex_exec_server::LOCAL_ENVIRONMENT_ID;
use codex_state::WorkflowLeaseConflict;
use codex_state::WorkflowLeaseError;
use codex_state::WorkflowLeasePath;
use codex_state::WorkflowLeaseState;
use codex_state::WorkflowPathLease;
use std::collections::BTreeSet;
use std::path::Path;
use std::time::Duration;

pub(super) fn state_path(path: &NormalizedLeasePath) -> Result<WorkflowLeasePath, OwnershipError> {
    let display = path.display_path().to_string_lossy().replace('\\', "/");
    let comparison_key = normalized_state_path(path.resolved_path());
    WorkflowLeasePath::new(display, comparison_key).map_err(|error| OwnershipError::State {
        message: error.to_string(),
    })
}

pub(super) fn duration_millis(duration: Duration) -> Result<i64, OwnershipError> {
    let millis =
        i64::try_from(duration.as_millis()).map_err(|_| OwnershipError::InvalidRequest {
            message: "lease duration is too large".to_string(),
        })?;
    if millis <= 0 || millis > 86_400_000 {
        return Err(OwnershipError::InvalidRequest {
            message: "lease duration must be between 1ms and 24h".to_string(),
        });
    }
    Ok(millis)
}

pub(super) fn validate_operation_digest(digest: &str) -> Result<(), OwnershipError> {
    if digest.trim().is_empty() || digest.len() > 128 || digest.contains('\0') {
        return Err(OwnershipError::InvalidRequest {
            message: "operation digest must be 1..=128 bytes".to_string(),
        });
    }
    Ok(())
}

pub(super) fn validate_override_reason(reason: &str) -> Result<(), OwnershipError> {
    if reason.trim().is_empty() || reason.len() > 1_024 || reason.contains('\0') {
        return Err(OwnershipError::InvalidRequest {
            message: "override reason must be 1..=1024 bytes".to_string(),
        });
    }
    Ok(())
}

pub(super) fn select_actor_leases(
    leases: &[WorkflowPathLease],
    paths: &[WorkflowLeasePath],
    owner_run_id: &str,
) -> Vec<WorkflowPathLease> {
    let mut selected = Vec::new();
    let mut seen = BTreeSet::new();
    for lease in leases.iter().filter(|lease| {
        lease.state == WorkflowLeaseState::Active && lease.owner_run_id == owner_run_id
    }) {
        if paths
            .iter()
            .any(|path| state_path_covers(&lease.path, path))
            && seen.insert(lease.lease_id.clone())
        {
            selected.push(lease.clone());
        }
    }
    selected
}

pub(super) fn state_path_covers(lease: &WorkflowLeasePath, requested: &WorkflowLeasePath) -> bool {
    let lease_components = path_components(&lease.comparison_key);
    let requested_components = path_components(&requested.comparison_key);
    lease_components.len() <= requested_components.len()
        && lease_components
            .iter()
            .zip(&requested_components)
            .all(|(left, right)| left == right)
}

pub(super) fn state_path_overlaps(left: &WorkflowLeasePath, right: &WorkflowLeasePath) -> bool {
    state_path_covers(left, right) || state_path_covers(right, left)
}

pub(super) fn lease_environment_matches(
    lease_environment: Option<&str>,
    expected_environment: &str,
) -> bool {
    lease_environment.unwrap_or(LOCAL_ENVIRONMENT_ID) == expected_environment
}

fn normalized_state_path(path: &Path) -> String {
    let value = path.to_string_lossy().replace('\\', "/");
    #[cfg(windows)]
    {
        value.to_ascii_lowercase()
    }
    #[cfg(not(windows))]
    {
        value
    }
}

fn path_components(path: &str) -> Vec<String> {
    path.split(['/', '\\'])
        .filter(|component| !component.is_empty())
        .map(|component| {
            #[cfg(windows)]
            {
                component.to_ascii_lowercase()
            }
            #[cfg(not(windows))]
            {
                component.to_string()
            }
        })
        .collect()
}

pub(super) fn map_state_error(error: anyhow::Error) -> OwnershipError {
    if let Some(error) = error.downcast_ref::<WorkflowLeaseError>() {
        return match error {
            WorkflowLeaseError::Conflict { conflicts } => OwnershipError::Conflict {
                conflicts: conflicts.clone(),
                operation_digest: String::new(),
                paths: conflicts
                    .iter()
                    .map(|conflict| conflict.path.clone())
                    .collect(),
            },
            WorkflowLeaseError::Missing { .. } => OwnershipError::State {
                message: "ownership lease was not found".to_string(),
            },
            WorkflowLeaseError::Stale { .. } => OwnershipError::State {
                message: "ownership lease fence is stale".to_string(),
            },
            WorkflowLeaseError::OverrideMismatch { .. } => OwnershipError::OverrideMismatch,
        };
    }
    OwnershipError::State {
        message: error.to_string(),
    }
}

pub(super) fn conflict_error(
    conflicts: Vec<WorkflowPathLease>,
    operation_digest: String,
    paths: Vec<WorkflowLeasePath>,
) -> OwnershipError {
    OwnershipError::Conflict {
        conflicts: conflicts
            .into_iter()
            .map(|lease| WorkflowLeaseConflict {
                lease_id: lease.lease_id,
                owner_run_id: lease.owner_run_id,
                path: lease.path,
                mode: lease.mode,
            })
            .collect(),
        operation_digest,
        paths,
    }
}
