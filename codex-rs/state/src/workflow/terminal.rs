//! Durable runtime snapshots for interactive terminal processes.
//!
//! Rollouts remain the canonical conversation history.  This table is a
//! bounded coordination projection: it may be updated by runtime activity and
//! is safe to discard when a process is reaped or a session shuts down.

use anyhow::Result;
use anyhow::bail;
use serde::Deserialize;
use serde::Serialize;
use sqlx::Row;

use super::WorkflowStore;
use super::types::*;

const MAX_TERMINAL_COMMAND_BYTES: usize = 1_024;
const MAX_TERMINAL_PREVIEW_BYTES: usize = 512;

/// Coordinator-safe lifecycle states for a runtime terminal observation.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum WorkflowTerminalProcessState {
    Running,
    Waiting,
    NeedsAttention,
    Exited,
    Failed,
    Cancelled,
}

impl WorkflowTerminalProcessState {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Running => "running",
            Self::Waiting => "waiting",
            Self::NeedsAttention => "needsAttention",
            Self::Exited => "exited",
            Self::Failed => "failed",
            Self::Cancelled => "cancelled",
        }
    }

    fn from_str(value: &str) -> Result<Self> {
        match value {
            "running" => Ok(Self::Running),
            "waiting" => Ok(Self::Waiting),
            "needsAttention" => Ok(Self::NeedsAttention),
            "exited" => Ok(Self::Exited),
            "failed" => Ok(Self::Failed),
            "cancelled" => Ok(Self::Cancelled),
            _ => bail!("unknown terminal process state: {value}"),
        }
    }
}

/// One bounded terminal snapshot stored outside rollout/model history.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WorkflowTerminalObservation {
    pub session_id: String,
    pub process_id: i64,
    pub command_summary: String,
    pub started_at_ms: i64,
    pub elapsed_ms: i64,
    pub last_activity_at_ms: i64,
    pub last_output_at_ms: Option<i64>,
    pub last_output_preview: Option<String>,
    pub last_output_bytes: i64,
    pub output_bytes: i64,
    pub state: WorkflowTerminalProcessState,
    pub final_receipt_emitted: bool,
    pub updated_at_ms: i64,
}

impl WorkflowTerminalObservation {
    pub fn validate(&self) -> Result<()> {
        validate_text(&self.session_id, MAX_ID_BYTES, "terminal session id")?;
        if self.process_id <= 0 {
            bail!("terminal process id must be positive");
        }
        if self.command_summary.len() > MAX_TERMINAL_COMMAND_BYTES {
            bail!("terminal command summary exceeds {MAX_TERMINAL_COMMAND_BYTES} bytes");
        }
        if self
            .last_output_preview
            .as_ref()
            .is_some_and(|preview| preview.len() > MAX_TERMINAL_PREVIEW_BYTES)
        {
            bail!("terminal output preview exceeds {MAX_TERMINAL_PREVIEW_BYTES} bytes");
        }
        for (value, name) in [
            (self.started_at_ms, "terminal started timestamp"),
            (self.elapsed_ms, "terminal elapsed duration"),
            (self.last_activity_at_ms, "terminal activity timestamp"),
            (self.last_output_bytes, "terminal output byte count"),
            (self.output_bytes, "terminal total output byte count"),
            (self.updated_at_ms, "terminal updated timestamp"),
        ] {
            validate_nonnegative_i64(value, name)?;
        }
        validate_optional_nonnegative_i64(self.last_output_at_ms, "terminal output timestamp")?;
        Ok(())
    }
}

const TERMINAL_COLUMNS: &str = "session_id, process_id, command_summary,
    started_at_ms, elapsed_ms, last_activity_at_ms, last_output_at_ms,
    last_output_preview, last_output_bytes, output_bytes, state,
    final_receipt_emitted, updated_at_ms";

