//! Experimental app-server contracts for durable, transient jobs.
//!
//! A job is a persisted, explicitly requested non-interactive run.  The
//! protocol deliberately exposes lifecycle metadata only: command output and
//! tool payloads remain in the canonical rollout/artifact stores and are not
//! copied into job responses.

use super::AskForApproval;
use super::SandboxMode;
use super::UserInput;
use crate::JsonSchema;
use crate::TS;
use codex_experimental_api_macros::ExperimentalApi;
use serde::Deserialize;
use serde::Serialize;
use serde_json::Value as JsonValue;
use std::collections::HashMap;
use std::path::PathBuf;

/// Classification used by the workflow projections and thread filters.
#[derive(
    Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, JsonSchema, TS, ExperimentalApi,
)]
#[serde(rename_all = "camelCase")]
#[ts(rename_all = "camelCase", export_to = "v2/")]
pub enum ThreadClass {
    /// A user-facing interactive conversation.
    Interactive,
    /// A child agent in a delegated agent tree.
    SubAgent,
    /// A durable, explicitly requested, non-interactive job.
    TransientJob,
    /// A run used internally by the runtime.
    Internal,
    /// A historical `exec` run whose original class is retained.
    LegacyExec,
}

/// Current lifecycle state of a durable job.
#[derive(
    Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, JsonSchema, TS, ExperimentalApi,
)]
#[serde(rename_all = "camelCase")]
#[ts(rename_all = "camelCase", export_to = "v2/")]
pub enum JobStatus {
    /// The run has been accepted but has not started execution.
    Pending,
    /// The run is currently executing.
    Running,
    /// The run finished successfully.
    Succeeded,
    /// The run finished with an execution error.
    Failed,
    /// The run cannot make progress without an explicit intervention.
    Blocked,
    /// The run's terminal state could not be determined.
    Inconclusive,
    /// The run was explicitly cancelled.
    Cancelled,
    /// The run was aborted before a normal terminal result was recorded.
    Aborted,
}

/// Terminal outcome recorded for a completed or stopped job.
#[derive(
    Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, JsonSchema, TS, ExperimentalApi,
)]
#[serde(rename_all = "camelCase")]
#[ts(rename_all = "camelCase", export_to = "v2/")]
pub enum TerminalOutcome {
    Succeeded,
    Failed,
    Blocked,
    Inconclusive,
    Cancelled,
    Aborted,
}

/// Durable lifecycle metadata for one transient job.
///
/// This type intentionally has no stdout, stderr, tool output, or model
/// response field. Those bytes remain available through the rollout/artifact
/// references maintained by the workflow implementation.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema, TS, ExperimentalApi)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct Job {
    /// For transient jobs this is also the run and hidden thread identity.
    pub id: String,
    /// Classification of the persisted workflow run. Jobs always use
    /// `transientJob`, but returning it makes cross-class projections explicit.
    pub thread_class: ThreadClass,
    /// Thread associated with this job's durable rollout.
    pub thread_id: String,
    /// Root thread of the agent tree, when the job belongs to one.
    pub root_thread_id: Option<String>,
    /// Parent run, when the job was created as a delegated child.
    pub parent_run_id: Option<String>,
    pub status: JobStatus,
    /// Terminal outcome, or `null` while the job is still running.
    pub outcome: Option<TerminalOutcome>,
    /// Client-supplied idempotency key, when one was supplied.
    pub idempotency_key: Option<String>,
    pub model_provider: Option<String>,
    pub model: Option<String>,
    pub cwd: Option<String>,
    /// Unix timestamp in seconds when the job was created.
    #[ts(type = "number")]
    pub created_at: i64,
    /// Unix timestamp in seconds when the job was last updated.
    #[ts(type = "number")]
    pub updated_at: i64,
    /// Unix timestamp in seconds when execution started.
    #[ts(type = "number | null")]
    pub started_at: Option<i64>,
    /// Unix timestamp in seconds when execution finished.
    #[ts(type = "number | null")]
    pub finished_at: Option<i64>,
    /// Optimistic-concurrency version of the workflow record.
    #[ts(type = "number")]
    pub version: i64,
}

/// Start one durable transient job.
#[derive(
    Serialize, Deserialize, Debug, Clone, Default, PartialEq, JsonSchema, TS, ExperimentalApi,
)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct JobRunParams {
    /// User input submitted to the job's first turn.
    pub input: Vec<UserInput>,
    /// Reusing this key with the same immutable parameters returns the
    /// existing job instead of starting another run.
    #[ts(optional = nullable)]
    pub idempotency_key: Option<String>,
    #[ts(optional = nullable)]
    pub model: Option<String>,
    /// Model provider to use for this run. Omission selects the configured
    /// provider. The wire name matches `thread/start`'s `modelProvider`.
    #[serde(alias = "provider")]
    #[ts(optional = nullable)]
    pub model_provider: Option<String>,
    #[ts(optional = nullable)]
    pub cwd: Option<PathBuf>,
    #[ts(optional = nullable)]
    pub approval_policy: Option<AskForApproval>,
    #[ts(optional = nullable)]
    pub sandbox: Option<SandboxMode>,
    #[ts(optional = nullable)]
    pub permissions: Option<String>,
    #[ts(optional = nullable)]
    pub config: Option<HashMap<String, JsonValue>>,
    /// Optional JSON Schema constraining the final assistant message.
    #[ts(optional = nullable)]
    pub output_schema: Option<JsonValue>,
}

/// Filter and paginate durable transient jobs.
#[derive(
    Serialize, Deserialize, Debug, Clone, Default, PartialEq, Eq, JsonSchema, TS, ExperimentalApi,
)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct JobListParams {
    /// Opaque keyset cursor returned by a previous call.
    #[ts(optional = nullable)]
    pub cursor: Option<String>,
    /// Optional page size. The server defaults to 50 and caps this at 200.
    #[ts(optional = nullable)]
    pub limit: Option<u32>,
    /// Optional current-state filter.
    #[ts(optional = nullable)]
    pub status: Option<JobStatus>,
    /// Optional terminal-outcome filter.
    #[ts(optional = nullable)]
    pub outcome: Option<TerminalOutcome>,
    /// Optional agent-tree root filter.
    #[ts(optional = nullable)]
    pub root_thread_id: Option<String>,
}

/// A page of durable transient jobs.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema, TS, ExperimentalApi)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct JobListResponse {
    pub data: Vec<Job>,
    pub next_cursor: Option<String>,
}

/// Read one durable transient job by its job identity.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema, TS, ExperimentalApi)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct JobReadParams {
    pub job_id: String,
}

/// Result of reading one durable transient job.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema, TS, ExperimentalApi)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct JobReadResponse {
    pub job: Job,
}

/// Result of accepting a durable transient job request.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema, TS, ExperimentalApi)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct JobRunResponse {
    pub job: Job,
}

/// Explicitly cancel one durable transient job.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema, TS, ExperimentalApi)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct JobCancelParams {
    pub job_id: String,
}

/// Result of an explicit job cancellation request.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema, TS, ExperimentalApi)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct JobCancelResponse {
    pub job: Job,
}
