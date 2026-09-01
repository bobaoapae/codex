//! Transactional path leases and root-authorized one-shot overrides.

use anyhow::Result;
use uuid::Uuid;

use super::WorkflowStore;
use super::lease_helpers::*;
use super::lease_types::*;
use super::types::*;

const LEASE_COLUMNS: &str = "lease_id, lease_token, root_run_id, owner_run_id,
    environment_id, path_display, path_key, mode, generation, state,
    issued_at_ms, expires_at_ms, released_at_ms, override_receipt_id";
const OVERRIDE_COLUMNS: &str = "override_id, token, root_run_id, paths_json,
    conflict_owner_run_ids_json, operation_digest, reason, receipt_id,
    generation, created_at_ms, consumed_at_ms";

impl WorkflowStore {
    /// Acquire every requested path under one SQLite write transaction.
    /// Existing active leases are compared by normalized path components, so
    /// textual prefixes cannot produce false conflicts.
    pub async fn acquire_path_leases(
        &self,
        request: &WorkflowLeaseAcquireRequest,
    ) -> Result<Vec<WorkflowPathLease>> {
        let paths = validate_acquire_request(request)?;
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        let now = now_ms();
        expire_active_leases(&mut tx, now).await?;
        let existing = active_leases_for_root(&mut tx, &request.root_run_id).await?;
        let conflicts = collect_conflicts(&existing, request, &paths);
        let override_receipt_id = if conflicts.is_empty() {
            if let WorkflowLeaseAuthority::RootOverride(override_use) = &request.authority {
                tx.rollback().await?;
                return Err(anyhow::Error::new(WorkflowLeaseError::OverrideMismatch {
                    override_id: override_use.override_id.clone(),
                }));
            }
            None
        } else if let WorkflowLeaseAuthority::RootOverride(override_use) = &request.authority {
            let override_record = match consume_override_in_tx(
                &mut tx,
                override_use,
                &request.root_run_id,
                &paths,
                &conflicts,
                now,
            )
            .await
            {
                Ok(record) => record,
                Err(error) => {
                    tx.rollback().await?;
                    return Err(error);
                }
            };
            Some(override_record.receipt_id)
        } else {
            tx.rollback().await?;
            return Err(anyhow::Error::new(WorkflowLeaseError::Conflict {
                conflicts,
            }));
        };

        let expires_at_ms = now
            .checked_add(request.lease_duration_ms)
            .ok_or_else(|| anyhow::anyhow!("path lease expiry timestamp overflow"))?;
        let mut leases = Vec::with_capacity(paths.len());
        for path in paths {
            let lease_id = Uuid::new_v4().to_string();
            let token = Uuid::new_v4().to_string();
            sqlx::query(
                "INSERT INTO workflow_path_leases
                 (lease_id, lease_token, root_run_id, owner_run_id, environment_id,
                  path_display, path_key, mode, generation, state, issued_at_ms,
                  expires_at_ms, override_receipt_id)
                 VALUES (?, ?, ?, ?, ?, ?, ?, ?, 1, 'active', ?, ?, ?)",
            )
            .bind(&lease_id)
            .bind(&token)
            .bind(&request.root_run_id)
            .bind(&request.owner_run_id)
            .bind(&request.environment_id)
            .bind(&path.display)
            .bind(&path.comparison_key)
            .bind(request.mode.as_str())
            .bind(now)
            .bind(expires_at_ms)
            .bind(override_receipt_id.as_deref())
            .execute(&mut *tx)
            .await?;
            leases.push(WorkflowPathLease {
                lease_id,
                token,
                root_run_id: request.root_run_id.clone(),
                owner_run_id: request.owner_run_id.clone(),
                environment_id: request.environment_id.clone(),
                path,
                mode: request.mode,
                generation: 1,
                expires_at_ms: Some(expires_at_ms),
                state: WorkflowLeaseState::Active,
                issued_at_ms: now,
                released_at_ms: None,
                override_receipt_id: override_receipt_id.clone(),
            });
        }
        tx.commit().await?;
        Ok(leases)
    }

    /// Read one lease by opaque lease ID.
    pub async fn get_path_lease(&self, lease_id: &str) -> Result<Option<WorkflowPathLease>> {
        validate_lease_id(lease_id)?;
        sqlx::query(sqlx::AssertSqlSafe(format!(
            "SELECT {LEASE_COLUMNS} FROM workflow_path_leases WHERE lease_id = ?"
        )))
        .bind(lease_id)
        .fetch_optional(self.pool.as_ref())
        .await?
        .as_ref()
        .map(lease_from_row)
        .transpose()
    }

