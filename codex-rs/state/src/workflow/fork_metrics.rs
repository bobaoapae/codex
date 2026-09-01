//! SQLite projection for fork timing and provider cache accounting.

use anyhow::Result;
use anyhow::bail;
use sqlx::Row;

use super::WorkflowStore;
use super::fork_metrics_types::*;
use super::types::*;

const METRICS_COLUMNS: &str = "fork_id, spawn_call_id, parent_thread_id,
    child_thread_id, fork_turns_mode, fork_turns_count,
    spawn_requested_at_ms, child_created_at_ms, first_event_at_ms,
    first_new_response_at_ms, completed_at_ms, projected_fork_bytes,
    projected_fork_tokens, provider_input_tokens,
    provider_cached_input_tokens, provider_uncached_input_tokens,
    provider_cache_write_input_tokens, warning_emitted,
    warning_projected_tokens, warning_limit_tokens, updated_at_ms";

impl WorkflowStore {
    /// Insert a fork projection and its bounded context-size entries.
    pub async fn create_fork_metrics(
        &self,
        request: &WorkflowForkMetricsCreate,
    ) -> Result<WorkflowForkMetrics> {
        request.validate()?;
        let mut tx = self.pool.begin().await?;
        sqlx::query(
            "INSERT INTO workflow_fork_metrics
             (fork_id, spawn_call_id, parent_thread_id, fork_turns_mode,
              fork_turns_count, spawn_requested_at_ms, projected_fork_bytes,
              projected_fork_tokens, updated_at_ms)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)
             ON CONFLICT(fork_id) DO UPDATE SET
                 spawn_call_id = excluded.spawn_call_id,
                 parent_thread_id = excluded.parent_thread_id,
                 fork_turns_mode = excluded.fork_turns_mode,
                 fork_turns_count = excluded.fork_turns_count,
                 spawn_requested_at_ms = excluded.spawn_requested_at_ms,
                 projected_fork_bytes = excluded.projected_fork_bytes,
                 projected_fork_tokens = excluded.projected_fork_tokens,
                 updated_at_ms = MAX(workflow_fork_metrics.updated_at_ms,
                                     excluded.updated_at_ms)",
        )
        .bind(&request.fork_id)
        .bind(&request.spawn_call_id)
        .bind(&request.parent_thread_id)
        .bind(request.fork_turns.mode())
        .bind(request.fork_turns.count().map(i64::from))
        .bind(request.spawn_requested_at_ms)
        .bind(request.projected_fork_bytes)
        .bind(request.projected_fork_tokens)
        .bind(request.spawn_requested_at_ms)
        .execute(&mut *tx)
        .await?;
        for entry in &request.context_entries {
            entry.validate()?;
            sqlx::query(
                "INSERT INTO workflow_fork_context
                 (fork_id, sequence, origin, byte_count, token_count)
                 VALUES (?, ?, ?, ?, ?) ON CONFLICT(fork_id, sequence) DO NOTHING",
            )
            .bind(&entry.fork_id)
            .bind(entry.sequence)
            .bind(entry.origin.as_str())
            .bind(entry.byte_count)
            .bind(entry.token_count)
            .execute(&mut *tx)
            .await?;
        }
        let row = sqlx::query(sqlx::AssertSqlSafe(format!(
            "SELECT {METRICS_COLUMNS} FROM workflow_fork_metrics WHERE fork_id = ?"
        )))
        .bind(&request.fork_id)
        .fetch_one(&mut *tx)
        .await?;
        let metrics = metrics_from_row(&row)?;
        tx.commit().await?;
        Ok(metrics)
    }

    /// Update the child-created timestamp and bind the child identity once.
    pub async fn mark_fork_child_created(
        &self,
        fork_id: &str,
        child_thread_id: &str,
        at_ms: i64,
    ) -> Result<bool> {
        validate_fork_id(fork_id)?;
        validate_text(child_thread_id, MAX_ID_BYTES, "fork child thread id")?;
        validate_nonnegative_i64(at_ms, "fork child timestamp")?;
        let result = sqlx::query(
            "UPDATE workflow_fork_metrics
             SET child_thread_id = ?, child_created_at_ms = COALESCE(child_created_at_ms, ?),
                 updated_at_ms = MAX(updated_at_ms, ?)
             WHERE fork_id = ? AND child_thread_id IS NULL",
        )
        .bind(child_thread_id)
        .bind(at_ms)
        .bind(at_ms)
        .bind(fork_id)
        .execute(self.pool.as_ref())
        .await?;
        Ok(result.rows_affected() == 1)
    }

    /// Record the first observable child event, preserving first-write order.
    pub async fn mark_fork_first_event(&self, fork_id: &str, at_ms: i64) -> Result<bool> {
        self.mark_first_timestamp(fork_id, "first_event_at_ms", at_ms)
            .await
    }

    /// Record the first response that was produced after fork creation.
    pub async fn mark_fork_first_new_response(&self, fork_id: &str, at_ms: i64) -> Result<bool> {
        self.mark_first_timestamp(fork_id, "first_new_response_at_ms", at_ms)
            .await
    }

    /// Record terminal completion for a forked child.  The operation is
    /// idempotent so duplicate completion events cannot move the timestamp.
    pub async fn mark_fork_completed(&self, fork_id: &str, at_ms: i64) -> Result<bool> {
        self.mark_first_timestamp(fork_id, "completed_at_ms", at_ms)
            .await
    }

    /// Add provider usage observed after the inherited-history boundary.
    pub async fn add_fork_provider_usage(
        &self,
        fork_id: &str,
        input_tokens: i64,
        cached_input_tokens: i64,
        cache_write_input_tokens: i64,
        at_ms: i64,
    ) -> Result<bool> {
        validate_fork_id(fork_id)?;
        for (value, name) in [
            (input_tokens, "fork input tokens"),
            (cached_input_tokens, "fork cached input tokens"),
            (cache_write_input_tokens, "fork cache-write input tokens"),
            (at_ms, "fork usage timestamp"),
        ] {
            validate_nonnegative_i64(value, name)?;
        }
        let uncached = input_tokens
            .saturating_sub(cached_input_tokens)
            .saturating_sub(cache_write_input_tokens);
        let result = sqlx::query(
            "UPDATE workflow_fork_metrics
             SET provider_input_tokens = COALESCE(provider_input_tokens, 0) + ?,
                 provider_cached_input_tokens = COALESCE(provider_cached_input_tokens, 0) + ?,
                 provider_uncached_input_tokens = COALESCE(provider_uncached_input_tokens, 0) + ?,
                 provider_cache_write_input_tokens = COALESCE(provider_cache_write_input_tokens, 0) + ?,
                 updated_at_ms = MAX(updated_at_ms, ?)
             WHERE fork_id = ?",
        )
        .bind(input_tokens)
        .bind(cached_input_tokens)
        .bind(uncached)
        .bind(cache_write_input_tokens)
        .bind(at_ms)
        .bind(fork_id)
        .execute(self.pool.as_ref())
        .await?;
        Ok(result.rows_affected() == 1)
    }

    /// Atomically claim the near-compaction warning.  A claimed warning is
    /// emitted exactly once by the caller; no compaction is requested here.
    pub async fn claim_fork_compaction_warning(
        &self,
        fork_id: &str,
        projected_tokens: i64,
        limit_tokens: i64,
        at_ms: i64,
    ) -> Result<bool> {
        validate_fork_id(fork_id)?;
        validate_nonnegative_i64(projected_tokens, "fork warning projected tokens")?;
        if limit_tokens <= 0 {
            bail!("fork warning token limit must be positive");
        }
        validate_nonnegative_i64(at_ms, "fork warning timestamp")?;
        let result = sqlx::query(
            "UPDATE workflow_fork_metrics
             SET warning_emitted = 1, warning_projected_tokens = ?,
                 warning_limit_tokens = ?, updated_at_ms = MAX(updated_at_ms, ?)
             WHERE fork_id = ? AND warning_emitted = 0",
        )
        .bind(projected_tokens)
        .bind(limit_tokens)
        .bind(at_ms)
        .bind(fork_id)
        .execute(self.pool.as_ref())
        .await?;
        Ok(result.rows_affected() == 1)
    }

    /// Update projected history sizes after the fork selection has been
    /// normalized and filtered.
    pub async fn update_fork_projection(
        &self,
        fork_id: &str,
        projected_fork_bytes: i64,
        projected_fork_tokens: i64,
        context_entries: &[WorkflowForkContextEntry],
        at_ms: i64,
    ) -> Result<bool> {
        validate_fork_id(fork_id)?;
        validate_nonnegative_i64(projected_fork_bytes, "projected fork bytes")?;
        validate_nonnegative_i64(projected_fork_tokens, "projected fork tokens")?;
        validate_nonnegative_i64(at_ms, "fork projection timestamp")?;
        if context_entries.len() > MAX_FORK_CONTEXT_ENTRIES {
            bail!("fork context entries exceed {MAX_FORK_CONTEXT_ENTRIES}");
        }
        let result = sqlx::query(
            "UPDATE workflow_fork_metrics
             SET projected_fork_bytes = ?, projected_fork_tokens = ?,
                 updated_at_ms = MAX(updated_at_ms, ?)
             WHERE fork_id = ?",
        )
        .bind(projected_fork_bytes)
        .bind(projected_fork_tokens)
        .bind(at_ms)
        .bind(fork_id)
        .execute(self.pool.as_ref())
        .await?;
        if result.rows_affected() == 0 {
            return Ok(false);
        }
        for entry in context_entries {
            if entry.fork_id != fork_id {
                bail!("fork context entry has a different fork id");
            }
            insert_context_entry(self.pool.as_ref(), entry).await?;
        }
        Ok(true)
    }

    /// Append one bounded new-output provenance entry.  Once the cap is full
    /// the aggregate metrics continue to update but no more rows are stored.
    pub async fn append_fork_context_entry(
        &self,
        entry: &WorkflowForkContextEntry,
    ) -> Result<bool> {
        entry.validate()?;
        insert_context_entry(self.pool.as_ref(), entry).await
    }

    /// Read one fork projection for a future internal context/inspect API.
    pub async fn get_fork_metrics(&self, fork_id: &str) -> Result<Option<WorkflowForkMetrics>> {
        validate_fork_id(fork_id)?;
        let row = sqlx::query(sqlx::AssertSqlSafe(format!(
            "SELECT {METRICS_COLUMNS} FROM workflow_fork_metrics WHERE fork_id = ?"
        )))
        .bind(fork_id)
        .fetch_optional(self.pool.as_ref())
        .await?;
        row.as_ref().map(metrics_from_row).transpose()
    }

    /// List fork projections for a parent in causal spawn order.
    pub async fn list_fork_metrics(
        &self,
        parent_thread_id: &str,
    ) -> Result<Vec<WorkflowForkMetrics>> {
        validate_text(parent_thread_id, MAX_ID_BYTES, "fork parent thread id")?;
        let rows = sqlx::query(sqlx::AssertSqlSafe(format!(
            "SELECT {METRICS_COLUMNS} FROM workflow_fork_metrics
             WHERE parent_thread_id = ?
             ORDER BY spawn_requested_at_ms ASC, fork_id ASC"
        )))
        .bind(parent_thread_id)
        .fetch_all(self.pool.as_ref())
        .await?;
        rows.iter().map(metrics_from_row).collect()
    }

    /// Read size/provenance entries without exposing their content.
    pub async fn list_fork_context(&self, fork_id: &str) -> Result<Vec<WorkflowForkContextEntry>> {
        validate_fork_id(fork_id)?;
        let rows = sqlx::query(
            "SELECT fork_id, sequence, origin, byte_count, token_count
             FROM workflow_fork_context WHERE fork_id = ? ORDER BY sequence ASC",
        )
        .bind(fork_id)
        .fetch_all(self.pool.as_ref())
        .await?;
        rows.iter().map(context_from_row).collect()
    }
}

