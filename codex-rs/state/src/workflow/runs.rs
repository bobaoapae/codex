//! Durable workflow-run operations.

use super::WorkflowStore;
use super::run_types::*;
use super::types::*;
use anyhow::Result;
use anyhow::bail;

const RUN_COLUMNS: &str = "run_id, thread_id, root_thread_id, parent_run_id, thread_class, status,
    outcome, idempotency_key, provider, model, cwd, metadata_json, created_at_ms,
    updated_at_ms, started_at_ms, finished_at_ms, version";

impl WorkflowStore {
    /// Create a run, returning the existing row when the same idempotency key
    /// (or run ID) is submitted with identical immutable fields.
    ///
    /// For transient jobs, `run_id` is also the thread and job identity. A
    /// replay with a new client-generated ID still returns the original row
    /// when its root and immutable parameter digest match.
    pub async fn create_run(&self, input: &WorkflowRunCreate) -> Result<WorkflowRun> {
        validate_run_create(input)?;
        // Compute the digest before touching SQLite. This both validates the
        // bounded immutable request and keeps the comparison contract local.
        input.immutable_params_digest()?;
        let metadata_json = serialize_optional_json(input.metadata.as_ref(), "run metadata")?;
        let now_ms = now_ms();
        sqlx::query(
            "INSERT INTO workflow_runs
                (run_id, thread_id, root_thread_id, parent_run_id, thread_class, status,
                 idempotency_key, provider, model, cwd, metadata_json, created_at_ms,
                 updated_at_ms)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
             ON CONFLICT DO NOTHING",
        )
        .bind(&input.run_id)
        .bind(&input.thread_id)
        .bind(&input.root_thread_id)
        .bind(&input.parent_run_id)
        .bind(input.thread_class.as_str())
        .bind(&input.status)
        .bind(&input.idempotency_key)
        .bind(&input.provider)
        .bind(&input.model)
        .bind(&input.cwd)
        .bind(metadata_json)
        .bind(now_ms)
        .bind(now_ms)
        .execute(self.pool.as_ref())
        .await?;

        let run_by_id = self.get_run(&input.run_id).await?;
        let run_by_key = match (
            input.root_thread_id.as_deref(),
            input.idempotency_key.as_deref(),
        ) {
            (Some(root_thread_id), Some(idempotency_key)) => {
                self.get_run_by_idempotency_key(root_thread_id, idempotency_key)
                    .await?
            }
            _ => None,
        };

        if let (Some(run_by_id), Some(run_by_key)) = (&run_by_id, &run_by_key)
            && run_by_id.run_id != run_by_key.run_id
        {
            bail!("workflow run id and idempotency key refer to different runs");
        }
        let Some(run) = run_by_id.or(run_by_key) else {
            bail!("workflow run was not created and no conflicting row was found");
        };

        // A run-ID collision is a conflict even if the caller happens to use
        // the same parameter digest. The idempotency-key path may, however,
        // intentionally return an existing run with a different generated ID.
        if run.run_id == input.run_id
            && (run.thread_id != input.thread_id || run.idempotency_key != input.idempotency_key)
        {
            bail!("workflow run identity already exists with different fields");
        }
        if !run_matches_create(&run, input) {
            bail!("workflow run or idempotency key already exists with different parameters");
        }
        Ok(run)
    }

    /// Explicitly named alias for the durable run creation operation.
    pub async fn create_workflow_run(&self, input: &WorkflowRunCreate) -> Result<WorkflowRun> {
        self.create_run(input).await
    }

    /// Return one workflow run by ID.
    pub async fn get_run(&self, run_id: &str) -> Result<Option<WorkflowRun>> {
        validate_text(run_id, MAX_ID_BYTES, "run id")?;
        let row = sqlx::query(sqlx::AssertSqlSafe(format!(
            "SELECT {RUN_COLUMNS} FROM workflow_runs WHERE run_id = ?"
        )))
        .bind(run_id)
        .fetch_optional(self.pool.as_ref())
        .await?;
        row.as_ref().map(workflow_run_from_row).transpose()
    }

    /// Explicitly named alias for reading a durable workflow run.
    pub async fn get_workflow_run(&self, run_id: &str) -> Result<Option<WorkflowRun>> {
        self.get_run(run_id).await
    }