    /// Release a lease with its current fencing token and generation.
    /// Releasing an already terminal lease is idempotent for the same fence.
    pub async fn release_path_lease(
        &self,
        request: &WorkflowLeaseReleaseRequest,
    ) -> Result<WorkflowPathLease> {
        validate_release_request(request)?;
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        expire_active_leases(&mut tx, now_ms()).await?;
        let row = sqlx::query(sqlx::AssertSqlSafe(format!(
            "SELECT {LEASE_COLUMNS} FROM workflow_path_leases WHERE lease_id = ?"
        )))
        .bind(&request.lease_id)
        .fetch_optional(&mut *tx)
        .await?;
        let Some(row) = row else {
            tx.rollback().await?;
            return Err(anyhow::Error::new(WorkflowLeaseError::Missing {
                lease_id: request.lease_id.clone(),
            }));
        };
        let lease = lease_from_row(&row)?;
        if lease.token != request.token || lease.generation != request.generation {
            tx.rollback().await?;
            return Err(anyhow::Error::new(WorkflowLeaseError::Stale {
                lease_id: request.lease_id.clone(),
            }));
        }
        if lease.state != WorkflowLeaseState::Active {
            tx.commit().await?;
            return Ok(lease);
        }
        let released_at_ms = now_ms();
        let row = sqlx::query(sqlx::AssertSqlSafe(format!(
            "UPDATE workflow_path_leases
             SET state = 'released', released_at_ms = ?
             WHERE lease_id = ? AND lease_token = ? AND generation = ?
               AND state = 'active'
             RETURNING {LEASE_COLUMNS}"
        )))
        .bind(released_at_ms)
        .bind(&request.lease_id)
        .bind(&request.token)
        .bind(request.generation)
        .fetch_optional(&mut *tx)
        .await?;
        let Some(row) = row else {
            tx.rollback().await?;
            return Err(anyhow::Error::new(WorkflowLeaseError::Stale {
                lease_id: request.lease_id.clone(),
            }));
        };
        let lease = lease_from_row(&row)?;
        tx.commit().await?;
        Ok(lease)
    }

    /// Mark expired active leases recoverable and fence their prior claims.
    pub async fn expire_path_leases(&self, now_ms: i64) -> Result<Vec<WorkflowPathLease>> {
        validate_nonnegative_i64(now_ms, "path lease expiration timestamp")?;
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        let rows = sqlx::query(sqlx::AssertSqlSafe(format!(
            "UPDATE workflow_path_leases
             SET state = 'recoverable', generation = generation + 1
             WHERE state = 'active' AND expires_at_ms IS NOT NULL
               AND expires_at_ms <= ?
             RETURNING {LEASE_COLUMNS}"
        )))
        .bind(now_ms)
        .fetch_all(&mut *tx)
        .await?;
        let mut leases = rows
            .iter()
            .map(lease_from_row)
            .collect::<Result<Vec<_>>>()?;
        leases.sort_by(|left, right| {
            left.root_run_id
                .cmp(&right.root_run_id)
                .then_with(|| left.path.comparison_key.cmp(&right.path.comparison_key))
                .then_with(|| left.lease_id.cmp(&right.lease_id))
        });
        tx.commit().await?;
        Ok(leases)
    }

    /// List all leases for a root in deterministic normalized-path order.
    pub async fn list_path_leases(&self, root_run_id: &str) -> Result<Vec<WorkflowPathLease>> {
        validate_lease_id(root_run_id)?;
        let rows = sqlx::query(sqlx::AssertSqlSafe(format!(
            "SELECT {LEASE_COLUMNS} FROM workflow_path_leases
             WHERE root_run_id = ?
             ORDER BY path_key ASC, path_display ASC, lease_id ASC"
        )))
        .bind(root_run_id)
        .fetch_all(self.pool.as_ref())
        .await?;
        rows.iter().map(lease_from_row).collect()
    }

