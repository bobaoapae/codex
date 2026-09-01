//! Durable workflow-checkpoint operations.

use anyhow::Result;
use anyhow::bail;

use super::WorkflowStore;
use super::run_types::*;
use super::types::*;

impl WorkflowStore {
    /// Append a checkpoint and allocate the next sequence atomically.
    pub async fn append_checkpoint(
        &self,
        input: &WorkflowCheckpointCreate,
    ) -> Result<WorkflowCheckpoint> {
        validate_checkpoint_create(input)?;
        let payload_json = serde_json::to_string(&input.payload)?;
        let mut tx = self.pool.begin().await?;
        let run_exists =
            sqlx::query_scalar::<_, i64>("SELECT 1 FROM workflow_runs WHERE run_id = ?")
                .bind(&input.run_id)
                .fetch_optional(&mut *tx)
                .await?
                .is_some();
        if !run_exists {
            tx.rollback().await?;
            bail!("workflow run does not exist: {}", input.run_id);
        }
        let sequence = sqlx::query_scalar::<_, i64>(
            "SELECT COALESCE(MAX(sequence), -1) + 1 FROM workflow_checkpoints WHERE run_id = ?",
        )
        .bind(&input.run_id)
        .fetch_one(&mut *tx)
        .await?;
        validate_nonnegative_i64(sequence, "checkpoint sequence")?;
        let now_ms = now_ms();
        let row = sqlx::query(
            "INSERT INTO workflow_checkpoints
                (run_id, sequence, checkpoint_kind, rollout_ordinal,
                 rollout_byte_offset, payload_json, created_at_ms)
             VALUES (?, ?, ?, ?, ?, ?, ?)
             RETURNING run_id, sequence, checkpoint_kind, rollout_ordinal,
                       rollout_byte_offset, payload_json, created_at_ms",
        )
        .bind(&input.run_id)
        .bind(sequence)
        .bind(&input.checkpoint_kind)
        .bind(input.rollout_ordinal)
        .bind(input.rollout_byte_offset)
        .bind(payload_json)
        .bind(now_ms)
        .fetch_one(&mut *tx)
        .await?;
        tx.commit().await?;
        workflow_checkpoint_from_row(&row)
    }

    /// List checkpoints in sequence order, optionally after a sequence.
    pub async fn list_checkpoints(
        &self,
        run_id: &str,
        after_sequence: Option<i64>,
        limit: u32,
    ) -> Result<Vec<WorkflowCheckpoint>> {
        validate_text(run_id, MAX_ID_BYTES, "run id")?;
        validate_page_size(limit)?;
        if after_sequence.is_some_and(|sequence| sequence < -1) {
            bail!("checkpoint sequence must be at least -1");
        }
        let rows = if let Some(after_sequence) = after_sequence {
            sqlx::query(
                "SELECT run_id, sequence, checkpoint_kind, rollout_ordinal,
                        rollout_byte_offset, payload_json, created_at_ms
                 FROM workflow_checkpoints WHERE run_id = ? AND sequence > ?
                 ORDER BY sequence LIMIT ?",
            )
            .bind(run_id)
            .bind(after_sequence)
            .bind(i64::from(limit))
            .fetch_all(self.pool.as_ref())
            .await?
        } else {
            sqlx::query(
                "SELECT run_id, sequence, checkpoint_kind, rollout_ordinal,
                        rollout_byte_offset, payload_json, created_at_ms
                 FROM workflow_checkpoints WHERE run_id = ?
                 ORDER BY sequence LIMIT ?",
            )
            .bind(run_id)
            .bind(i64::from(limit))
            .fetch_all(self.pool.as_ref())
            .await?
        };
        rows.iter().map(workflow_checkpoint_from_row).collect()
    }
}
