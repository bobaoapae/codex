//! Private SQL, parsing, fencing, and validation helpers for backfill state.

use anyhow::Result;
use anyhow::bail;
use sqlx::Row;
use sqlx::Sqlite;

use super::backfill_types::*;
use super::types::*;

pub(super) const STATE_COLUMNS: &str = "id, status, watermark_created_at_ms,
    watermark_rollout_id, last_success_at_ms, updated_at_ms, error_json,
    owner_id, owner_token, lease_id, lease_expires_at_ms, generation,
    generation_id, cursor_json, source_size_bytes, source_mtime_ms";
pub(super) const JOURNAL_COLUMNS: &str = "journal_id, rollout_id, source_path,
    byte_offset, rollout_ordinal, status, error_json, updated_at_ms, owner_id,
    owner_token, lease_id, lease_expires_at_ms, generation, generation_id,
    cursor_json, source_size_bytes, source_mtime_ms";
pub(super) const INCREMENTAL_COLUMNS: &str = "id, status, watermark_created_at_ms,
    watermark_rollout_id, updated_at_ms, error_json, owner_id, owner_token,
    lease_id, generation";

#[derive(Debug, Clone, Copy, Default)]
pub(super) struct BlockingJournalCounts {
    pub(super) pending: u32,
    pub(super) processing: u32,
    pub(super) recoverable: u32,
    pub(super) failed: u32,
}

impl BlockingJournalCounts {
    pub(super) const fn has_blocking_work(self) -> bool {
        self.pending != 0 || self.processing != 0 || self.recoverable != 0 || self.failed != 0
    }
}

pub(super) async fn load_state(
    tx: &mut sqlx::Transaction<'_, Sqlite>,
) -> Result<WorkflowBackfillState> {
    let row = sqlx::query(sqlx::AssertSqlSafe(format!(
        "SELECT {STATE_COLUMNS} FROM workflow_backfill_state WHERE id = 1"
    )))
    .fetch_one(&mut **tx)
    .await?;
    state_from_row(&row)
}

pub(super) async fn journal_by_rollout(
    tx: &mut sqlx::Transaction<'_, Sqlite>,
    rollout_id: &str,
) -> Result<Option<WorkflowBackfillJournalEntry>> {
    let row = sqlx::query(sqlx::AssertSqlSafe(format!(
        "SELECT {JOURNAL_COLUMNS} FROM workflow_backfill_journal
         WHERE rollout_id = ?"
    )))
    .bind(rollout_id)
    .fetch_optional(&mut **tx)
    .await?;
    row.as_ref().map(journal_from_row).transpose()
}

pub(super) async fn blocking_journal_counts(
    tx: &mut sqlx::Transaction<'_, Sqlite>,
) -> Result<BlockingJournalCounts> {
    let rows = sqlx::query(
        "SELECT status, COUNT(*) AS count FROM workflow_backfill_journal GROUP BY status",
    )
    .fetch_all(&mut **tx)
    .await?;
    let mut counts = BlockingJournalCounts::default();
    for row in rows {
        let count = u32::try_from(row.try_get::<i64, _>("count")?)
            .map_err(|_| anyhow::anyhow!("backfill journal count exceeds u32"))?;
        match row.try_get::<String, _>("status")?.as_str() {
            "pending" => counts.pending = count,
            "processing" => counts.processing = count,
            "recoverable" => counts.recoverable = count,
            "failed" => counts.failed = count,
            "complete" | "skippedPermanent" => {}
            status => bail!("unknown backfill journal status: {status}"),
        }
    }
    Ok(counts)
}

pub(super) fn state_from_row(row: &sqlx::sqlite::SqliteRow) -> Result<WorkflowBackfillState> {
    let watermark_created_at_ms = row.try_get::<Option<i64>, _>("watermark_created_at_ms")?;
    let watermark_rollout_id = row.try_get::<Option<String>, _>("watermark_rollout_id")?;
    let watermark = match (watermark_created_at_ms, watermark_rollout_id) {
        (Some(created_at_ms), Some(rollout_id)) => {
            Some(WorkflowBackfillWatermark::new(created_at_ms, rollout_id)?)
        }
        (None, None) => None,
        _ => bail!("backfill watermark is incomplete"),
    };
    Ok(WorkflowBackfillState {
        status: WorkflowBackfillStatus::from_str(&row.try_get::<String, _>("status")?)?,
        watermark,
        last_success_at_ms: row.try_get("last_success_at_ms")?,
        updated_at_ms: row.try_get("updated_at_ms")?,
        error: row.try_get("error_json")?,
        owner_id: row.try_get("owner_id")?,
        owner_token: row.try_get("owner_token")?,
        lease_id: row.try_get("lease_id")?,
        lease_expires_at_ms: row.try_get("lease_expires_at_ms")?,
        generation: row.try_get("generation")?,
        generation_id: row.try_get("generation_id")?,
        cursor_json: row.try_get("cursor_json")?,
        source_size_bytes: row.try_get("source_size_bytes")?,
        source_mtime_ms: row.try_get("source_mtime_ms")?,
    })
}

