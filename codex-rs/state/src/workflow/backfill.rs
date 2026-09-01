//! Crash-safe coordinator for historical rollout backfill.

use anyhow::Result;
use anyhow::bail;
use uuid::Uuid;

use super::WorkflowStore;
use super::backfill_helpers::*;
use super::backfill_types::*;
use super::types::*;

impl WorkflowStore {
    /// Read the frozen historical coordinator state without claiming it.
    pub async fn get_backfill_coordinator_state(&self) -> Result<WorkflowBackfillState> {
        let row = sqlx::query(sqlx::AssertSqlSafe(format!(
            "SELECT {STATE_COLUMNS} FROM workflow_backfill_state WHERE id = 1"
        )))
        .fetch_one(self.pool.as_ref())
        .await?;
        state_from_row(&row)
    }

    /// Freeze a preview-provided watermark and claim the historical pass.
    /// Existing processing claims are never silently stolen; callers must
    /// explicitly reclaim an expired claim before starting again.
    pub async fn begin_backfill(
        &self,
        request: &WorkflowBackfillBeginRequest,
    ) -> Result<WorkflowBackfillClaim> {
        validate_begin_request(request)?;
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        let current = load_state(&mut tx).await?;
        let now = now_ms();
        if current.status == WorkflowBackfillStatus::Complete {
            tx.rollback().await?;
            return Err(anyhow::Error::new(WorkflowBackfillError::Busy));
        }
        if current.status == WorkflowBackfillStatus::Processing {
            if current.owner_id.as_deref() == Some(request.owner_id.as_str())
                && current.watermark.as_ref() == Some(&request.watermark)
                && current
                    .lease_expires_at_ms
                    .is_some_and(|expires_at_ms| expires_at_ms > now)
            {
                let (Some(token), Some(lease_id), Some(expires_at_ms)) = (
                    current.owner_token.clone(),
                    current.lease_id.clone(),
                    current.lease_expires_at_ms,
                ) else {
                    tx.rollback().await?;
                    return Err(anyhow::Error::new(WorkflowBackfillError::Stale));
                };
                tx.commit().await?;
                return Ok(WorkflowBackfillClaim {
                    watermark: request.watermark.clone(),
                    owner_id: request.owner_id.clone(),
                    token,
                    lease_id,
                    generation: current.generation,
                    lease_expires_at_ms: expires_at_ms,
                });
            }
            tx.rollback().await?;
            return Err(anyhow::Error::new(WorkflowBackfillError::Busy));
        }
        if current
            .watermark
            .as_ref()
            .is_some_and(|watermark| watermark != &request.watermark)
        {
            tx.rollback().await?;
            return Err(anyhow::Error::new(WorkflowBackfillError::Stale));
        }
        let generation = next_generation(current.generation)?;
        let lease_id = Uuid::new_v4().to_string();
        let token = Uuid::new_v4().to_string();
        let updated_at_ms = now;
        let lease_expires_at_ms = updated_at_ms
            .checked_add(request.lease_duration_ms)
            .ok_or_else(|| anyhow::anyhow!("backfill lease timestamp overflow"))?;
        let row = sqlx::query(sqlx::AssertSqlSafe(format!(
            "UPDATE workflow_backfill_state
             SET status = 'processing', watermark_created_at_ms = ?,
                 watermark_rollout_id = ?, updated_at_ms = ?, error_json = NULL,
                 owner_id = ?, owner_token = ?, lease_id = ?,
                 lease_expires_at_ms = ?, generation = ?
             WHERE id = 1 AND generation = ?
             RETURNING {STATE_COLUMNS}"
        )))
        .bind(request.watermark.created_at_ms)
        .bind(&request.watermark.rollout_id)
        .bind(updated_at_ms)
        .bind(&request.owner_id)
        .bind(&token)
        .bind(&lease_id)
        .bind(lease_expires_at_ms)
        .bind(generation)
        .bind(current.generation)
        .fetch_optional(&mut *tx)
        .await?;
        let Some(_row) = row else {
            tx.rollback().await?;
            return Err(anyhow::Error::new(WorkflowBackfillError::Stale));
        };
        tx.commit().await?;
        Ok(WorkflowBackfillClaim {
            watermark: request.watermark.clone(),
            owner_id: request.owner_id.clone(),
            token,
            lease_id,
            generation,
            lease_expires_at_ms,
        })
    }

