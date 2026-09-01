//! Lifecycle enforcement for durable, non-interactive transient jobs.
//!
//! Transient jobs share the regular core thread runtime with interactive
//! threads.  Their durable classification therefore comes only from the
//! workflow store; thread source, model provider, and event shape are not
//! sufficient to identify a job.  This module keeps the workflow transition
//! and the non-interactive request policy in one small boundary so regular
//! threads retain their existing behavior.

use codex_core::CodexThread;
use codex_protocol::ThreadId;
use codex_protocol::approvals::ElicitationAction;
use codex_protocol::protocol::ApplyPatchApprovalRequestEvent;
use codex_protocol::protocol::CodexErrorInfo;
use codex_protocol::protocol::ErrorEvent;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::ExecApprovalRequestEvent;
use codex_protocol::protocol::ReviewDecision;
use codex_protocol::protocol::TurnCompleteEvent;
use codex_protocol::request_permissions::RequestPermissionsEvent;
use codex_protocol::request_permissions::RequestPermissionsResponse;
use codex_protocol::request_user_input::RequestUserInputResponse;
use codex_state::WorkflowRun;
use codex_state::WorkflowRunCursor;
use codex_state::WorkflowRunListFilter;
use codex_state::WorkflowRunListRequest;
use codex_state::WorkflowStore;
use codex_state::WorkflowThreadClass;
use std::collections::HashMap;
use std::collections::HashSet;
use std::sync::Arc;
use tracing::error;
use tracing::warn;

const STARTUP_RUN_PAGE_SIZE: u32 = 200;

/// Classification captured when a thread listener starts.
///
/// Keeping the classification with the listener avoids a workflow database
/// lookup for every streaming delta.  The lifecycle handler still reloads the
/// run row before each state transition so job-processor races are resolved by
/// the workflow CAS instead of a stale startup snapshot.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ThreadClassification {
    /// The workflow store contains no transient-job row for this thread.
    NotTransient,
    /// The workflow store explicitly classified this thread as a transient job.
    Transient { run_id: String },
}

/// Whether the caller should continue with the normal app-server event path.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum EventHandling {
    /// The thread is not a transient job, so existing behavior must run.
    Continue,
    /// The event was consumed by the transient-job policy and must not create
    /// a client-facing pending request.
    Suppress,
}

/// Classify one loaded thread from the durable workflow store.
pub(crate) async fn classify_thread(
    conversation: &Arc<CodexThread>,
    conversation_id: ThreadId,
) -> ThreadClassification {
    let Some(state_db) = conversation.state_db() else {
        return ThreadClassification::NotTransient;
    };
    let workflow = state_db.workflow_store().clone();
    let run = match transient_run(&workflow, conversation_id).await {
        Ok(run) => run,
        Err(error) => {
            warn!(
                thread_id = %conversation_id,
                "failed to classify transient job event from workflow store: {error}"
            );
            return ThreadClassification::NotTransient;
        }
    };
    let Some(run) = run else {
        return ThreadClassification::NotTransient;
    };
    ThreadClassification::Transient { run_id: run.run_id }
}

/// Mark transient jobs left by a previous process as inconclusive.
///
/// App-server startup happens before clients can load a thread, so pending or
/// running jobs whose thread is not present in `loaded_thread_ids` cannot be
/// resumed safely.  The operation is deliberately a bounded, paginated read
/// followed by status-only CAS transitions; it never invokes a provider or
/// schedules a retry.  A concurrent terminal transition wins the CAS race.
pub(crate) async fn recover_unloaded_transient_jobs(
    workflow: &WorkflowStore,
    loaded_thread_ids: &[ThreadId],
    startup_started_at_ms: i64,
) -> anyhow::Result<usize> {
    let loaded_thread_ids = loaded_thread_ids
        .iter()
        .map(ToString::to_string)
        .collect::<HashSet<_>>();
    let mut recovered = 0;
    for status in ["pending", "running"] {
        let filter = WorkflowRunListFilter {
            thread_class: Some(WorkflowThreadClass::TransientJob),
            status: Some(status.to_string()),
            root_thread_id: None,
        };
        let mut cursor: Option<WorkflowRunCursor> = None;
        loop {
            let request =
                WorkflowRunListRequest::new(filter.clone(), cursor, STARTUP_RUN_PAGE_SIZE)?;
            let page = workflow.list_runs(&request).await?;
            cursor = page.next_cursor;
            for run in page.runs {
                if run.updated_at_ms >= startup_started_at_ms
                    || loaded_thread_ids.contains(&run.thread_id)
                {
                    continue;
                }
                if workflow
                    .transition_run_status_cas(
                        &run.run_id,
                        &run.status,
                        "inconclusive",
                        Some("stale"),
                    )
                    .await?
                {
                    recovered += 1;
                }
            }
            if cursor.is_none() {
                break;
            }
        }
    }
    Ok(recovered)
}