impl WorkflowStore {
    /// Upsert a bounded runtime observation.  Older asynchronous heartbeat
    /// writes cannot overwrite a newer observation because the timestamp is a
    /// monotonic write fence for one process row.
    pub async fn upsert_terminal_observation(
        &self,
        observation: &WorkflowTerminalObservation,
    ) -> Result<()> {
        observation.validate()?;
        sqlx::query(
            "INSERT INTO workflow_terminal_observations
                (session_id, process_id, command_summary, started_at_ms, elapsed_ms,
                 last_activity_at_ms, last_output_at_ms, last_output_preview,
                 last_output_bytes, output_bytes, state, final_receipt_emitted, updated_at_ms)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
             ON CONFLICT(session_id, process_id) DO UPDATE SET
                 command_summary = excluded.command_summary,
                 started_at_ms = excluded.started_at_ms,
                 elapsed_ms = excluded.elapsed_ms,
                 last_activity_at_ms = excluded.last_activity_at_ms,
                 last_output_at_ms = excluded.last_output_at_ms,
                 last_output_preview = excluded.last_output_preview,
                 last_output_bytes = excluded.last_output_bytes,
                 output_bytes = excluded.output_bytes,
                 state = excluded.state,
                 final_receipt_emitted = excluded.final_receipt_emitted,
                 updated_at_ms = excluded.updated_at_ms
             WHERE excluded.updated_at_ms >= workflow_terminal_observations.updated_at_ms",
        )
        .bind(&observation.session_id)
        .bind(observation.process_id)
        .bind(&observation.command_summary)
        .bind(observation.started_at_ms)
        .bind(observation.elapsed_ms)
        .bind(observation.last_activity_at_ms)
        .bind(observation.last_output_at_ms)
        .bind(&observation.last_output_preview)
        .bind(observation.last_output_bytes)
        .bind(observation.output_bytes)
        .bind(observation.state.as_str())
        .bind(observation.final_receipt_emitted)
        .bind(observation.updated_at_ms)
        .execute(self.pool.as_ref())
        .await?;
        Ok(())
    }

    /// Read one runtime terminal observation.
    pub async fn get_terminal_observation(
        &self,
        session_id: &str,
        process_id: i64,
    ) -> Result<Option<WorkflowTerminalObservation>> {
        validate_text(session_id, MAX_ID_BYTES, "terminal session id")?;
        if process_id <= 0 {
            bail!("terminal process id must be positive");
        }
        let row = sqlx::query(sqlx::AssertSqlSafe(format!(
            "SELECT {TERMINAL_COLUMNS} FROM workflow_terminal_observations
             WHERE session_id = ? AND process_id = ?"
        )))
        .bind(session_id)
        .bind(process_id)
        .fetch_optional(self.pool.as_ref())
        .await?;
        row.map(|row| terminal_observation_from_row(&row))
            .transpose()
    }

    /// List runtime observations for one session in deterministic PID order.
    pub async fn list_terminal_observations(
        &self,
        session_id: &str,
    ) -> Result<Vec<WorkflowTerminalObservation>> {
        validate_text(session_id, MAX_ID_BYTES, "terminal session id")?;
        let rows = sqlx::query(sqlx::AssertSqlSafe(format!(
            "SELECT {TERMINAL_COLUMNS} FROM workflow_terminal_observations
             WHERE session_id = ? ORDER BY process_id ASC"
        )))
        .bind(session_id)
        .fetch_all(self.pool.as_ref())
        .await?;
        rows.iter().map(terminal_observation_from_row).collect()
    }

    /// Remove one observation after its process handle has been reaped.
    pub async fn delete_terminal_observation(
        &self,
        session_id: &str,
        process_id: i64,
    ) -> Result<bool> {
        validate_text(session_id, MAX_ID_BYTES, "terminal session id")?;
        if process_id <= 0 {
            bail!("terminal process id must be positive");
        }
        let result = sqlx::query(
            "DELETE FROM workflow_terminal_observations
             WHERE session_id = ? AND process_id = ?",
        )
        .bind(session_id)
        .bind(process_id)
        .execute(self.pool.as_ref())
        .await?;
        Ok(result.rows_affected() == 1)
    }

    /// Remove all observations owned by a closing session.
    pub async fn delete_terminal_observations_for_session(&self, session_id: &str) -> Result<u64> {
        validate_text(session_id, MAX_ID_BYTES, "terminal session id")?;
        let result = sqlx::query("DELETE FROM workflow_terminal_observations WHERE session_id = ?")
            .bind(session_id)
            .execute(self.pool.as_ref())
            .await?;
        Ok(result.rows_affected())
    }
}

fn terminal_observation_from_row(
    row: &sqlx::sqlite::SqliteRow,
) -> Result<WorkflowTerminalObservation> {
    let state = WorkflowTerminalProcessState::from_str(row.try_get("state")?)?;
    let observation = WorkflowTerminalObservation {
        session_id: row.try_get("session_id")?,
        process_id: row.try_get("process_id")?,
        command_summary: row.try_get("command_summary")?,
        started_at_ms: row.try_get("started_at_ms")?,
        elapsed_ms: row.try_get("elapsed_ms")?,
        last_activity_at_ms: row.try_get("last_activity_at_ms")?,
        last_output_at_ms: row.try_get("last_output_at_ms")?,
        last_output_preview: row.try_get("last_output_preview")?,
        last_output_bytes: row.try_get("last_output_bytes")?,
        output_bytes: row.try_get("output_bytes")?,
        state,
        final_receipt_emitted: row.try_get::<i64, _>("final_receipt_emitted")? != 0,
        updated_at_ms: row.try_get("updated_at_ms")?,
    };
    observation.validate()?;
    Ok(observation)
}