impl WorkflowStore {
    async fn mark_first_timestamp(&self, fork_id: &str, column: &str, at_ms: i64) -> Result<bool> {
        validate_fork_id(fork_id)?;
        validate_nonnegative_i64(at_ms, "fork event timestamp")?;
        let query = match column {
            "first_event_at_ms" => {
                "UPDATE workflow_fork_metrics SET first_event_at_ms = ?, updated_at_ms = MAX(updated_at_ms, ?) WHERE fork_id = ? AND first_event_at_ms IS NULL"
            }
            "first_new_response_at_ms" => {
                "UPDATE workflow_fork_metrics SET first_new_response_at_ms = ?, updated_at_ms = MAX(updated_at_ms, ?) WHERE fork_id = ? AND first_new_response_at_ms IS NULL"
            }
            "completed_at_ms" => {
                "UPDATE workflow_fork_metrics SET completed_at_ms = ?, updated_at_ms = MAX(updated_at_ms, ?) WHERE fork_id = ? AND completed_at_ms IS NULL"
            }
            _ => bail!("unknown fork timestamp column: {column}"),
        };
        let result = sqlx::query(query)
            .bind(at_ms)
            .bind(at_ms)
            .bind(fork_id)
            .execute(self.pool.as_ref())
            .await?;
        Ok(result.rows_affected() == 1)
    }
}