pub(crate) async fn handle_classified_event(
    conversation: &Arc<CodexThread>,
    event_id: &str,
    event: &EventMsg,
    classification: &ThreadClassification,
) -> EventHandling {
    let ThreadClassification::Transient { run_id } = classification else {
        return EventHandling::Continue;
    };
    let Some(state_db) = conversation.state_db() else {
        return EventHandling::Continue;
    };
    let workflow = state_db.workflow_store().clone();

    match event {
        EventMsg::TurnStarted(_) => {
            transition_turn_started(&workflow, run_id).await;
        }
        EventMsg::TurnComplete(payload) => {
            let terminal_status = terminal_status_for_turn_complete(payload);
            transition_terminal(&workflow, run_id, terminal_status).await;
        }
        EventMsg::TurnAborted(_payload) => {
            let terminal_status =
                terminal_status_for_turn_abort(cancellation_requested(&workflow, run_id).await);
            transition_terminal(&workflow, run_id, terminal_status).await;
        }
        EventMsg::ExecApprovalRequest(request) => {
            transition_terminal(&workflow, run_id, "blocked").await;
            reject_exec_approval(conversation, event_id, request).await;
            interrupt_transient_job(conversation).await;
            return EventHandling::Suppress;
        }
        EventMsg::ApplyPatchApprovalRequest(request) => {
            transition_terminal(&workflow, run_id, "blocked").await;
            reject_patch_approval(conversation, request).await;
            interrupt_transient_job(conversation).await;
            return EventHandling::Suppress;
        }
        EventMsg::RequestPermissions(request) => {
            transition_terminal(&workflow, run_id, "blocked").await;
            reject_permissions_request(conversation, request).await;
            interrupt_transient_job(conversation).await;
            return EventHandling::Suppress;
        }
        EventMsg::RequestUserInput(_request) => {
            transition_terminal(&workflow, run_id, "blocked").await;
            reject_user_input(conversation, event_id).await;
            interrupt_transient_job(conversation).await;
            return EventHandling::Suppress;
        }
        EventMsg::ElicitationRequest(request) => {
            transition_terminal(&workflow, run_id, "blocked").await;
            reject_elicitation(conversation, request).await;
            interrupt_transient_job(conversation).await;
            return EventHandling::Suppress;
        }
        EventMsg::Error(_) => {
            // The terminal Error event is retained in ThreadState and is
            // represented by TurnComplete.  Waiting for TurnComplete keeps
            // the workflow row aligned with the canonical terminal event.
        }
        _ => {}
    }

    EventHandling::Continue
}

async fn transient_run(
    workflow: &WorkflowStore,
    thread_id: ThreadId,
) -> anyhow::Result<Option<WorkflowRun>> {
    Ok(workflow
        .get_runs_by_thread_id(&thread_id.to_string())
        .await?
        .into_iter()
        .find(|run| run.thread_class == WorkflowThreadClass::TransientJob))
}

async fn transition_turn_started(workflow: &WorkflowStore, run_id: &str) {
    let Some(run) = current_run(workflow, run_id).await else {
        return;
    };
    if terminal_status(&run.status) {
        return;
    }
    if let Err(error) = workflow
        .transition_run_status_cas(&run.run_id, &run.status, "running", None)
        .await
    {
        warn!(
            run_id = %run.run_id,
            "failed to persist transient job running state: {error}"
        );
    }
}

async fn transition_terminal(workflow: &WorkflowStore, run_id: &str, status: &str) {
    let Some(run) = current_run(workflow, run_id).await else {
        return;
    };
    if terminal_status(&run.status) {
        return;
    }
    if let Err(error) = workflow
        .transition_run_status_cas(&run.run_id, &run.status, status, Some(status))
        .await
    {
        warn!(
            run_id = %run.run_id,
            target_status = status,
            "failed to persist transient job terminal state: {error}"
        );
    }
}

