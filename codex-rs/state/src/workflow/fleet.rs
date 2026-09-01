//! Crash-safe fleet lifecycle coordination.

use super::WorkflowStore;
use super::fleet_helpers::*;
use super::fleet_types::*;
use super::search_types::bool_as_sql;
use super::types::now_ms;
use anyhow::Result;
use anyhow::bail;
use uuid::Uuid;

impl WorkflowStore {
    /// Return the durable state of a fleet root, if it has been registered.
    pub async fn get_fleet_state(&self, root_run_id: &str) -> Result<Option<FleetState>> {
        validate_fleet_root_id(root_run_id)?;
        let row = sqlx::query(
            "SELECT root_run_id, state, generation, admissions_sealed,
                    active_operation_id, updated_at_ms
             FROM workflow_fleet_roots WHERE root_run_id = ?",
        )
        .bind(root_run_id)
        .fetch_optional(self.pool.as_ref())
        .await?;
        row.as_ref().map(fleet_state_from_row).transpose()
    }

    /// Seal new admissions for a root using a generation compare-and-swap.
    ///
    /// Sealing is idempotent at the expected generation. A closed root remains
    /// sealed and is never reopened.
    pub async fn seal_fleet_admissions(
        &self,
        root_run_id: &str,
        expected_generation: i64,
    ) -> Result<FleetState> {
        validate_fleet_root_id(root_run_id)?;
        validate_generation(expected_generation)?;
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        ensure_fleet_root(&mut tx, root_run_id).await?;
        let current = fleet_state_by_id(&mut tx, root_run_id).await?;
        if current.generation != expected_generation {
            bail!("stale fleet generation");
        }
        if current.active_operation_id.is_some() {
            bail!("fleet operation is already active");
        }
        if current.admissions_sealed {
            tx.commit().await?;
            return Ok(current);
        }
        let new_generation = next_generation(current.generation)?;
        let row = sqlx::query(
            "UPDATE workflow_fleet_roots
             SET admissions_sealed = 1, generation = ?, updated_at_ms = ?
             WHERE root_run_id = ? AND generation = ? AND active_operation_id IS NULL
             RETURNING root_run_id, state, generation, admissions_sealed,
                       active_operation_id, updated_at_ms",
        )
        .bind(new_generation)
        .bind(now_ms())
        .bind(root_run_id)
        .bind(expected_generation)
        .fetch_one(&mut *tx)
        .await?;
        tx.commit().await?;
        fleet_state_from_row(&row)
    }