pub(super) fn journal_from_row(
    row: &sqlx::sqlite::SqliteRow,
) -> Result<WorkflowBackfillJournalEntry> {
    Ok(WorkflowBackfillJournalEntry {
        journal_id: row.try_get("journal_id")?,
        rollout_id: row.try_get("rollout_id")?,
        source_path: row.try_get("source_path")?,
        byte_offset: row.try_get("byte_offset")?,
        rollout_ordinal: row.try_get("rollout_ordinal")?,
        status: WorkflowBackfillJournalStatus::from_str(&row.try_get::<String, _>("status")?)?,
        error: row.try_get("error_json")?,
        updated_at_ms: row.try_get("updated_at_ms")?,
        owner_id: row.try_get("owner_id")?,
        owner_token: row.try_get("owner_token")?,
        lease_id: row.try_get("lease_id")?,
        lease_expires_at_ms: row.try_get("lease_expires_at_ms")?,
        generation: row.try_get("generation")?,
        generation_id: row.try_get("generation_id")?,
        cursor_json: row.try_get("cursor_json")?,
        source_size_bytes: row.try_get("source_size_bytes")?,
        source_mtime_ms: row.try_get("source_mtime_ms")?,
    })
}

pub(super) fn incremental_from_row(
    row: &sqlx::sqlite::SqliteRow,
) -> Result<WorkflowBackfillIncrementalState> {
    let watermark_created_at_ms = row.try_get::<Option<i64>, _>("watermark_created_at_ms")?;
    let watermark_rollout_id = row.try_get::<Option<String>, _>("watermark_rollout_id")?;
    let watermark = match (watermark_created_at_ms, watermark_rollout_id) {
        (Some(created_at_ms), Some(rollout_id)) => {
            Some(WorkflowBackfillWatermark::new(created_at_ms, rollout_id)?)
        }
        (None, None) => None,
        _ => bail!("incremental backfill watermark is incomplete"),
    };
    Ok(WorkflowBackfillIncrementalState {
        status: WorkflowBackfillStatus::from_str(&row.try_get::<String, _>("status")?)?,
        watermark,
        updated_at_ms: row.try_get("updated_at_ms")?,
        error: row.try_get("error_json")?,
        owner_id: row.try_get("owner_id")?,
        owner_token: row.try_get("owner_token")?,
        lease_id: row.try_get("lease_id")?,
        generation: row.try_get("generation")?,
    })
}

pub(super) fn validate_begin_request(request: &WorkflowBackfillBeginRequest) -> Result<()> {
    validate_watermark(&request.watermark)?;
    validate_owner(&request.owner_id)?;
    validate_lease_duration(request.lease_duration_ms)
}

pub(super) fn validate_resume_request(request: &WorkflowBackfillResumeRequest) -> Result<()> {
    validate_owner(&request.owner_id)?;
    validate_token(&request.token)?;
    validate_nonnegative_i64(request.generation, "backfill generation")?;
    validate_lease_duration(request.lease_duration_ms)
}

pub(super) fn validate_rollout_id(rollout_id: &str) -> Result<()> {
    validate_text(rollout_id, MAX_ID_BYTES, "backfill rollout id")
}

pub(super) fn validate_journal_create(input: &WorkflowBackfillJournalCreate) -> Result<()> {
    validate_rollout_id(&input.rollout_id)?;
    validate_source_path(&input.source_path)?;
    validate_optional_nonnegative_i64(input.source_size_bytes, "backfill source size")?;
    validate_optional_nonnegative_i64(input.source_mtime_ms, "backfill source mtime")
}

pub(super) fn validate_journal_claim_request(
    request: &WorkflowBackfillJournalClaimRequest,
) -> Result<()> {
    validate_rollout_id(&request.rollout_id)?;
    validate_owner(&request.owner_id)?;
    validate_lease_duration(request.lease_duration_ms)
}

pub(super) fn validate_journal_update_request(
    request: &WorkflowBackfillJournalUpdate,
) -> Result<()> {
    validate_rollout_id(&request.rollout_id)?;
    validate_owner(&request.owner_id)?;
    validate_token(&request.token)?;
    validate_nonnegative_i64(request.generation, "backfill journal generation")?;
    validate_source_path(&request.source_path)?;
    validate_nonnegative_i64(request.byte_offset, "backfill byte offset")?;
    validate_nonnegative_i64(request.rollout_ordinal, "backfill rollout ordinal")?;
    validate_error(request.error.as_deref())?;
    validate_cursor(request.cursor_json.as_deref())?;
    validate_optional_nonnegative_i64(request.source_size_bytes, "backfill source size")?;
    validate_optional_nonnegative_i64(request.source_mtime_ms, "backfill source mtime")?;
    validate_lease_duration(request.lease_duration_ms)
}

pub(super) fn validate_finalize_request(request: &WorkflowBackfillFinalizeRequest) -> Result<()> {
    validate_owner(&request.owner_id)?;
    validate_token(&request.token)?;
    validate_nonnegative_i64(request.generation, "backfill generation")
}

pub(super) fn next_generation(current: i64) -> Result<i64> {
    current
        .checked_add(1)
        .ok_or_else(|| anyhow::anyhow!("backfill generation overflow"))
}