    /// Return a run by its root-scoped idempotency key.
    pub async fn get_run_by_idempotency_key(
        &self,
        root_thread_id: &str,
        idempotency_key: &str,
    ) -> Result<Option<WorkflowRun>> {
        validate_text(root_thread_id, MAX_ID_BYTES, "root thread id")?;
        validate_text(
            idempotency_key,
            MAX_IDEMPOTENCY_KEY_BYTES,
            "idempotency key",
        )?;
        let row = sqlx::query(sqlx::AssertSqlSafe(format!(
            "SELECT {RUN_COLUMNS} FROM workflow_runs
             WHERE root_thread_id = ? AND idempotency_key = ?"
        )))
        .bind(root_thread_id)
        .bind(idempotency_key)
        .fetch_optional(self.pool.as_ref())
        .await?;
        row.as_ref().map(workflow_run_from_row).transpose()
    }

    /// List workflow runs in descending `(created_at_ms, run_id)` order.
    ///
    /// The cursor is a keyset anchor, not an offset. Consequently concurrent
    /// inserts before the anchor do not duplicate already-returned rows.
    pub async fn list_runs(&self, request: &WorkflowRunListRequest) -> Result<WorkflowRunPage> {
        request.validate()?;
        let mut statement = format!("SELECT {RUN_COLUMNS} FROM workflow_runs WHERE 1 = 1");
        if request.filter.thread_class.is_some() {
            statement.push_str(" AND thread_class = ?");
        }
        if request.filter.status.is_some() {
            statement.push_str(" AND status = ?");
        }
        if request.filter.root_thread_id.is_some() {
            statement.push_str(" AND root_thread_id = ?");
        }
        if request.cursor.is_some() {
            statement.push_str(" AND (created_at_ms < ? OR (created_at_ms = ? AND run_id < ?))");
        }
        statement.push_str(" ORDER BY created_at_ms DESC, run_id DESC LIMIT ?");

        let mut query = sqlx::query(sqlx::AssertSqlSafe(statement));
        if let Some(thread_class) = request.filter.thread_class {
            query = query.bind(thread_class.as_str());
        }
        if let Some(status) = request.filter.status.as_deref() {
            query = query.bind(status);
        }
        if let Some(root_thread_id) = request.filter.root_thread_id.as_deref() {
            query = query.bind(root_thread_id);
        }
        if let Some(cursor) = &request.cursor {
            query = query
                .bind(cursor.created_at_ms)
                .bind(cursor.created_at_ms)
                .bind(&cursor.run_id);
        }
        query = query.bind(i64::from(request.limit) + 1);
        let mut rows = query.fetch_all(self.pool.as_ref()).await?;
        let has_more = rows.len() > request.limit as usize;
        if has_more {
            rows.truncate(request.limit as usize);
        }
        let runs = rows
            .iter()
            .map(workflow_run_from_row)
            .collect::<Result<Vec<_>>>()?;
        let next_cursor = has_more
            .then(|| runs.last())
            .flatten()
            .map(|run| WorkflowRunCursor {
                created_at_ms: run.created_at_ms,
                run_id: run.run_id.clone(),
            });
        Ok(WorkflowRunPage { runs, next_cursor })
    }

    /// Alias emphasizing that this is the durable workflow-run listing.
    pub async fn list_workflow_runs(
        &self,
        request: &WorkflowRunListRequest,
    ) -> Result<WorkflowRunPage> {
        self.list_runs(request).await
    }

    /// Read all runs associated with one thread, newest first.
    pub async fn get_runs_by_thread_id(&self, thread_id: &str) -> Result<Vec<WorkflowRun>> {
        validate_text(thread_id, MAX_ID_BYTES, "thread id")?;
        let rows = sqlx::query(sqlx::AssertSqlSafe(format!(
            "SELECT {RUN_COLUMNS} FROM workflow_runs
             WHERE thread_id = ? ORDER BY created_at_ms DESC, run_id DESC"
        )))
        .bind(thread_id)
        .fetch_all(self.pool.as_ref())
        .await?;
        rows.iter().map(workflow_run_from_row).collect()
    }

    /// Read runs for a bounded batch of thread IDs in one SQLite query.
    pub async fn get_runs_by_thread_ids(&self, thread_ids: &[String]) -> Result<Vec<WorkflowRun>> {
        if thread_ids.len() > MAX_BATCH_IDS {
            bail!("thread ID batch exceeds {MAX_BATCH_IDS} entries");
        }
        if thread_ids.is_empty() {
            return Ok(Vec::new());
        }
        for thread_id in thread_ids {
            validate_text(thread_id, MAX_ID_BYTES, "thread id")?;
        }
        let placeholders = (0..thread_ids.len())
            .map(|_| "?")
            .collect::<Vec<_>>()
            .join(", ");
        let mut query = sqlx::query(sqlx::AssertSqlSafe(format!(
            "SELECT {RUN_COLUMNS} FROM workflow_runs
             WHERE thread_id IN ({placeholders})
             ORDER BY created_at_ms DESC, run_id DESC"
        )));
        for thread_id in thread_ids {
            query = query.bind(thread_id);
        }
        let rows = query.fetch_all(self.pool.as_ref()).await?;
        rows.iter().map(workflow_run_from_row).collect()
    }

