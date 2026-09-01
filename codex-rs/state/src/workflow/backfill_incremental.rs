//! Explicit incremental-capture state kept separate from the frozen pass.

use anyhow::Result;

use super::WorkflowStore;
use super::backfill_helpers::*;
use super::backfill_types::*;
use super::types::now_ms;

impl WorkflowStore {
    /// Request an explicit incremental pass after a frozen historical
    /// watermark. This state is independent and remains pending until a
    /// caller claims and advances it.
    pub async fn request_incremental_backfill(
        &self,
        watermark: &WorkflowBackfillWatermark,
    ) -> Result<WorkflowBackfillIncrementalState> {
        validate_watermark(watermark)?;
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        let now = now_ms();
        let row = sqlx::query(sqlx::AssertSqlSafe(format!(
            "UPDATE workflow_backfill_incremental
             SET status = 'pending', watermark_created_at_ms = ?,
                 watermark_rollout_id = ?, owner_id = NULL, owner_token = NULL,
                 lease_id = NULL, updated_at_ms = ?, generation = generation + 1
             WHERE id = 1
             RETURNING {INCREMENTAL_COLUMNS}"
        )))
        .bind(watermark.created_at_ms)
        .bind(&watermark.rollout_id)
        .bind(now)
        .fetch_one(&mut *tx)
        .await?;
        let state = incremental_from_row(&row)?;
        tx.commit().await?;
        Ok(state)
    }

    /// Mark incremental capture pending explicitly; historical finalize never
    /// changes this independent state.
    pub async fn mark_incremental_backfill_pending(
        &self,
    ) -> Result<WorkflowBackfillIncrementalState> {
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        let now = now_ms();
        let row = sqlx::query(sqlx::AssertSqlSafe(format!(
            "UPDATE workflow_backfill_incremental
             SET status = 'pending', owner_id = NULL, owner_token = NULL,
                 lease_id = NULL, updated_at_ms = ?, generation = generation + 1
             WHERE id = 1
             RETURNING {INCREMENTAL_COLUMNS}"
        )))
        .bind(now)
        .fetch_one(&mut *tx)
        .await?;
        let state = incremental_from_row(&row)?;
        tx.commit().await?;
        Ok(state)
    }

    /// Read explicit incremental-capture state.
    pub async fn get_incremental_backfill_state(&self) -> Result<WorkflowBackfillIncrementalState> {
        let row = sqlx::query(sqlx::AssertSqlSafe(format!(
            "SELECT {INCREMENTAL_COLUMNS} FROM workflow_backfill_incremental WHERE id = 1"
        )))
        .fetch_one(self.pool.as_ref())
        .await?;
        incremental_from_row(&row)
    }
}