    /// Alias emphasizing that begin is the coordinator claim operation.
    pub async fn claim_backfill(
        &self,
        request: &WorkflowBackfillBeginRequest,
    ) -> Result<WorkflowBackfillClaim> {
        self.begin_backfill(request).await
    }

    /// Renew/resume an active coordinator claim with an exact fence.
    pub async fn resume_backfill(
        &self,
        request: &WorkflowBackfillResumeRequest,
    ) -> Result<WorkflowBackfillClaim> {
        validate_resume_request(request)?;
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        let current = load_state(&mut tx).await?;
        let now = now_ms();
        if current.status != WorkflowBackfillStatus::Processing
            || current.owner_id.as_deref() != Some(request.owner_id.as_str())
            || current.owner_token.as_deref() != Some(request.token.as_str())
            || current.generation != request.generation
            || current
                .lease_expires_at_ms
                .is_none_or(|expires_at_ms| expires_at_ms <= now)
        {
            tx.rollback().await?;
            return Err(anyhow::Error::new(WorkflowBackfillError::Stale));
        }
        let generation = next_generation(current.generation)?;
        let lease_expires_at_ms = now
            .checked_add(request.lease_duration_ms)
            .ok_or_else(|| anyhow::anyhow!("backfill lease timestamp overflow"))?;
        let row = sqlx::query(sqlx::AssertSqlSafe(format!(
            "UPDATE workflow_backfill_state
             SET updated_at_ms = ?, lease_expires_at_ms = ?, generation = ?
             WHERE id = 1 AND status = 'processing' AND owner_id = ?
               AND owner_token = ? AND generation = ?
             RETURNING {STATE_COLUMNS}"
        )))
        .bind(now)
        .bind(lease_expires_at_ms)
        .bind(generation)
        .bind(&request.owner_id)
        .bind(&request.token)
        .bind(request.generation)
        .fetch_optional(&mut *tx)
        .await?;
        let Some(row) = row else {
            tx.rollback().await?;
            return Err(anyhow::Error::new(WorkflowBackfillError::Stale));
        };
        let state = state_from_row(&row)?;
        let Some(watermark) = state.watermark else {
            tx.rollback().await?;
            bail!("processing backfill has no frozen watermark");
        };
        tx.commit().await?;
        Ok(WorkflowBackfillClaim {
            watermark,
            owner_id: request.owner_id.clone(),
            token: request.token.clone(),
            lease_id: state.lease_id.unwrap_or_default(),
            generation,
            lease_expires_at_ms,
        })
    }

    /// Explicitly move an expired coordinator claim to recoverable.
    pub async fn reclaim_expired_backfill(&self, at_ms: i64) -> Result<bool> {
        validate_nonnegative_i64(at_ms, "backfill reclaim timestamp")?;
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        let changed = sqlx::query(
            "UPDATE workflow_backfill_state
             SET status = 'recoverable', owner_id = NULL, owner_token = NULL,
                 lease_id = NULL, lease_expires_at_ms = NULL,
                 updated_at_ms = ?, generation = generation + 1
             WHERE id = 1 AND status = 'processing'
               AND lease_expires_at_ms IS NOT NULL AND lease_expires_at_ms <= ?",
        )
        .bind(at_ms)
        .bind(at_ms)
        .execute(&mut *tx)
        .await?
        .rows_affected()
            == 1;
        tx.commit().await?;
        Ok(changed)
    }