    /// Alias used by callers that describe the operation as a batch read.
    pub async fn read_runs_by_thread_ids(&self, thread_ids: &[String]) -> Result<Vec<WorkflowRun>> {
        self.get_runs_by_thread_ids(thread_ids).await
    }

    /// Transition a run using its current version as a compare-and-swap token.
    ///
    /// The boolean compatibility result is `true` for both a newly applied
    /// transition and an already-applied identical terminal transition.
    pub async fn transition_run_cas(
        &self,
        run_id: &str,
        expected_version: i64,
        expected_status: &str,
        new_status: &str,
        outcome: Option<&str>,
    ) -> Result<bool> {
        Ok(matches!(
            self.transition_run_cas_outcome(
                run_id,
                expected_version,
                expected_status,
                new_status,
                outcome,
            )
            .await?,
            WorkflowRunTransitionOutcome::Applied | WorkflowRunTransitionOutcome::AlreadyApplied
        ))
    }

    /// Return the typed result of a versioned run transition.
    pub async fn transition_run_cas_outcome(
        &self,
        run_id: &str,
        expected_version: i64,
        expected_status: &str,
        new_status: &str,
        outcome: Option<&str>,
    ) -> Result<WorkflowRunTransitionOutcome> {
        validate_text(run_id, MAX_ID_BYTES, "run id")?;
        validate_nonempty_bounded(expected_status, MAX_STATUS_BYTES, "expected status")?;
        validate_nonempty_bounded(new_status, MAX_STATUS_BYTES, "new status")?;
        validate_optional_text(outcome, MAX_STATUS_BYTES, "outcome")?;
        if expected_version < 0 {
            bail!("expected version must be non-negative");
        }
        let Some(current) = self.get_run(run_id).await? else {
            return Ok(WorkflowRunTransitionOutcome::Missing);
        };
        if is_terminal_status(&current.status) {
            return Ok(
                if current.status == new_status && current.outcome.as_deref() == outcome {
                    WorkflowRunTransitionOutcome::AlreadyApplied
                } else {
                    WorkflowRunTransitionOutcome::Stale
                },
            );
        }
        if current.status != expected_status || current.version != expected_version {
            return Ok(WorkflowRunTransitionOutcome::Stale);
        }

        let now_ms = now_ms();
        let started_at = (new_status == "running").then_some(now_ms);
        let finished_at = is_terminal_status(new_status).then_some(now_ms);
        let result = sqlx::query(
            "UPDATE workflow_runs
             SET status = ?, outcome = ?, updated_at_ms = ?,
                 started_at_ms = CASE WHEN ? IS NULL THEN started_at_ms ELSE COALESCE(started_at_ms, ?) END,
                 finished_at_ms = CASE WHEN ? IS NULL THEN finished_at_ms ELSE COALESCE(finished_at_ms, ?) END,
                 version = version + 1
             WHERE run_id = ? AND status = ? AND version = ?
               AND status NOT IN ('succeeded', 'failed', 'blocked', 'inconclusive', 'cancelled', 'aborted')",
        )
        .bind(new_status)
        .bind(outcome)
        .bind(now_ms)
        .bind(started_at)
        .bind(started_at)
        .bind(finished_at)
        .bind(finished_at)
        .bind(run_id)
        .bind(expected_status)
        .bind(expected_version)
        .execute(self.pool.as_ref())
        .await?;
        if result.rows_affected() == 1 {
            return Ok(WorkflowRunTransitionOutcome::Applied);
        }

        // Another writer may have committed the exact terminal state between
        // the read and our CAS update. Treat that race as an idempotent success.
        let Some(latest) = self.get_run(run_id).await? else {
            return Ok(WorkflowRunTransitionOutcome::Missing);
        };
        Ok(
            if is_terminal_status(&latest.status)
                && latest.status == new_status
                && latest.outcome.as_deref() == outcome
            {
                WorkflowRunTransitionOutcome::AlreadyApplied
            } else {
                WorkflowRunTransitionOutcome::Stale
            },
        )
    }

    /// Transition a run with a status-only compare-and-swap for callers that
    /// do not retain the version field.
    pub async fn transition_run_status_cas(
        &self,
        run_id: &str,
        expected_status: &str,
        new_status: &str,
        outcome: Option<&str>,
    ) -> Result<bool> {
        Ok(matches!(
            self.transition_run_status_cas_outcome(run_id, expected_status, new_status, outcome)
                .await?,
            WorkflowRunTransitionOutcome::Applied | WorkflowRunTransitionOutcome::AlreadyApplied
        ))
    }

