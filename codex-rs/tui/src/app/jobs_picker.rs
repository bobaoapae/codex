//! FORK: app-server backed `/jobs` picker.
//!
//! This module owns only RPC orchestration. Durable job metadata comes from
//! `job/list` and `job/read`; no rollout scan or content-based classification
//! is performed here.

use std::collections::HashSet;

use super::*;
use crate::app_event::AppEvent;
use codex_app_server_client::AppServerRequestHandle;
use codex_app_server_protocol::ClientRequest;
use codex_app_server_protocol::JobCancelParams;
use codex_app_server_protocol::JobCancelResponse;
use codex_app_server_protocol::JobListParams;
use codex_app_server_protocol::JobListResponse;
use codex_app_server_protocol::JobReadParams;
use codex_app_server_protocol::JobReadResponse;
use codex_app_server_protocol::RequestId;
use uuid::Uuid;

const JOB_LIST_PAGE_SIZE: u32 = 50;
const JOB_LIST_MAX_ROWS: usize = 200;

impl App {
    pub(super) fn open_jobs_picker(&mut self, app_server: &AppServerSession) {
        let request_id = self.chat_widget.begin_jobs_picker_request();
        self.chat_widget.show_jobs_loading();
        let request_handle = app_server.request_handle();
        let app_event_tx = self.app_event_tx.clone();
        tokio::spawn(async move {
            let (result, has_more) = match load_jobs(request_handle).await {
                Ok((jobs, has_more)) => (Ok(jobs), has_more),
                Err(error) => (Err(error), false),
            };
            app_event_tx.send(AppEvent::JobsPickerLoaded {
                request_id,
                result,
                has_more,
            });
        });
    }

    pub(super) fn apply_jobs_picker_result(
        &mut self,
        request_id: Uuid,
        result: Result<Vec<codex_app_server_protocol::Job>, String>,
        has_more: bool,
    ) {
        if !self.chat_widget.finish_jobs_picker_request(request_id) {
            return;
        }
        match result {
            Ok(jobs) if jobs.is_empty() => {
                self.chat_widget.show_jobs_empty();
                if has_more {
                    self.chat_widget.show_jobs_error(
                        "The job service returned more rows but no usable job metadata."
                            .to_string(),
                    );
                }
            }
            Ok(jobs) => {
                self.chat_widget.show_jobs_picker(jobs, has_more);
            }
            Err(error) => self.chat_widget.show_jobs_error(error),
        }
    }

    pub(super) fn read_job(&mut self, app_server: &AppServerSession, job_id: String) {
        let request_id = self.chat_widget.begin_job_read();
        let request_handle = app_server.request_handle();
        let app_event_tx = self.app_event_tx.clone();
        tokio::spawn(async move {
            let result = request_handle
                .request_typed::<JobReadResponse>(ClientRequest::JobRead {
                    request_id: RequestId::String(format!("job-read-{}", Uuid::new_v4())),
                    params: JobReadParams { job_id },
                })
                .await
                .map_err(|error| error.to_string());
            app_event_tx.send(AppEvent::JobReadLoaded { request_id, result });
        });
    }

    pub(super) fn apply_job_read_result(
        &mut self,
        request_id: Uuid,
        result: Result<JobReadResponse, String>,
    ) {
        if !self.chat_widget.finish_job_read(request_id) {
            return;
        }
        match result {
            Ok(response) => self.chat_widget.show_job_details(response.job),
            Err(error) => self.chat_widget.show_job_read_error(error),
        }
    }

    pub(super) fn cancel_job(&mut self, app_server: &AppServerSession, job_id: String) {
        let request_id = self.chat_widget.begin_job_cancel();
        let request_handle = app_server.request_handle();
        let app_event_tx = self.app_event_tx.clone();
        tokio::spawn(async move {
            let result = request_handle
                .request_typed::<JobCancelResponse>(ClientRequest::JobCancel {
                    request_id: RequestId::String(format!("job-cancel-{}", Uuid::new_v4())),
                    params: JobCancelParams { job_id },
                })
                .await
                .map_err(|error| error.to_string());
            app_event_tx.send(AppEvent::JobCancelLoaded { request_id, result });
        });
    }

    pub(super) fn apply_job_cancel_result(
        &mut self,
        request_id: Uuid,
        result: Result<JobCancelResponse, String>,
    ) {
        if !self.chat_widget.finish_job_cancel(request_id) {
            return;
        }
        match result {
            Ok(response) => self.chat_widget.show_job_cancel_result(&response.job),
            Err(error) => self
                .chat_widget
                .add_error_message(format!("Failed to cancel job: {error}")),
        }
    }
}

async fn load_jobs(
    request_handle: AppServerRequestHandle,
) -> Result<(Vec<codex_app_server_protocol::Job>, bool), String> {
    let mut jobs = Vec::new();
    let mut cursor = None;
    let mut seen_cursors = HashSet::new();
    let mut has_more = false;

    while jobs.len() < JOB_LIST_MAX_ROWS && seen_cursors.insert(cursor.clone()) {
        let page = request_handle
            .request_typed::<JobListResponse>(ClientRequest::JobList {
                request_id: RequestId::String(format!("job-list-{}", Uuid::new_v4())),
                params: JobListParams {
                    cursor,
                    limit: Some(JOB_LIST_PAGE_SIZE),
                    status: None,
                    outcome: None,
                    root_thread_id: None,
                },
            })
            .await
            .map_err(|error| error.to_string())?;

        let remaining = JOB_LIST_MAX_ROWS.saturating_sub(jobs.len());
        jobs.extend(page.data.into_iter().take(remaining));
        let Some(next_cursor) = page.next_cursor else {
            break;
        };
        if jobs.len() >= JOB_LIST_MAX_ROWS {
            has_more = true;
            break;
        }
        cursor = Some(next_cursor);
    }

    Ok((jobs, has_more))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn list_limits_each_backend_page_to_fifty_rows() {
        assert_eq!(JOB_LIST_PAGE_SIZE, 50);
        assert_eq!(JOB_LIST_MAX_ROWS, 200);
    }
}