    /// Register one rollout identity. Re-registering a renamed plain or zstd
    /// source updates only its current path/metadata and never duplicates the
    /// journal row.
    pub async fn register_backfill_rollout(
        &self,
        input: &WorkflowBackfillJournalCreate,
    ) -> Result<WorkflowBackfillJournalEntry> {
        validate_journal_create(input)?;
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        let existing = journal_by_rollout(&mut tx, &input.rollout_id).await?;
        if let Some(existing) = existing {
            if existing.status == WorkflowBackfillJournalStatus::Processing {
                tx.commit().await?;
                return Ok(existing);
            }
            let row = sqlx::query(sqlx::AssertSqlSafe(format!(
                "UPDATE workflow_backfill_journal
                 SET source_path = ?, source_size_bytes = ?, source_mtime_ms = ?,
                     updated_at_ms = ?
                 WHERE rollout_id = ?
                 RETURNING {JOURNAL_COLUMNS}"
            )))
            .bind(&input.source_path)
            .bind(input.source_size_bytes)
            .bind(input.source_mtime_ms)
            .bind(now_ms())
            .bind(&input.rollout_id)
            .fetch_one(&mut *tx)
            .await?;
            let entry = journal_from_row(&row)?;
            tx.commit().await?;
            return Ok(entry);
        }
        let row = sqlx::query(sqlx::AssertSqlSafe(format!(
            "INSERT INTO workflow_backfill_journal
             (rollout_id, source_path, byte_offset, rollout_ordinal, status,
              updated_at_ms, source_size_bytes, source_mtime_ms, generation)
             VALUES (?, ?, 0, 0, 'pending', ?, ?, ?, 0)
             RETURNING {JOURNAL_COLUMNS}"
        )))
        .bind(&input.rollout_id)
        .bind(&input.source_path)
        .bind(now_ms())
        .bind(input.source_size_bytes)
        .bind(input.source_mtime_ms)
        .fetch_one(&mut *tx)
        .await?;
        let entry = journal_from_row(&row)?;
        tx.commit().await?;
        Ok(entry)
    }

    /// Read one journal row by rollout identity, independent of source path.
    pub async fn get_backfill_journal(
        &self,
        rollout_id: &str,
    ) -> Result<Option<WorkflowBackfillJournalEntry>> {
        validate_rollout_id(rollout_id)?;
        let row = sqlx::query(sqlx::AssertSqlSafe(format!(
            "SELECT {JOURNAL_COLUMNS} FROM workflow_backfill_journal
             WHERE rollout_id = ?"
        )))
        .bind(rollout_id)
        .fetch_optional(self.pool.as_ref())
        .await?;
        row.as_ref().map(journal_from_row).transpose()
    }

    /// List all journal rows in rollout-identity order.
    pub async fn list_backfill_journal(&self) -> Result<Vec<WorkflowBackfillJournalEntry>> {
        let rows = sqlx::query(sqlx::AssertSqlSafe(format!(
            "SELECT {JOURNAL_COLUMNS} FROM workflow_backfill_journal
             ORDER BY rollout_id ASC"
        )))
        .fetch_all(self.pool.as_ref())
        .await?;
        rows.iter().map(journal_from_row).collect()
    }

