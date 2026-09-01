//! Private SQL, validation, and conflict helpers for path leases.

use anyhow::Result;
use anyhow::bail;
use sqlx::Row;
use sqlx::Sqlite;
use std::collections::BTreeMap;

use super::lease_types::*;
use super::types::*;

const LEASE_COLUMNS: &str = "lease_id, lease_token, root_run_id, owner_run_id,
    environment_id, path_display, path_key, mode, generation, state,
    issued_at_ms, expires_at_ms, released_at_ms, override_receipt_id";
const OVERRIDE_COLUMNS: &str = "override_id, token, root_run_id, paths_json,
    conflict_owner_run_ids_json, operation_digest, reason, receipt_id,
    generation, created_at_ms, consumed_at_ms";

pub(super) async fn expire_active_leases(
    tx: &mut sqlx::Transaction<'_, Sqlite>,
    now_ms: i64,
) -> Result<()> {
    sqlx::query(
        "UPDATE workflow_path_leases
         SET state = 'recoverable', generation = generation + 1
         WHERE state = 'active' AND expires_at_ms IS NOT NULL AND expires_at_ms <= ?",
    )
    .bind(now_ms)
    .execute(&mut **tx)
    .await?;
    Ok(())
}

pub(super) async fn active_leases_for_root(
    tx: &mut sqlx::Transaction<'_, Sqlite>,
    root_run_id: &str,
) -> Result<Vec<WorkflowPathLease>> {
    let rows = sqlx::query(sqlx::AssertSqlSafe(format!(
        "SELECT {LEASE_COLUMNS} FROM workflow_path_leases
         WHERE root_run_id = ? AND state = 'active'"
    )))
    .bind(root_run_id)
    .fetch_all(&mut **tx)
    .await?;
    rows.iter().map(lease_from_row).collect()
}

pub(super) fn collect_conflicts(
    existing: &[WorkflowPathLease],
    request: &WorkflowLeaseAcquireRequest,
    paths: &[WorkflowLeasePath],
) -> Vec<WorkflowLeaseConflict> {
    let mut conflicts = BTreeMap::new();
    for lease in existing {
        if lease.environment_id != request.environment_id
            || (request.mode == WorkflowLeaseMode::Read && lease.mode == WorkflowLeaseMode::Read)
            || !paths.iter().any(|path| paths_match(path, &lease.path))
        {
            continue;
        }
        conflicts.insert(
            lease.lease_id.clone(),
            WorkflowLeaseConflict {
                lease_id: lease.lease_id.clone(),
                owner_run_id: lease.owner_run_id.clone(),
                path: lease.path.clone(),
                mode: lease.mode,
            },
        );
    }
    conflicts.into_values().collect()
}

pub(super) async fn consume_override_in_tx(
    tx: &mut sqlx::Transaction<'_, Sqlite>,
    proof: &WorkflowLeaseOverrideUse,
    root_run_id: &str,
    paths: &[WorkflowLeasePath],
    conflicts: &[WorkflowLeaseConflict],
    now_ms: i64,
) -> Result<WorkflowLeaseOverride> {
    let row = sqlx::query(sqlx::AssertSqlSafe(format!(
        "SELECT {OVERRIDE_COLUMNS} FROM workflow_path_lease_overrides
         WHERE override_id = ?"
    )))
    .bind(&proof.override_id)
    .fetch_optional(&mut **tx)
    .await?;
    let Some(row) = row else {
        return Err(anyhow::Error::new(WorkflowLeaseError::OverrideMismatch {
            override_id: proof.override_id.clone(),
        }));
    };
    let record = override_from_row(&row)?;
    let mut conflict_owners = if conflicts.is_empty() {
        proof.conflict_owner_run_ids.clone()
    } else {
        conflicts
            .iter()
            .map(|conflict| conflict.owner_run_id.clone())
            .collect::<Vec<_>>()
    };
    conflict_owners.sort();
    conflict_owners.dedup();
    let root_matches = root_run_id.is_empty() || record.root_run_id == root_run_id;
    if !root_matches
        || record.token != proof.token
        || record.generation != proof.generation
        || record.operation_digest != proof.operation_digest
        || record.paths != paths
        || record.conflict_owner_run_ids != conflict_owners
        || record.consumed_at_ms.is_some()
    {
        return Err(anyhow::Error::new(WorkflowLeaseError::OverrideMismatch {
            override_id: proof.override_id.clone(),
        }));
    }
    let consumed_at_ms = now_ms;
    let row = sqlx::query(sqlx::AssertSqlSafe(format!(
        "UPDATE workflow_path_lease_overrides
         SET consumed_at_ms = ?
         WHERE override_id = ? AND token = ? AND generation = ?
           AND consumed_at_ms IS NULL
         RETURNING {OVERRIDE_COLUMNS}"
    )))
    .bind(consumed_at_ms)
    .bind(&proof.override_id)
    .bind(&proof.token)
    .bind(proof.generation)
    .fetch_optional(&mut **tx)
    .await?;
    let Some(row) = row else {
        return Err(anyhow::Error::new(WorkflowLeaseError::OverrideMismatch {
            override_id: proof.override_id.clone(),
        }));
    };
    override_from_row(&row)
}

