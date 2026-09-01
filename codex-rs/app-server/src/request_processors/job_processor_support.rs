//! Shared conversions and terminal-state helpers for durable jobs.

use codex_app_server_protocol::JSONRPCErrorError;
use codex_app_server_protocol::Job;
use codex_app_server_protocol::JobRunParams;
use codex_app_server_protocol::JobStatus;
use codex_app_server_protocol::TerminalOutcome;
use codex_app_server_protocol::ThreadClass;
use codex_app_server_protocol::ThreadStartParams;
use codex_state::WorkflowRun;
use codex_state::WorkflowStore;
use codex_state::WorkflowThreadClass;
use serde_json::Map;
use serde_json::Value;
use sha2::Digest;
use sha2::Sha256;

use crate::error_code::internal_error;

pub(super) const JOB_IDEMPOTENCY_ROOT: &str = "__codex_transient_jobs__";

/// Returns bounded operational metadata for a job row.
///
/// The full canonical request is hashed before this projection so idempotency
/// still detects changes to input and config without persisting either value.
pub(super) fn immutable_job_metadata(params: &JobRunParams) -> Result<Value, serde_json::Error> {
    let canonical_params = canonicalize_json(serde_json::to_value(params)?);
    let params_digest = Sha256::digest(serde_json::to_vec(&canonical_params)?);
    let params_digest = params_digest
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    let input_char_count = params
        .input
        .iter()
        .map(codex_app_server_protocol::UserInput::text_char_count)
        .sum::<usize>();
    Ok(Value::Object(Map::from_iter([
        (
            "hasInput".to_string(),
            Value::Bool(!params.input.is_empty()),
        ),
        (
            "inputCharCount".to_string(),
            Value::from(input_char_count as u64),
        ),
        (
            "inputItemCount".to_string(),
            Value::from(params.input.len() as u64),
        ),
        (
            "paramsDigest".to_string(),
            Value::String(format!("sha256:{params_digest}")),
        ),
        (
            "requestedSource".to_string(),
            Value::String("appServer.jobRun".to_string()),
        ),
        (
            "requestedThreadClass".to_string(),
            Value::String("transientJob".to_string()),
        ),
    ])))
}

fn canonicalize_json(value: Value) -> Value {
    match value {
        Value::Array(values) => Value::Array(values.into_iter().map(canonicalize_json).collect()),
        Value::Object(values) => {
            let mut canonical = Map::new();
            let mut entries = values.into_iter().collect::<Vec<_>>();
            entries.sort_unstable_by(|left, right| left.0.cmp(&right.0));
            for (key, value) in entries {
                canonical.insert(key, canonicalize_json(value));
            }
            Value::Object(canonical)
        }
        value => value,
    }
}

pub(super) fn thread_start_params(params: &JobRunParams) -> ThreadStartParams {
    ThreadStartParams {
        model: params.model.clone(),
        model_provider: params.model_provider.clone(),
        cwd: params
            .cwd
            .as_ref()
            .map(|cwd| cwd.to_string_lossy().into_owned()),
        approval_policy: params.approval_policy,
        sandbox: params.sandbox,
        permissions: params.permissions.clone(),
        config: params.config.clone(),
        ephemeral: Some(false),
        thread_class: Some(ThreadClass::TransientJob),
        ..Default::default()
    }
}

pub(super) async fn terminalize(
    workflow: &WorkflowStore,
    run: &WorkflowRun,
    status: &str,
) -> Result<(), ()> {
    let Some(current) = workflow.get_run(&run.run_id).await.map_err(|_| ())? else {
        return Err(());
    };
    if is_terminal(&current.status) {
        return Ok(());
    }
    workflow
        .transition_run_cas(
            &current.run_id,
            current.version,
            &current.status,
            status,
            Some(status),
        )
        .await
        .map(|_| ())
        .map_err(|_| ())
}