async fn insert_context_entry(
    pool: &sqlx::SqlitePool,
    entry: &WorkflowForkContextEntry,
) -> Result<bool> {
    entry.validate()?;
    let result = sqlx::query(
        "INSERT INTO workflow_fork_context
         (fork_id, sequence, origin, byte_count, token_count)
         SELECT ?, ?, ?, ?, ?
         WHERE (SELECT COUNT(*) FROM workflow_fork_context WHERE fork_id = ?) < ?
         ON CONFLICT(fork_id, sequence) DO NOTHING",
    )
    .bind(&entry.fork_id)
    .bind(entry.sequence)
    .bind(entry.origin.as_str())
    .bind(entry.byte_count)
    .bind(entry.token_count)
    .bind(&entry.fork_id)
    .bind(i64::try_from(MAX_FORK_CONTEXT_ENTRIES).unwrap_or(i64::MAX))
    .execute(pool)
    .await?;
    Ok(result.rows_affected() == 1)
}

fn metrics_from_row(row: &sqlx::sqlite::SqliteRow) -> Result<WorkflowForkMetrics> {
    let fork_turns = WorkflowForkTurns::from_parts(
        row.try_get("fork_turns_mode")?,
        row.try_get("fork_turns_count")?,
    )?;
    let metrics = WorkflowForkMetrics {
        fork_id: row.try_get("fork_id")?,
        spawn_call_id: row.try_get("spawn_call_id")?,
        parent_thread_id: row.try_get("parent_thread_id")?,
        child_thread_id: row.try_get("child_thread_id")?,
        fork_turns,
        spawn_requested_at_ms: row.try_get("spawn_requested_at_ms")?,
        child_created_at_ms: row.try_get("child_created_at_ms")?,
        first_event_at_ms: row.try_get("first_event_at_ms")?,
        first_new_response_at_ms: row.try_get("first_new_response_at_ms")?,
        completed_at_ms: row.try_get("completed_at_ms")?,
        projected_fork_bytes: row.try_get("projected_fork_bytes")?,
        projected_fork_tokens: row.try_get("projected_fork_tokens")?,
        provider_input_tokens: row.try_get("provider_input_tokens")?,
        provider_cached_input_tokens: row.try_get("provider_cached_input_tokens")?,
        provider_uncached_input_tokens: row.try_get("provider_uncached_input_tokens")?,
        provider_cache_write_input_tokens: row.try_get("provider_cache_write_input_tokens")?,
        warning_emitted: row.try_get::<i64, _>("warning_emitted")? != 0,
        warning_projected_tokens: row.try_get("warning_projected_tokens")?,
        warning_limit_tokens: row.try_get("warning_limit_tokens")?,
        updated_at_ms: row.try_get("updated_at_ms")?,
    };
    metrics.validate()?;
    Ok(metrics)
}

fn context_from_row(row: &sqlx::sqlite::SqliteRow) -> Result<WorkflowForkContextEntry> {
    let entry = WorkflowForkContextEntry {
        fork_id: row.try_get("fork_id")?,
        sequence: row.try_get("sequence")?,
        origin: WorkflowForkContextOrigin::from_str(row.try_get("origin")?)?,
        byte_count: row.try_get("byte_count")?,
        token_count: row.try_get("token_count")?,
    };
    entry.validate()?;
    Ok(entry)
}

fn validate_fork_id(fork_id: &str) -> Result<()> {
    validate_text(fork_id, 128, "fork id")
}