    /// Claim one pending/recoverable journal row oldest by rollout identity.
    pub async fn claim_backfill_journal(
        &self,
        request: &WorkflowBackfillJournalClaimRequest,
    ) -> Result<Option<WorkflowBackfillJournalClaim>> {
        validate_journal_claim_request(request)?;
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        let Some(current) = journal_by_rollout(&mut tx, &request.rollout_id).await? else {
            tx.rollback().await?;
            return Err(anyhow::Error::new(WorkflowBackfillError::MissingJournal {
                rollout_id: request.rollout_id.clone(),
            }));
        };
        if current.status.is_terminal() || current.status == WorkflowBackfillJournalStatus::Failed {
            tx.commit().await?;
            return Ok(None);
        }
        if current.status == WorkflowBackfillJournalStatus::Processing {
            tx.rollback().await?;
            return Err(anyhow::Error::new(WorkflowBackfillError::Busy));
        }
        let generation = next_generation(current.generation)?;
        let token = Uuid::new_v4().to_string();
        let lease_id = Uuid::new_v4().to_string();
        let now = now_ms();
        let expires_at_ms = now
            .checked_add(request.lease_duration_ms)
            .ok_or_else(|| anyhow::anyhow!("backfill journal lease timestamp overflow"))?;
        let row = sqlx::query(sqlx::AssertSqlSafe(format!(
            "UPDATE workflow_backfill_journal
             SET status = 'processing', owner_id = ?, owner_token = ?, lease_id = ?,
                 lease_expires_at_ms = ?, updated_at_ms = ?, generation = ?
             WHERE rollout_id = ? AND generation = ?
               AND status IN ('pending', 'recoverable')
             RETURNING {JOURNAL_COLUMNS}"
        )))
        .bind(&request.owner_id)
        .bind(&token)
        .bind(&lease_id)
        .bind(expires_at_ms)
        .bind(now)
        .bind(generation)
        .bind(&request.rollout_id)
        .bind(current.generation)
        .fetch_optional(&mut *tx)
        .await?;
        let Some(row) = row else {
            tx.rollback().await?;
            return Err(anyhow::Error::new(WorkflowBackfillError::Stale));
        };
        let entry = journal_from_row(&row)?;
        tx.commit().await?;
        Ok(Some(WorkflowBackfillJournalClaim {
            entry,
            owner_id: request.owner_id.clone(),
            token,
            generation,
            lease_expires_at_ms: expires_at_ms,
        }))
    }

    /// Update journal position/source metadata under the exact claim fence.
    pub async fn update_backfill_journal(
        &self,
        request: &WorkflowBackfillJournalUpdate,
    ) -> Result<WorkflowBackfillJournalEntry> {
        validate_journal_update_request(request)?;
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        let now = now_ms();
        let current = journal_by_rollout(&mut tx, &request.rollout_id)
            .await?
            .ok_or_else(|| {
                anyhow::Error::new(WorkflowBackfillError::MissingJournal {
                    rollout_id: request.rollout_id.clone(),
                })
            })?;
        if current.status != WorkflowBackfillJournalStatus::Processing
            || current.owner_id.as_deref() != Some(request.owner_id.as_str())
            || current.owner_token.as_deref() != Some(request.token.as_str())
            || current.generation != request.generation
            || current
                .lease_expires_at_ms
                .is_none_or(|expires_at_ms| expires_at_ms <= now)
        {
            tx.rollback().await?;
            return Err(anyhow::Error::new(WorkflowBackfillError::Stale));
        }
        let new_generation = next_generation(current.generation)?;
        let (owner_id, owner_token, lease_id, lease_expires_at_ms) =
            if request.status == WorkflowBackfillJournalStatus::Processing {
                let expires = now
                    .checked_add(request.lease_duration_ms)
                    .ok_or_else(|| anyhow::anyhow!("backfill journal lease timestamp overflow"))?;
                (
                    Some(request.owner_id.as_str()),
                    Some(request.token.as_str()),
                    current.lease_id.as_deref(),
                    Some(expires),
                )
            } else {
                (None, None, None, None)
            };
        let row = sqlx::query(sqlx::AssertSqlSafe(format!(
            "UPDATE workflow_backfill_journal
             SET source_path = ?, byte_offset = ?, rollout_ordinal = ?, status = ?,
                 error_json = ?, updated_at_ms = ?, owner_id = ?, owner_token = ?,
                 lease_id = ?, lease_expires_at_ms = ?, generation_id = ?,
                 cursor_json = ?, source_size_bytes = ?, source_mtime_ms = ?,
                 generation = ?
             WHERE rollout_id = ? AND status = 'processing' AND owner_id = ?
               AND owner_token = ? AND generation = ?
             RETURNING {JOURNAL_COLUMNS}"
        )))
        .bind(&request.source_path)
        .bind(request.byte_offset)
        .bind(request.rollout_ordinal)
        .bind(request.status.as_str())
        .bind(request.error.as_deref())
        .bind(now)
        .bind(owner_id)
        .bind(owner_token)
        .bind(lease_id)
        .bind(lease_expires_at_ms)
        .bind(request.generation_id)
        .bind(request.cursor_json.as_deref())
        .bind(request.source_size_bytes)
        .bind(request.source_mtime_ms)
        .bind(new_generation)
        .bind(&request.rollout_id)
        .bind(&request.owner_id)
        .bind(&request.token)
        .bind(request.generation)
        .fetch_optional(&mut *tx)
        .await?;
        let Some(row) = row else {
            tx.rollback().await?;
            return Err(anyhow::Error::new(WorkflowBackfillError::Stale));
        };
        let entry = journal_from_row(&row)?;
        tx.commit().await?;
        Ok(entry)
    }

