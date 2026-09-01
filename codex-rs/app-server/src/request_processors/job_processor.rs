//! Experimental app-server APIs for durable transient jobs.
//!
//! Jobs use the regular thread and turn machinery, but keep their lifecycle in
//! the workflow store. This processor deliberately does not expose model
//! output or stdout: clients can inspect the associated thread/rollout using
//! the existing thread APIs when they need that detail.

use std::sync::Arc;

use codex_app_server_protocol::ClientResponsePayload;
use codex_app_server_protocol::JSONRPCErrorError;
use codex_app_server_protocol::JobCancelParams;
use codex_app_server_protocol::JobCancelResponse;
use codex_app_server_protocol::JobListParams;
use codex_app_server_protocol::JobListResponse;
use codex_app_server_protocol::JobReadParams;
use codex_app_server_protocol::JobReadResponse;
use codex_app_server_protocol::JobRunParams;
use codex_app_server_protocol::JobRunResponse;
use codex_app_server_protocol::TurnInterruptParams;
use codex_app_server_protocol::TurnStartParams;
use codex_core::ThreadManager;
use codex_protocol::mcp::ClientMcpExtensions;
use codex_rollout::StateDbHandle;
use codex_state::WorkflowRun;
use codex_state::WorkflowRunCreate;
use codex_state::WorkflowRunCursor;
use codex_state::WorkflowRunListFilter;
use codex_state::WorkflowRunListRequest;
use codex_state::WorkflowStore;
use codex_state::WorkflowThreadClass;
use tokio::time::Duration;
use tokio::time::timeout;
use tokio_util::task::TaskTracker;
use tracing::Instrument;

use crate::error_code::internal_error;
use crate::error_code::invalid_request;
use crate::outgoing_message::ConnectionRequestId;
use crate::outgoing_message::RequestContext;
use crate::request_processors::ThreadRequestProcessor;
use crate::request_processors::TurnRequestProcessor;

use super::job_processor_support::JOB_IDEMPOTENCY_ROOT;
use super::job_processor_support::api_job;
use super::job_processor_support::immutable_job_metadata;
use super::job_processor_support::is_terminal;
use super::job_processor_support::job_status_string;
use super::job_processor_support::terminal_outcome_string;
use super::job_processor_support::terminalize;
use super::job_processor_support::thread_start_params;
use super::job_processor_support::workflow_error;

const JOB_LIST_DEFAULT_LIMIT: u32 = 50;
const JOB_LIST_MAX_LIMIT: u32 = 200;

/// Handles lifecycle and query operations for durable transient jobs.
pub(crate) struct JobRequestProcessor {
    workflow: Option<WorkflowStore>,
    thread_manager: Arc<ThreadManager>,
    thread_processor: ThreadRequestProcessor,
    turn_processor: TurnRequestProcessor,
    config: Arc<codex_core::config::Config>,
    tasks: TaskTracker,
}

impl JobRequestProcessor {
    pub(crate) fn new(
        state_db: Option<StateDbHandle>,
        thread_manager: Arc<ThreadManager>,
        thread_processor: ThreadRequestProcessor,
        turn_processor: TurnRequestProcessor,
        config: Arc<codex_core::config::Config>,
    ) -> Self {
        Self {
            workflow: state_db.map(|state_db| state_db.workflow_store().clone()),
            thread_manager,
            thread_processor,
            turn_processor,
            config,
            tasks: TaskTracker::new(),
        }
    }

    fn workflow_store(&self) -> Result<&WorkflowStore, JSONRPCErrorError> {
        self.workflow
            .as_ref()
            .ok_or_else(|| invalid_request("durable jobs require sqlite state"))
    }