    /// Begin one exclusive lifecycle operation for a fleet root.
    ///
    /// A recoverable operation remains the active CAS fence until this method
    /// receives an allowed explicit resume or close. The old operation and all
    /// of its member results remain durable while its root pointer is cleared
    /// in the same transaction that admits the replacement operation.
    pub async fn begin_fleet_operation(
        &self,
        root_run_id: &str,
        kind: FleetOperationKind,
        expected_generation: i64,
        expected_member_count: u32,
    ) -> Result<FleetOperation> {
        validate_fleet_root_id(root_run_id)?;
        validate_generation(expected_generation)?;
        if expected_member_count > MAX_FLEET_MEMBERS {
            bail!("fleet member count exceeds {MAX_FLEET_MEMBERS}");
        }

        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        ensure_fleet_root(&mut tx, root_run_id).await?;
        let current = fleet_state_by_id(&mut tx, root_run_id).await?;
        if current.generation != expected_generation {
            bail!("stale fleet generation");
        }
        let recovered_operation_kind =
            if let Some(active_operation_id) = current.active_operation_id.as_deref() {
                let active_operation = fleet_operation_by_id(&mut tx, active_operation_id).await?;
                match active_operation.status {
                    FleetOperationStatus::Running => {
                        bail!("fleet operation is already active");
                    }
                    FleetOperationStatus::Recoverable => {
                        if active_operation.new_generation != expected_generation {
                            bail!("stale fleet generation");
                        }
                        validate_recoverable_operation_transition(
                            current.state,
                            active_operation.kind,
                            kind,
                        )?;
                        let cleared = sqlx::query(
                            "UPDATE workflow_fleet_roots
                         SET active_operation_id = NULL, updated_at_ms = ?
                         WHERE root_run_id = ? AND generation = ?
                           AND active_operation_id = ?",
                        )
                        .bind(now_ms())
                        .bind(root_run_id)
                        .bind(expected_generation)
                        .bind(active_operation_id)
                        .execute(&mut *tx)
                        .await?;
                        if cleared.rows_affected() != 1 {
                            bail!("stale fleet generation");
                        }
                        Some(active_operation.kind)
                    }
                    FleetOperationStatus::Complete | FleetOperationStatus::Failed => {
                        bail!("fleet operation is already final");
                    }
                }
            } else {
                None
            };
        if recovered_operation_kind.is_none() {
            validate_operation_transition(current.state, kind)?;
        }
        let new_generation = next_generation(expected_generation)?;
        let operation_id = Uuid::now_v7().to_string();
        let timestamp = now_ms();
        sqlx::query(
            "INSERT INTO workflow_fleet_operations
             (operation_id, root_run_id, kind, status, expected_generation,
              new_generation, expected_member_count, partial, created_at_ms, updated_at_ms)
             VALUES (?, ?, ?, 'running', ?, ?, ?, 0, ?, ?)",
        )
        .bind(&operation_id)
        .bind(root_run_id)
        .bind(kind.as_str())
        .bind(expected_generation)
        .bind(new_generation)
        .bind(i64::from(expected_member_count))
        .bind(timestamp)
        .bind(timestamp)
        .execute(&mut *tx)
        .await?;
        let sealed = matches!(
            kind,
            FleetOperationKind::Suspend | FleetOperationKind::Close
        ) || current.admissions_sealed;
        let root_update = sqlx::query(
            "UPDATE workflow_fleet_roots
             SET generation = ?, admissions_sealed = ?, active_operation_id = ?, updated_at_ms = ?
             WHERE root_run_id = ? AND generation = ? AND active_operation_id IS NULL",
        )
        .bind(new_generation)
        .bind(bool_as_sql(sealed))
        .bind(&operation_id)
        .bind(timestamp)
        .bind(root_run_id)
        .bind(expected_generation)
        .execute(&mut *tx)
        .await?;
        if root_update.rows_affected() != 1 {
            bail!("stale fleet generation");
        }
        tx.commit().await?;
        Ok(FleetOperation {
            operation_id,
            root_run_id: root_run_id.to_string(),
            kind,
            status: FleetOperationStatus::Running,
            expected_generation,
            new_generation,
            expected_member_count,
            result_count: 0,
            partial: false,
            created_at_ms: timestamp,
            updated_at_ms: timestamp,
        })
    }