    /// Return the typed result of a status-only transition.
    pub async fn transition_run_status_cas_outcome(
        &self,
        run_id: &str,
        expected_status: &str,
        new_status: &str,
        outcome: Option<&str>,
    ) -> Result<WorkflowRunTransitionOutcome> {
        validate_text(run_id, MAX_ID_BYTES, "run id")?;
        let Some(run) = self.get_run(run_id).await? else {
            return Ok(WorkflowRunTransitionOutcome::Missing);
        };
        self.transition_run_cas_outcome(run_id, run.version, expected_status, new_status, outcome)
            .await
    }

    /// Recover every pending/running run older than `stale_before_ms`.
    ///
    /// This operation only changes durable state to `inconclusive`; it never
    /// schedules a retry or invokes a provider.
    pub async fn recover_stale_runs(&self, stale_before_ms: i64) -> Result<Vec<WorkflowRun>> {
        validate_nonnegative_i64(stale_before_ms, "stale run cutoff")?;
        let now_ms = now_ms();
        let rows = sqlx::query(sqlx::AssertSqlSafe(format!(
            "UPDATE workflow_runs
             SET status = 'inconclusive', outcome = 'stale', updated_at_ms = ?,
                 finished_at_ms = COALESCE(finished_at_ms, ?), version = version + 1
             WHERE status IN ('pending', 'running') AND updated_at_ms < ?
             RETURNING {RUN_COLUMNS}"
        )))
        .bind(now_ms)
        .bind(now_ms)
        .bind(stale_before_ms)
        .fetch_all(self.pool.as_ref())
        .await?;
        let mut runs = rows
            .iter()
            .map(workflow_run_from_row)
            .collect::<Result<Vec<_>>>()?;
        runs.sort_by(|left, right| {
            right
                .created_at_ms
                .cmp(&left.created_at_ms)
                .then_with(|| right.run_id.cmp(&left.run_id))
        });
        Ok(runs)
    }

    /// Recover one stale run, returning its final row when recovery occurred
    /// or had already been applied.
    pub async fn recover_stale_run(
        &self,
        run_id: &str,
        stale_before_ms: i64,
    ) -> Result<Option<WorkflowRun>> {
        validate_text(run_id, MAX_ID_BYTES, "run id")?;
        validate_nonnegative_i64(stale_before_ms, "stale run cutoff")?;
        let Some(current) = self.get_run(run_id).await? else {
            return Ok(None);
        };
        if current.status == "inconclusive" && current.outcome.as_deref() == Some("stale") {
            return Ok(Some(current));
        }
        if !matches!(current.status.as_str(), "pending" | "running")
            || current.updated_at_ms >= stale_before_ms
        {
            return Ok(None);
        }
        let outcome = self
            .recover_stale_run_cas(run_id, current.version, stale_before_ms)
            .await?;
        if matches!(
            outcome,
            WorkflowRunTransitionOutcome::Applied | WorkflowRunTransitionOutcome::AlreadyApplied
        ) {
            return self.get_run(run_id).await;
        }
        Ok(None)
    }

    /// Recover one stale run with an optimistic generation/version check.
    pub async fn recover_stale_run_cas(
        &self,
        run_id: &str,
        expected_version: i64,
        stale_before_ms: i64,
    ) -> Result<WorkflowRunTransitionOutcome> {
        validate_text(run_id, MAX_ID_BYTES, "run id")?;
        validate_nonnegative_i64(stale_before_ms, "stale run cutoff")?;
        if expected_version < 0 {
            bail!("expected version must be non-negative");
        }
        let now_ms = now_ms();
        let result = sqlx::query(
            "UPDATE workflow_runs
             SET status = 'inconclusive', outcome = 'stale', updated_at_ms = ?,
                 finished_at_ms = COALESCE(finished_at_ms, ?), version = version + 1
             WHERE run_id = ? AND version = ? AND updated_at_ms < ?
               AND status IN ('pending', 'running')",
        )
        .bind(now_ms)
        .bind(now_ms)
        .bind(run_id)
        .bind(expected_version)
        .bind(stale_before_ms)
        .execute(self.pool.as_ref())
        .await?;
        if result.rows_affected() == 1 {
            return Ok(WorkflowRunTransitionOutcome::Applied);
        }
        let Some(current) = self.get_run(run_id).await? else {
            return Ok(WorkflowRunTransitionOutcome::Missing);
        };
        Ok(
            if current.status == "inconclusive" && current.outcome.as_deref() == Some("stale") {
                WorkflowRunTransitionOutcome::AlreadyApplied
            } else {
                WorkflowRunTransitionOutcome::Stale
            },
        )
    }
}