pub(super) fn validate_acquire_request(
    request: &WorkflowLeaseAcquireRequest,
) -> Result<Vec<WorkflowLeasePath>> {
    validate_lease_id(&request.root_run_id)?;
    validate_lease_id(&request.owner_run_id)?;
    validate_optional_lease_id(request.environment_id.as_deref(), "lease environment id")?;
    if !(1..=MAX_LEASE_DURATION_MS).contains(&request.lease_duration_ms) {
        bail!("path lease duration must be between 1 and {MAX_LEASE_DURATION_MS} milliseconds");
    }
    let paths = canonical_paths(&request.paths)?;
    if let WorkflowLeaseAuthority::RootOverride(proof) = &request.authority {
        validate_override_use(proof)?;
    }
    Ok(paths)
}

pub(super) fn validate_release_request(request: &WorkflowLeaseReleaseRequest) -> Result<()> {
    validate_lease_id(&request.lease_id)?;
    validate_lease_token(&request.token)?;
    validate_nonnegative_i64(request.generation, "path lease generation")
}

pub(super) fn validate_override_create(
    request: &WorkflowLeaseOverrideCreate,
) -> Result<Vec<WorkflowLeasePath>> {
    validate_lease_id(&request.root_run_id)?;
    validate_bounded_nonempty(&request.operation_digest, 128, "lease operation digest")?;
    validate_bounded_nonempty(
        &request.reason,
        MAX_LEASE_REASON_BYTES,
        "lease override reason",
    )?;
    validate_lease_id(&request.receipt_id)?;
    canonical_paths(&request.paths)
}

pub(super) fn validate_override_use(proof: &WorkflowLeaseOverrideUse) -> Result<()> {
    validate_lease_id(&proof.override_id)?;
    validate_lease_token(&proof.token)?;
    validate_nonnegative_i64(proof.generation, "path lease override generation")?;
    validate_bounded_nonempty(&proof.operation_digest, 128, "lease operation digest")?;
    canonical_paths(&proof.paths)?;
    canonical_owner_ids(&proof.conflict_owner_run_ids)?;
    Ok(())
}

pub(super) fn validate_lease_id(value: &str) -> Result<()> {
    validate_text(value, MAX_ID_BYTES, "path lease id")?;
    if value.contains('\0') {
        bail!("path lease id must not contain NUL");
    }
    Ok(())
}

pub(super) fn validate_lease_token(value: &str) -> Result<()> {
    validate_text(value, MAX_ID_BYTES, "path lease token")?;
    if value.contains('\0') {
        bail!("path lease token must not contain NUL");
    }
    Ok(())
}

pub(super) fn validate_optional_lease_id(value: Option<&str>, name: &str) -> Result<()> {
    if let Some(value) = value {
        validate_text(value, MAX_ID_BYTES, name)?;
        if value.contains('\0') {
            bail!("{name} must not contain NUL");
        }
    }
    Ok(())
}

pub(super) fn validate_bounded_nonempty(value: &str, max_bytes: usize, name: &str) -> Result<()> {
    validate_text(value, max_bytes, name)?;
    if value.trim().is_empty() || value.contains('\0') {
        bail!("{name} must be non-empty and must not contain NUL");
    }
    Ok(())
}

pub(super) fn bounded_json<T: serde::Serialize>(value: &T, name: &str) -> Result<String> {
    let encoded = serde_json::to_string(value)?;
    validate_json_bytes(&encoded, name)?;
    Ok(encoded)
}

pub(super) fn lease_from_row(row: &sqlx::sqlite::SqliteRow) -> Result<WorkflowPathLease> {
    let path = WorkflowLeasePath::new(
        row.try_get::<String, _>("path_display")?,
        row.try_get::<String, _>("path_key")?,
    )?;
    Ok(WorkflowPathLease {
        lease_id: row.try_get("lease_id")?,
        token: row.try_get("lease_token")?,
        root_run_id: row.try_get("root_run_id")?,
        owner_run_id: row.try_get("owner_run_id")?,
        environment_id: row.try_get("environment_id")?,
        path,
        mode: WorkflowLeaseMode::from_str(&row.try_get::<String, _>("mode")?)?,
        generation: row.try_get("generation")?,
        state: WorkflowLeaseState::from_str(&row.try_get::<String, _>("state")?)?,
        issued_at_ms: row.try_get("issued_at_ms")?,
        expires_at_ms: row.try_get("expires_at_ms")?,
        released_at_ms: row.try_get("released_at_ms")?,
        override_receipt_id: row.try_get("override_receipt_id")?,
    })
}

pub(super) fn override_from_row(row: &sqlx::sqlite::SqliteRow) -> Result<WorkflowLeaseOverride> {
    let paths_json = row.try_get::<String, _>("paths_json")?;
    let owner_ids_json = row.try_get::<String, _>("conflict_owner_run_ids_json")?;
    validate_json_bytes(&paths_json, "path lease override paths")?;
    validate_json_bytes(&owner_ids_json, "path lease override owners")?;
    let paths = canonical_paths(&serde_json::from_str::<Vec<WorkflowLeasePath>>(
        &paths_json,
    )?)?;
    let conflict_owner_run_ids =
        canonical_owner_ids(&serde_json::from_str::<Vec<String>>(&owner_ids_json)?)?;
    Ok(WorkflowLeaseOverride {
        override_id: row.try_get("override_id")?,
        token: row.try_get("token")?,
        root_run_id: row.try_get("root_run_id")?,
        paths,
        conflict_owner_run_ids,
        operation_digest: row.try_get("operation_digest")?,
        reason: row.try_get("reason")?,
        receipt_id: row.try_get("receipt_id")?,
        generation: row.try_get("generation")?,
        created_at_ms: row.try_get("created_at_ms")?,
        consumed_at_ms: row.try_get("consumed_at_ms")?,
    })
}