    /// Record a member result exactly once for one operation.
    pub async fn record_fleet_member_result(
        &self,
        result: &FleetMemberResult,
    ) -> Result<FleetMemberResultOutcome> {
        validate_member_result(result)?;
        if result.thread_id.is_none() && result.run_id.is_none() {
            bail!("fleet member result needs a thread id or run id");
        }
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        let operation = fleet_operation_by_id(&mut tx, &result.operation_id).await?;
        let root = fleet_state_by_id(&mut tx, &operation.root_run_id).await?;
        if !operation.status.is_terminal()
            && root.active_operation_id.as_deref() != Some(result.operation_id.as_str())
        {
            bail!("fleet operation is no longer active");
        }
        let existing = sqlx::query(
            "SELECT operation_id, member_id, thread_id, run_id, requested_state,
                    previous_state, final_state, success, error, depth, order_index, updated_at_ms
             FROM workflow_fleet_member_results
             WHERE operation_id = ? AND member_id = ?",
        )
        .bind(&result.operation_id)
        .bind(&result.member_id)
        .fetch_optional(&mut *tx)
        .await?;
        if let Some(row) = existing {
            let existing = member_result_from_row(&row)?;
            if same_member_result(&existing, result) {
                tx.commit().await?;
                return Ok(FleetMemberResultOutcome::AlreadyRecorded(existing));
            }
            bail!("fleet member result conflicts with an existing result");
        }
        if operation.status.is_terminal() {
            bail!("fleet operation is already final");
        }
        if operation.result_count >= operation.expected_member_count {
            bail!("fleet operation already has all expected member results");
        }
        let mut stored = result.clone();
        stored.error = redact_error(result.error.as_deref());
        stored.updated_at_ms = now_ms();
        sqlx::query(
            "INSERT INTO workflow_fleet_member_results
             (operation_id, member_id, thread_id, run_id, requested_state,
              previous_state, final_state, success, error, depth, order_index, updated_at_ms)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(&stored.operation_id)
        .bind(&stored.member_id)
        .bind(stored.thread_id.as_deref())
        .bind(stored.run_id.as_deref())
        .bind(&stored.requested_state)
        .bind(stored.previous_state.as_deref())
        .bind(stored.final_state.as_deref())
        .bind(bool_as_sql(stored.success))
        .bind(stored.error.as_deref())
        .bind(stored.depth)
        .bind(stored.order_index)
        .bind(stored.updated_at_ms)
        .execute(&mut *tx)
        .await?;
        sqlx::query(
            "UPDATE workflow_fleet_operations SET updated_at_ms = ? WHERE operation_id = ?",
        )
        .bind(stored.updated_at_ms)
        .bind(&stored.operation_id)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(FleetMemberResultOutcome::Recorded(stored))
    }

    /// Read one operation and its bounded, deterministic member results.
    pub async fn get_fleet_operation_status(
        &self,
        operation_id: &str,
    ) -> Result<Option<FleetOperationSnapshot>> {
        validate_operation_id(operation_id)?;
        let Some(operation) = self.get_fleet_operation(operation_id).await? else {
            return Ok(None);
        };
        let rows = sqlx::query(
            "SELECT operation_id, member_id, thread_id, run_id, requested_state,
                    previous_state, final_state, success, error, depth, order_index, updated_at_ms
             FROM workflow_fleet_member_results
             WHERE operation_id = ? ORDER BY depth, order_index, member_id LIMIT ?",
        )
        .bind(operation_id)
        .bind(i64::from(MAX_FLEET_MEMBERS))
        .fetch_all(self.pool.as_ref())
        .await?;
        let results = rows
            .iter()
            .map(member_result_from_row)
            .collect::<Result<Vec<_>>>()?;
        Ok(Some(FleetOperationSnapshot { operation, results }))
    }

    /// Explicitly mark a running operation recoverable after a detected crash.
    /// No member is restarted and no operation is retried. The root pointer
    /// and admission seal remain in place until a later explicit resume or
    /// close performs the fenced recovery and starts a replacement operation.
    pub async fn recover_fleet_operation(&self, operation_id: &str) -> Result<bool> {
        validate_operation_id(operation_id)?;
        Ok(sqlx::query(
            "UPDATE workflow_fleet_operations
             SET status = 'recoverable',
                 partial = CASE
                     WHEN (SELECT COUNT(*) FROM workflow_fleet_member_results r
                           WHERE r.operation_id = workflow_fleet_operations.operation_id)
                          < expected_member_count
                     THEN 1 ELSE partial END,
                 updated_at_ms = ?
             WHERE operation_id = ? AND status = 'running'",
        )
        .bind(now_ms())
        .bind(operation_id)
        .execute(self.pool.as_ref())
        .await?
        .rows_affected()
            == 1)
    }

    /// Finish an operation, or leave it recoverable when results are partial.
    pub async fn finalize_fleet_operation(
        &self,
        operation_id: &str,
        requested_status: FleetOperationStatus,
    ) -> Result<FleetOperation> {
        validate_operation_id(operation_id)?;
        if requested_status == FleetOperationStatus::Running {
            bail!("running is not a finalization status");
        }
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        let operation = fleet_operation_by_id(&mut tx, operation_id).await?;
        if operation.status.is_terminal() {
            if operation.status == requested_status {
                tx.commit().await?;
                return Ok(operation);
            }
            bail!("fleet operation is already final");
        }
        let root = fleet_state_by_id(&mut tx, &operation.root_run_id).await?;
        if root.active_operation_id.as_deref() != Some(operation_id) {
            bail!("fleet operation is no longer active");
        }
        let result_count = sqlx::query_scalar::<_, i64>(
            "SELECT COUNT(*) FROM workflow_fleet_member_results WHERE operation_id = ?",
        )
        .bind(operation_id)
        .fetch_one(&mut *tx)
        .await?;
        let failed_result_count = sqlx::query_scalar::<_, i64>(
            "SELECT COUNT(*) FROM workflow_fleet_member_results
             WHERE operation_id = ? AND success = 0",
        )
        .bind(operation_id)
        .fetch_one(&mut *tx)
        .await?;
        let partial = result_count < i64::from(operation.expected_member_count);
        let status = if requested_status == FleetOperationStatus::Complete
            && (partial || failed_result_count > 0)
        {
            FleetOperationStatus::Recoverable
        } else {
            requested_status
        };
        let timestamp = now_ms();
        sqlx::query(
            "UPDATE workflow_fleet_operations
             SET status = ?, partial = ?, updated_at_ms = ?
             WHERE operation_id = ? AND status IN ('running', 'recoverable')",
        )
        .bind(status.as_str())
        .bind(bool_as_sql(partial))
        .bind(timestamp)
        .bind(operation_id)
        .execute(&mut *tx)
        .await?;
        if status.is_terminal() {
            let next_state = if status == FleetOperationStatus::Failed {
                FleetRootState::Failed
            } else {
                match operation.kind {
                    FleetOperationKind::Suspend => FleetRootState::Suspended,
                    FleetOperationKind::Resume => FleetRootState::Active,
                    FleetOperationKind::Close => FleetRootState::Closed,
                }
            };
            let sealed = !matches!(next_state, FleetRootState::Active);
            sqlx::query(
                "UPDATE workflow_fleet_roots
                 SET state = ?, admissions_sealed = ?, active_operation_id = NULL,
                     updated_at_ms = ?
                 WHERE root_run_id = ? AND active_operation_id = ?",
            )
            .bind(next_state.as_str())
            .bind(bool_as_sql(sealed))
            .bind(timestamp)
            .bind(&operation.root_run_id)
            .bind(operation_id)
            .execute(&mut *tx)
            .await?;
        }
        let updated = fleet_operation_by_id(&mut tx, operation_id).await?;
        tx.commit().await?;
        Ok(updated)
    }

    /// Alias used by callers that describe finalization as finishing.
    pub async fn finish_fleet_operation(
        &self,
        operation_id: &str,
        requested_status: FleetOperationStatus,
    ) -> Result<FleetOperation> {
        self.finalize_fleet_operation(operation_id, requested_status)
            .await
    }

    async fn get_fleet_operation(&self, operation_id: &str) -> Result<Option<FleetOperation>> {
        let row = sqlx::query(
            "SELECT operation_id, root_run_id, kind, status, expected_generation,
                    new_generation, expected_member_count, partial, created_at_ms, updated_at_ms,
                    (SELECT COUNT(*) FROM workflow_fleet_member_results r
                     WHERE r.operation_id = o.operation_id) AS result_count
             FROM workflow_fleet_operations o WHERE operation_id = ?",
        )
        .bind(operation_id)
        .fetch_optional(self.pool.as_ref())
        .await?;
        row.as_ref().map(fleet_operation_from_row).transpose()
    }
}

#[cfg(test)]
#[path = "fleet_tests.rs"]
mod tests;