    /// Issue a root-authorized one-shot override for an exact conflict set.
    pub async fn issue_path_lease_override(
        &self,
        request: &WorkflowLeaseOverrideCreate,
    ) -> Result<WorkflowLeaseOverride> {
        let paths = validate_override_create(request)?;
        let owners = canonical_owner_ids(&request.conflict_owner_run_ids)?;
        let paths_json = bounded_json(&paths, "path lease override paths")?;
        let owners_json = bounded_json(&owners, "path lease override owners")?;
        let override_id = Uuid::new_v4().to_string();
        let token = Uuid::new_v4().to_string();
        let generation = 1;
        let created_at_ms = now_ms();
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        sqlx::query(
            "INSERT INTO workflow_path_lease_overrides
             (override_id, token, root_run_id, paths_json,
              conflict_owner_run_ids_json, operation_digest, reason, receipt_id,
              generation, created_at_ms)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(&override_id)
        .bind(&token)
        .bind(&request.root_run_id)
        .bind(&paths_json)
        .bind(&owners_json)
        .bind(&request.operation_digest)
        .bind(&request.reason)
        .bind(&request.receipt_id)
        .bind(generation)
        .bind(created_at_ms)
        .execute(&mut *tx)
        .await?;
        let row = sqlx::query(sqlx::AssertSqlSafe(format!(
            "SELECT {OVERRIDE_COLUMNS} FROM workflow_path_lease_overrides
             WHERE override_id = ?"
        )))
        .bind(&override_id)
        .fetch_one(&mut *tx)
        .await?;
        let record = override_from_row(&row)?;
        tx.commit().await?;
        Ok(record)
    }

    /// Read one override, including whether its one-shot token was consumed.
    pub async fn get_path_lease_override(
        &self,
        override_id: &str,
    ) -> Result<Option<WorkflowLeaseOverride>> {
        validate_lease_id(override_id)?;
        sqlx::query(sqlx::AssertSqlSafe(format!(
            "SELECT {OVERRIDE_COLUMNS} FROM workflow_path_lease_overrides
             WHERE override_id = ?"
        )))
        .bind(override_id)
        .fetch_optional(self.pool.as_ref())
        .await?
        .as_ref()
        .map(override_from_row)
        .transpose()
    }

    /// Find the newest unconsumed override whose root, digest, paths and
    /// conflict owners all match exactly. This supports a prepared one-shot
    /// root request without consuming mismatched or stale proofs.
    pub async fn find_unconsumed_path_lease_override(
        &self,
        root_run_id: &str,
        operation_digest: &str,
        paths: &[WorkflowLeasePath],
        conflict_owner_run_ids: &[String],
    ) -> Result<Option<WorkflowLeaseOverride>> {
        validate_lease_id(root_run_id)?;
        validate_bounded_nonempty(operation_digest, 128, "lease operation digest")?;
        let paths = canonical_paths(paths)?;
        let conflict_owner_run_ids = canonical_owner_ids(conflict_owner_run_ids)?;
        let rows = sqlx::query(sqlx::AssertSqlSafe(format!(
            "SELECT {OVERRIDE_COLUMNS} FROM workflow_path_lease_overrides
             WHERE root_run_id = ? AND operation_digest = ? AND consumed_at_ms IS NULL
             ORDER BY created_at_ms DESC, override_id DESC LIMIT 256"
        )))
        .bind(root_run_id)
        .bind(operation_digest)
        .fetch_all(self.pool.as_ref())
        .await?;
        for row in rows {
            let record = override_from_row(&row)?;
            if record.paths == paths && record.conflict_owner_run_ids == conflict_owner_run_ids {
                return Ok(Some(record));
            }
        }
        Ok(None)
    }

    /// Consume an override atomically after checking its exact proof fields.
    pub async fn consume_path_lease_override(
        &self,
        proof: &WorkflowLeaseOverrideUse,
    ) -> Result<WorkflowLeaseOverride> {
        validate_override_use(proof)?;
        let paths = canonical_paths(&proof.paths)?;
        let owners = canonical_owner_ids(&proof.conflict_owner_run_ids)?;
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        let record = match consume_override_in_tx(&mut tx, proof, "", &paths, &[], now_ms()).await {
            Ok(record) => record,
            Err(error) => {
                tx.rollback().await?;
                return Err(error);
            }
        };
        if record.paths != paths || record.conflict_owner_run_ids != owners {
            tx.rollback().await?;
            return Err(anyhow::Error::new(WorkflowLeaseError::OverrideMismatch {
                override_id: proof.override_id.clone(),
            }));
        }
        tx.commit().await?;
        Ok(record)
    }
}