    pub(crate) async fn job_run(
        &self,
        request_id: ConnectionRequestId,
        params: JobRunParams,
        app_server_client_name: Option<String>,
        app_server_client_version: Option<String>,
        client_mcp_extensions: ClientMcpExtensions,
        request_context: RequestContext,
    ) -> Result<Option<ClientResponsePayload>, JSONRPCErrorError> {
        let workflow = self.workflow_store()?;
        TurnRequestProcessor::validate_job_input(&params.input)?;
        if params.idempotency_key.as_deref().is_some_and(str::is_empty) {
            return Err(invalid_request("idempotencyKey must not be empty"));
        }
        if params.sandbox.is_some() && params.permissions.is_some() {
            return Err(invalid_request(
                "`permissions` cannot be combined with `sandbox`",
            ));
        }
        let metadata = immutable_job_metadata(&params).map_err(|error| {
            invalid_request(format!(
                "failed to encode immutable job parameters: {error}"
            ))
        })?;
        let thread_id = self.thread_manager.reserve_thread_id();
        let run_id = thread_id.to_string();
        let root_thread_id = params
            .idempotency_key
            .as_ref()
            .map(|_| JOB_IDEMPOTENCY_ROOT.to_string());
        let run_input = WorkflowRunCreate {
            run_id: run_id.clone(),
            thread_id: run_id.clone(),
            root_thread_id,
            parent_run_id: None,
            thread_class: WorkflowThreadClass::TransientJob,
            status: "pending".to_string(),
            idempotency_key: params.idempotency_key.clone(),
            provider: params
                .model_provider
                .clone()
                .or_else(|| Some(self.config.model_provider_id.clone())),
            model: params.model.clone().or_else(|| self.config.model.clone()),
            cwd: params
                .cwd
                .as_ref()
                .map(|cwd| cwd.to_string_lossy().into_owned())
                .or_else(|| Some(self.config.cwd.to_string_lossy().into_owned())),
            metadata: Some(metadata),
        };
        let run = workflow
            .create_run(&run_input)
            .await
            .map_err(workflow_error)?;

        // A different reserved ID means another request won the idempotency
        // race. Return that durable result and never start a duplicate thread.
        if run.run_id != run_id {
            return Ok(Some(JobRunResponse { job: api_job(run)? }.into()));
        }

        let worker = Self::run_job(
            workflow.clone(),
            run,
            params,
            request_id,
            app_server_client_name,
            app_server_client_version,
            client_mcp_extensions,
            request_context.clone(),
            self.thread_processor.clone(),
            self.turn_processor.clone(),
        );
        self.tasks.spawn(worker.instrument(request_context.span()));

        let run = workflow
            .get_run(&run_id)
            .await
            .map_err(workflow_error)?
            .ok_or_else(|| internal_error("job disappeared after creation"))?;
        Ok(Some(JobRunResponse { job: api_job(run)? }.into()))
    }

    pub(crate) async fn job_list(
        &self,
        params: JobListParams,
    ) -> Result<Option<ClientResponsePayload>, JSONRPCErrorError> {
        let workflow = self.workflow_store()?;
        let limit = params
            .limit
            .unwrap_or(JOB_LIST_DEFAULT_LIMIT)
            .clamp(1, JOB_LIST_MAX_LIMIT);
        let cursor = params
            .cursor
            .as_deref()
            .map(WorkflowRunCursor::decode)
            .transpose()
            .map_err(|error| invalid_request(format!("invalid job pagination cursor: {error}")))?;
        let outcome_filter = params.outcome;
        let filter = WorkflowRunListFilter {
            thread_class: Some(WorkflowThreadClass::TransientJob),
            status: params.status.map(job_status_string),
            root_thread_id: params.root_thread_id,
        };
        let mut page_cursor = cursor;
        let mut runs = Vec::with_capacity(limit as usize);
        loop {
            let request = WorkflowRunListRequest::new(filter.clone(), page_cursor, limit)
                .map_err(workflow_error)?;
            let page = workflow.list_runs(&request).await.map_err(workflow_error)?;
            page_cursor = page.next_cursor;
            runs.extend(page.runs.into_iter().filter(|run| {
                outcome_filter.is_none_or(|outcome| {
                    run.outcome.as_deref() == Some(terminal_outcome_string(outcome))
                })
            }));
            if runs.len() >= limit as usize || page_cursor.is_none() {
                break;
            }
        }
        runs.truncate(limit as usize);
        let data = runs
            .iter()
            .cloned()
            .map(api_job)
            .collect::<Result<Vec<_>, _>>()?;
        let source_has_more = page_cursor.is_some();
        let next_cursor = source_has_more
            .then(|| runs.last())
            .flatten()
            .map(|run| WorkflowRunCursor {
                created_at_ms: run.created_at_ms,
                run_id: run.run_id.clone(),
            })
            .map(|cursor| cursor.encode())
            .transpose()
            .map_err(|error| {
                internal_error(format!("failed to encode job pagination cursor: {error}"))
            })?;
        Ok(Some(JobListResponse { data, next_cursor }.into()))
    }

    pub(crate) async fn job_read(
        &self,
        params: JobReadParams,
    ) -> Result<Option<ClientResponsePayload>, JSONRPCErrorError> {
        let workflow = self.workflow_store()?;
        let run = workflow
            .get_run(&params.job_id)
            .await
            .map_err(workflow_error)?
            .ok_or_else(|| invalid_request(format!("job not found: {}", params.job_id)))?;
        if run.thread_class != WorkflowThreadClass::TransientJob {
            return Err(invalid_request(format!("job not found: {}", params.job_id)));
        }
        Ok(Some(JobReadResponse { job: api_job(run)? }.into()))
    }