fn terminal_status_for_turn_complete(event: &TurnCompleteEvent) -> &'static str {
    match event.error.as_ref() {
        None => "succeeded",
        Some(error) if is_blocking_error(error) => "blocked",
        Some(_) => "failed",
    }
}

fn terminal_status_for_turn_abort(cancel_requested: bool) -> &'static str {
    if cancel_requested {
        "cancelled"
    } else {
        "aborted"
    }
}

fn is_blocking_error(error: &ErrorEvent) -> bool {
    matches!(
        error.codex_error_info.as_ref(),
        Some(CodexErrorInfo::SessionBudgetExceeded | CodexErrorInfo::UsageLimitExceeded)
    )
}

async fn cancellation_requested(workflow: &WorkflowStore, run_id: &str) -> bool {
    let Some(run) = current_run(workflow, run_id).await else {
        return false;
    };
    run.status == "cancelled"
        || run
            .metadata
            .as_ref()
            .and_then(|metadata| {
                metadata
                    .get("cancelRequested")
                    .or_else(|| metadata.get("cancel_requested"))
            })
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false)
}

async fn current_run(workflow: &WorkflowStore, run_id: &str) -> Option<WorkflowRun> {
    match workflow.get_run(run_id).await {
        Ok(run) => run,
        Err(error) => {
            warn!(
                run_id,
                "failed to read transient job lifecycle state: {error}"
            );
            None
        }
    }
}

fn terminal_status(status: &str) -> bool {
    matches!(
        status,
        "succeeded" | "failed" | "blocked" | "inconclusive" | "cancelled" | "aborted"
    )
}

async fn reject_exec_approval(
    conversation: &Arc<CodexThread>,
    event_id: &str,
    request: &ExecApprovalRequestEvent,
) {
    let turn_id = (!request.turn_id.is_empty())
        .then(|| request.turn_id.clone())
        .or_else(|| Some(event_id.to_string()));
    submit_policy_response(
        conversation,
        codex_protocol::protocol::Op::ExecApproval {
            id: request.effective_approval_id(),
            turn_id,
            decision: ReviewDecision::denied("transient jobs cannot wait for interactive approval"),
        },
    )
    .await;
}

async fn reject_patch_approval(
    conversation: &Arc<CodexThread>,
    request: &ApplyPatchApprovalRequestEvent,
) {
    submit_policy_response(
        conversation,
        codex_protocol::protocol::Op::PatchApproval {
            id: request.call_id.clone(),
            decision: ReviewDecision::denied("transient jobs cannot wait for interactive approval"),
        },
    )
    .await;
}

async fn reject_permissions_request(
    conversation: &Arc<CodexThread>,
    request: &RequestPermissionsEvent,
) {
    submit_policy_response(
        conversation,
        codex_protocol::protocol::Op::RequestPermissionsResponse {
            id: request.call_id.clone(),
            response: RequestPermissionsResponse {
                permissions: Default::default(),
                scope: Default::default(),
                strict_auto_review: false,
            },
        },
    )
    .await;
}

async fn reject_user_input(conversation: &Arc<CodexThread>, event_id: &str) {
    submit_policy_response(
        conversation,
        codex_protocol::protocol::Op::UserInputAnswer {
            id: event_id.to_string(),
            response: RequestUserInputResponse {
                answers: HashMap::new(),
            },
        },
    )
    .await;
}

async fn reject_elicitation(
    conversation: &Arc<CodexThread>,
    request: &codex_protocol::approvals::ElicitationRequestEvent,
) {
    submit_policy_response(
        conversation,
        codex_protocol::protocol::Op::ResolveElicitation {
            server_name: request.server_name.clone(),
            request_id: request.id.clone(),
            decision: ElicitationAction::Cancel,
            content: None,
            meta: None,
        },
    )
    .await;
}

async fn submit_policy_response(
    conversation: &Arc<CodexThread>,
    operation: codex_protocol::protocol::Op,
) {
    if let Err(error) = conversation.submit(operation).await {
        error!("failed to submit automatic transient-job response: {error}");
    }
}

async fn interrupt_transient_job(conversation: &Arc<CodexThread>) {
    if let Err(error) = conversation
        .submit(codex_protocol::protocol::Op::Interrupt)
        .await
    {
        error!("failed to interrupt transient job after interactive request: {error}");
    }
}

#[cfg(test)]
#[path = "transient_job_lifecycle_tests.rs"]
mod tests;