pub(super) fn api_job(run: WorkflowRun) -> Result<Job, JSONRPCErrorError> {
    Ok(Job {
        id: run.run_id,
        thread_class: api_thread_class(run.thread_class),
        thread_id: run.thread_id,
        root_thread_id: (run.root_thread_id.as_deref() != Some(JOB_IDEMPOTENCY_ROOT))
            .then_some(run.root_thread_id)
            .flatten(),
        parent_run_id: run.parent_run_id,
        status: api_job_status(&run.status)?,
        outcome: run
            .outcome
            .as_deref()
            .map(api_terminal_outcome)
            .transpose()?,
        idempotency_key: run.idempotency_key,
        model_provider: run.provider,
        model: run.model,
        cwd: run.cwd,
        created_at: run.created_at_ms.div_euclid(1_000),
        updated_at: run.updated_at_ms.div_euclid(1_000),
        started_at: run.started_at_ms.map(|value| value.div_euclid(1_000)),
        finished_at: run.finished_at_ms.map(|value| value.div_euclid(1_000)),
        version: run.version,
    })
}

fn api_thread_class(class: WorkflowThreadClass) -> ThreadClass {
    match class {
        WorkflowThreadClass::Interactive => ThreadClass::Interactive,
        WorkflowThreadClass::SubAgent => ThreadClass::SubAgent,
        WorkflowThreadClass::TransientJob => ThreadClass::TransientJob,
        WorkflowThreadClass::Internal => ThreadClass::Internal,
        WorkflowThreadClass::LegacyExec => ThreadClass::LegacyExec,
    }
}

fn api_job_status(status: &str) -> Result<JobStatus, JSONRPCErrorError> {
    match status {
        "pending" => Ok(JobStatus::Pending),
        "running" => Ok(JobStatus::Running),
        "succeeded" => Ok(JobStatus::Succeeded),
        "failed" => Ok(JobStatus::Failed),
        "blocked" => Ok(JobStatus::Blocked),
        "inconclusive" => Ok(JobStatus::Inconclusive),
        "cancelled" => Ok(JobStatus::Cancelled),
        "aborted" => Ok(JobStatus::Aborted),
        _ => Err(internal_error(format!(
            "unknown workflow job status: {status}"
        ))),
    }
}

fn api_terminal_outcome(status: &str) -> Result<TerminalOutcome, JSONRPCErrorError> {
    match status {
        "succeeded" => Ok(TerminalOutcome::Succeeded),
        "failed" => Ok(TerminalOutcome::Failed),
        "blocked" => Ok(TerminalOutcome::Blocked),
        "inconclusive" => Ok(TerminalOutcome::Inconclusive),
        "cancelled" => Ok(TerminalOutcome::Cancelled),
        "aborted" => Ok(TerminalOutcome::Aborted),
        _ => Err(internal_error(format!(
            "unknown workflow terminal outcome: {status}"
        ))),
    }
}

pub(super) fn job_status_string(status: JobStatus) -> String {
    match status {
        JobStatus::Pending => "pending",
        JobStatus::Running => "running",
        JobStatus::Succeeded => "succeeded",
        JobStatus::Failed => "failed",
        JobStatus::Blocked => "blocked",
        JobStatus::Inconclusive => "inconclusive",
        JobStatus::Cancelled => "cancelled",
        JobStatus::Aborted => "aborted",
    }
    .to_string()
}

pub(super) fn terminal_outcome_string(outcome: TerminalOutcome) -> &'static str {
    match outcome {
        TerminalOutcome::Succeeded => "succeeded",
        TerminalOutcome::Failed => "failed",
        TerminalOutcome::Blocked => "blocked",
        TerminalOutcome::Inconclusive => "inconclusive",
        TerminalOutcome::Cancelled => "cancelled",
        TerminalOutcome::Aborted => "aborted",
    }
}

pub(super) fn is_terminal(status: &str) -> bool {
    matches!(
        status,
        "succeeded" | "failed" | "blocked" | "inconclusive" | "cancelled" | "aborted"
    )
}

pub(super) fn workflow_error(error: anyhow::Error) -> JSONRPCErrorError {
    internal_error(format!("workflow job operation failed: {error}"))
}

#[cfg(test)]
#[path = "job_processor_support_tests.rs"]
mod tests;