    pub(crate) async fn job_cancel(
        &self,
        request_id: ConnectionRequestId,
        params: JobCancelParams,
    ) -> Result<Option<ClientResponsePayload>, JSONRPCErrorError> {
        let workflow = self.workflow_store()?;
        let run = workflow
            .get_run(&params.job_id)
            .await
            .map_err(workflow_error)?
            .ok_or_else(|| invalid_request(format!("job not found: {}", params.job_id)))?;
        if run.thread_class != WorkflowThreadClass::TransientJob {
            return Err(invalid_request(format!("job not found: {}", params.job_id)));
        }
        let run_id = run.run_id.clone();
        let mut current = run;
        for _ in 0..2 {
            if is_terminal(&current.status) {
                return Ok(Some(
                    JobCancelResponse {
                        job: api_job(current)?,
                    }
                    .into(),
                ));
            }

            if current.status == "running" {
                // An empty turn id is the existing startup/whole-thread
                // interrupt form. It avoids inventing a second turn-id
                // registry for jobs.
                if let Err(error) = self
                    .turn_processor
                    .interrupt_for_job(
                        &request_id,
                        TurnInterruptParams {
                            thread_id: current.thread_id.clone(),
                            turn_id: String::new(),
                        },
                    )
                    .await
                {
                    let latest = workflow
                        .get_run(&current.run_id)
                        .await
                        .map_err(workflow_error)?;
                    if latest.is_some_and(|run| is_terminal(&run.status)) {
                        break;
                    }
                    return Err(error);
                }
            }

            if workflow
                .transition_run_cas(
                    &current.run_id,
                    current.version,
                    &current.status,
                    "cancelled",
                    Some("cancelled"),
                )
                .await
                .map_err(workflow_error)?
            {
                break;
            }
            current = workflow
                .get_run(&current.run_id)
                .await
                .map_err(workflow_error)?
                .ok_or_else(|| internal_error("job disappeared during cancellation"))?;
        }
        let final_run = workflow
            .get_run(&run_id)
            .await
            .map_err(workflow_error)?
            .ok_or_else(|| internal_error("job disappeared after cancellation"))?;
        Ok(Some(
            JobCancelResponse {
                job: api_job(final_run)?,
            }
            .into(),
        ))
    }

    pub(crate) async fn drain_background_tasks(&self) {
        self.tasks.close();
        if timeout(Duration::from_secs(10), self.tasks.wait())
            .await
            .is_err()
        {
            tracing::warn!("timed out waiting for durable job tasks to shut down; proceeding");
        }
    }

    #[allow(clippy::too_many_arguments)]
    async fn run_job(
        workflow: WorkflowStore,
        run: WorkflowRun,
        params: JobRunParams,
        request_id: ConnectionRequestId,
        app_server_client_name: Option<String>,
        app_server_client_version: Option<String>,
        client_mcp_extensions: ClientMcpExtensions,
        request_context: RequestContext,
        thread_processor: ThreadRequestProcessor,
        turn_processor: TurnRequestProcessor,
    ) {
        let thread_id = match codex_protocol::ThreadId::from_string(&run.thread_id) {
            Ok(thread_id) => thread_id,
            Err(error) => {
                let _ = terminalize(&workflow, &run, "failed").await;
                tracing::error!(job_id = %run.run_id, "invalid job thread id: {error}");
                return;
            }
        };
        let thread_params = thread_start_params(&params);
        let _thread = match thread_processor
            .start_thread_for_job(
                request_id.clone(),
                thread_params,
                app_server_client_name,
                app_server_client_version,
                client_mcp_extensions,
                request_context.clone(),
                thread_id,
            )
            .await
        {
            Ok(thread) => thread,
            Err(error) => {
                let _ = terminalize(&workflow, &run, "failed").await;
                tracing::error!(job_id = %run.run_id, "failed to start job thread: {error:?}");
                return;
            }
        };

        let Some(current) = workflow.get_run(&run.run_id).await.ok().flatten() else {
            return;
        };
        if is_terminal(&current.status) {
            return;
        }
        let turn_params = TurnStartParams {
            thread_id: run.thread_id.clone(),
            input: params.input,
            output_schema: params.output_schema,
            ..Default::default()
        };
        if let Err(error) = turn_processor
            .start_turn_for_job(request_id, turn_params)
            .await
        {
            let _ = terminalize(&workflow, &run, "failed").await;
            tracing::error!(job_id = %run.run_id, "failed to start job turn: {error:?}");
        }
        // The regular listener owns delivery and persistence of the terminal
        // event; the manager retains the thread after this task completes.
    }
}