    /// Reclaim all expired journal claims explicitly, without scheduling work.
    pub async fn reclaim_expired_backfill_journal(
        &self,
        at_ms: i64,
    ) -> Result<Vec<WorkflowBackfillJournalEntry>> {
        validate_nonnegative_i64(at_ms, "backfill journal reclaim timestamp")?;
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        let rows = sqlx::query(sqlx::AssertSqlSafe(format!(
            "UPDATE workflow_backfill_journal
             SET status = 'recoverable', owner_id = NULL, owner_token = NULL,
                 lease_id = NULL, lease_expires_at_ms = NULL,
                 updated_at_ms = ?, generation = generation + 1
             WHERE status = 'processing' AND lease_expires_at_ms IS NOT NULL
               AND lease_expires_at_ms <= ?
             RETURNING {JOURNAL_COLUMNS}"
        )))
        .bind(at_ms)
        .bind(at_ms)
        .fetch_all(&mut *tx)
        .await?;
        let mut entries = rows
            .iter()
            .map(journal_from_row)
            .collect::<Result<Vec<_>>>()?;
        entries.sort_by(|left, right| left.rollout_id.cmp(&right.rollout_id));
        tx.commit().await?;
        Ok(entries)
    }

    /// Finalize only after all journal rows are complete or permanent skips.
    pub async fn finalize_backfill(
        &self,
        request: &WorkflowBackfillFinalizeRequest,
    ) -> Result<WorkflowBackfillState> {
        validate_finalize_request(request)?;
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        let current = load_state(&mut tx).await?;
        if current.status == WorkflowBackfillStatus::Complete {
            tx.commit().await?;
            return Ok(current);
        }
        if current.status != WorkflowBackfillStatus::Processing
            || current.owner_id.as_deref() != Some(request.owner_id.as_str())
            || current.owner_token.as_deref() != Some(request.token.as_str())
            || current.generation != request.generation
            || current
                .lease_expires_at_ms
                .is_none_or(|expires_at_ms| expires_at_ms <= now_ms())
        {
            tx.rollback().await?;
            return Err(anyhow::Error::new(WorkflowBackfillError::Stale));
        }
        let counts = blocking_journal_counts(&mut tx).await?;
        if counts.has_blocking_work() {
            tx.rollback().await?;
            return Err(anyhow::Error::new(WorkflowBackfillError::PendingWork {
                pending: counts.pending,
                processing: counts.processing,
                recoverable: counts.recoverable,
                failed: counts.failed,
            }));
        }
        let generation = next_generation(current.generation)?;
        let now = now_ms();
        let row = sqlx::query(sqlx::AssertSqlSafe(format!(
            "UPDATE workflow_backfill_state
             SET status = 'complete', last_success_at_ms = ?, updated_at_ms = ?,
                 owner_id = NULL, owner_token = NULL, lease_id = NULL,
                 lease_expires_at_ms = NULL, generation = ?
             WHERE id = 1 AND status = 'processing' AND owner_id = ?
               AND owner_token = ? AND generation = ?
             RETURNING {STATE_COLUMNS}"
        )))
        .bind(now)
        .bind(now)
        .bind(generation)
        .bind(&request.owner_id)
        .bind(&request.token)
        .bind(request.generation)
        .fetch_optional(&mut *tx)
        .await?;
        let Some(row) = row else {
            tx.rollback().await?;
            return Err(anyhow::Error::new(WorkflowBackfillError::Stale));
        };
        let state = state_from_row(&row)?;
        tx.commit().await?;
        Ok(state)
    }
}
