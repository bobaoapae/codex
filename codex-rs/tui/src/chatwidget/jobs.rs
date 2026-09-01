//! FORK: durable transient-job browsing for `/jobs`.
//!
//! The widget owns only request generations and presentation. Job data is
//! fetched by the app layer through `job/list` and `job/read`; this module
//! never scans rollout files or infers a job from message content.

use std::collections::HashMap;

use codex_app_server_protocol::Job;
use codex_app_server_protocol::JobStatus;
use codex_app_server_protocol::TerminalOutcome;
use uuid::Uuid;

use super::ChatWidget;
use crate::app_event::AppEvent;
use crate::bottom_pane::SelectionItem;
use crate::bottom_pane::SelectionViewParams;
use crate::bottom_pane::popup_consts::standard_popup_hint_line;

pub(crate) const JOBS_PICKER_VIEW_ID: &str = "jobs-picker";
const JOBS_TITLE: &str = "Transient jobs";
const JOBS_EMPTY_MESSAGE: &str = "No transient jobs found";
const JOBS_EMPTY_HINT: &str =
    "Durable jobs started with `codex exec --transient` or the app-server job/run API appear here.";

#[derive(Default)]
pub(crate) struct JobsState {
    pub(super) list_request_id: Option<Uuid>,
    pub(super) read_request_id: Option<Uuid>,
    pub(super) cancel_request_id: Option<Uuid>,
}

pub(crate) fn loading_params() -> SelectionViewParams {
    SelectionViewParams {
        view_id: Some(JOBS_PICKER_VIEW_ID),
        title: Some(JOBS_TITLE.to_string()),
        subtitle: Some("Loading durable jobs…".to_string()),
        footer_hint: Some(standard_popup_hint_line()),
        items: vec![status_item("Loading transient jobs…", "Fetching job/list…")],
        is_searchable: false,
        ..Default::default()
    }
}

pub(crate) fn empty_params() -> SelectionViewParams {
    SelectionViewParams {
        view_id: Some(JOBS_PICKER_VIEW_ID),
        title: Some(JOBS_TITLE.to_string()),
        subtitle: Some(JOBS_EMPTY_HINT.to_string()),
        footer_hint: Some(standard_popup_hint_line()),
        items: vec![
            refresh_item(),
            status_item(JOBS_EMPTY_MESSAGE, JOBS_EMPTY_HINT),
        ],
        is_searchable: true,
        search_placeholder: Some("Filter jobs".to_string()),
        ..Default::default()
    }
}

pub(crate) fn error_params(error: &str) -> SelectionViewParams {
    SelectionViewParams {
        view_id: Some(JOBS_PICKER_VIEW_ID),
        title: Some(JOBS_TITLE.to_string()),
        subtitle: Some("The job service returned an error".to_string()),
        footer_hint: Some(standard_popup_hint_line()),
        items: vec![refresh_item(), status_item("Could not load jobs", error)],
        is_searchable: false,
        ..Default::default()
    }
}

pub(crate) fn picker_params(jobs: &[Job], has_more: bool) -> SelectionViewParams {
    let mut ordered = jobs.to_vec();
    ordered.sort_by(|left, right| {
        right
            .created_at
            .cmp(&left.created_at)
            .then_with(|| right.id.cmp(&left.id))
    });

    let group_counts = ordered.iter().fold(HashMap::new(), |mut counts, job| {
        if let Some(key) = grouping_key(job) {
            *counts.entry(key).or_insert(0usize) += 1;
        }
        counts
    });
    let mut group_attempts = HashMap::new();
    let mut items = vec![refresh_item()];
    for job in &ordered {
        let group = grouping_key(job);
        let attempt = group.as_ref().map(|key| {
            let next = group_attempts.entry(key.clone()).or_insert(0usize);
            *next += 1;
            *next
        });
        if let Some(key) = group.as_deref()
            && group_attempts.get(key) == Some(&1)
        {
            let count = group_counts.get(key).copied().unwrap_or_default();
            items.push(SelectionItem {
                name: format!("Group · {key}"),
                description: Some(format!(
                    "{count} attempt{}",
                    if count == 1 { "" } else { "s" }
                )),
                search_value: Some(key.to_string()),
                is_disabled: true,
                ..Default::default()
            });
        }

        let status = job_status_label(job.status);
        let id = job.id.clone();
        let name = match attempt {
            Some(attempt) => format!("  attempt {attempt} · {status} · {}", short_id(&job.id)),
            None => format!("{status} · {}", short_id(&job.id)),
        };
        items.push(SelectionItem {
            name,
            description: Some(job_description(job)),
            search_value: Some(job_search_value(job)),
            actions: vec![Box::new(move |tx| {
                tx.send(AppEvent::ReadJob { job_id: id.clone() });
            })],
            dismiss_on_select: true,
            ..Default::default()
        });
    }
    if has_more {
        items.push(status_item(
            "More jobs available",
            "Only the first 200 jobs are shown; refresh to query the next published page.",
        ));
    }

    SelectionViewParams {
        view_id: Some(JOBS_PICKER_VIEW_ID),
        title: Some(JOBS_TITLE.to_string()),
        subtitle: Some("Newest first · select a job to read its current state".to_string()),
        footer_hint: Some(standard_popup_hint_line()),
        items,
        is_searchable: true,
        search_placeholder: Some("Filter jobs".to_string()),
        ..Default::default()
    }
}

fn refresh_item() -> SelectionItem {
    SelectionItem {
        name: "Refresh jobs".to_string(),
        description: Some("Query the app-server for the latest durable job state".to_string()),
        search_value: Some("refresh jobs".to_string()),
        actions: vec![Box::new(|tx| tx.send(AppEvent::OpenJobsPicker))],
        dismiss_on_select: true,
        ..Default::default()
    }
}

fn status_item(name: &str, description: &str) -> SelectionItem {
    SelectionItem {
        name: name.to_string(),
        description: Some(description.to_string()),
        is_disabled: true,
        ..Default::default()
    }
}

fn grouping_key(job: &Job) -> Option<String> {
    job.parent_run_id
        .as_ref()
        .map(|parent| format!("parent {}", short_id(parent)))
        .or_else(|| {
            job.idempotency_key
                .as_ref()
                .map(|key| format!("idempotency {key}"))
        })
}

fn job_search_value(job: &Job) -> String {
    [
        job.id.as_str(),
        job.thread_id.as_str(),
        job.root_thread_id.as_deref().unwrap_or_default(),
        job.parent_run_id.as_deref().unwrap_or_default(),
        job.idempotency_key.as_deref().unwrap_or_default(),
        job.model_provider.as_deref().unwrap_or_default(),
        job.model.as_deref().unwrap_or_default(),
        job.cwd.as_deref().unwrap_or_default(),
    ]
    .join(" ")
}

fn job_description(job: &Job) -> String {
    let outcome = job.outcome.map(terminal_outcome_label).unwrap_or("running");
    let provider = job.model_provider.as_deref().unwrap_or("provider unknown");
    let model = job.model.as_deref().unwrap_or("model unknown");
    let cwd = job.cwd.as_deref().unwrap_or("cwd unknown");
    format!(
        "outcome {outcome} · {provider}/{model} · {} · created {} · updated {}",
        truncate_cwd(cwd),
        format_timestamp(job.created_at),
        format_timestamp(job.updated_at)
    )
}

fn job_status_label(status: JobStatus) -> &'static str {
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
}

fn terminal_outcome_label(outcome: TerminalOutcome) -> &'static str {
    match outcome {
        TerminalOutcome::Succeeded => "succeeded",
        TerminalOutcome::Failed => "failed",
        TerminalOutcome::Blocked => "blocked",
        TerminalOutcome::Inconclusive => "inconclusive",
        TerminalOutcome::Cancelled => "cancelled",
        TerminalOutcome::Aborted => "aborted",
    }
}

fn short_id(value: &str) -> &str {
    value.get(..8).unwrap_or(value)
}

fn truncate_cwd(cwd: &str) -> String {
    const MAX_CWD_CHARS: usize = 48;
    if cwd.chars().count() <= MAX_CWD_CHARS {
        return cwd.to_string();
    }
    let tail = cwd
        .chars()
        .rev()
        .take(MAX_CWD_CHARS.saturating_sub(3))
        .collect::<String>()
        .chars()
        .rev()
        .collect::<String>();
    format!("...{tail}")
}

fn format_timestamp(value: i64) -> String {
    chrono::DateTime::from_timestamp(value, /*nsecs*/ 0).map_or_else(
        || "unknown".to_string(),
        |timestamp| {
            timestamp
                .with_timezone(&chrono::Local)
                .format("%Y-%m-%d %H:%M")
                .to_string()
        },
    )
}

fn can_cancel(job: &Job) -> bool {
    matches!(job.status, JobStatus::Pending | JobStatus::Running)
}

impl ChatWidget {
    pub(crate) fn begin_jobs_picker_request(&mut self) -> Uuid {
        let request_id = Uuid::new_v4();
        self.jobs.list_request_id = Some(request_id);
        request_id
    }

    pub(crate) fn finish_jobs_picker_request(&mut self, request_id: Uuid) -> bool {
        if self.jobs.list_request_id != Some(request_id) {
            return false;
        }
        self.jobs.list_request_id = None;
        true
    }

    pub(crate) fn begin_job_read(&mut self) -> Uuid {
        let request_id = Uuid::new_v4();
        self.jobs.read_request_id = Some(request_id);
        request_id
    }

    pub(crate) fn finish_job_read(&mut self, request_id: Uuid) -> bool {
        if self.jobs.read_request_id != Some(request_id) {
            return false;
        }
        self.jobs.read_request_id = None;
        true
    }

    pub(crate) fn begin_job_cancel(&mut self) -> Uuid {
        let request_id = Uuid::new_v4();
        self.jobs.cancel_request_id = Some(request_id);
        request_id
    }

    pub(crate) fn finish_job_cancel(&mut self, request_id: Uuid) -> bool {
        if self.jobs.cancel_request_id != Some(request_id) {
            return false;
        }
        self.jobs.cancel_request_id = None;
        true
    }

    pub(crate) fn show_jobs_loading(&mut self) {
        self.bottom_pane.show_selection_view(loading_params());
    }

    pub(crate) fn show_jobs_picker(&mut self, jobs: Vec<Job>, has_more: bool) {
        self.bottom_pane
            .show_selection_view(picker_params(&jobs, has_more));
    }

    pub(crate) fn show_jobs_empty(&mut self) {
        self.bottom_pane.show_selection_view(empty_params());
    }

    pub(crate) fn show_jobs_error(&mut self, error: String) {
        self.bottom_pane.show_selection_view(error_params(&error));
    }

    pub(crate) fn show_job_details(&mut self, job: Job) {
        let mut items = vec![
            status_item("Status", job_status_label(job.status)),
            status_item(
                "Outcome",
                job.outcome.map(terminal_outcome_label).unwrap_or("running"),
            ),
            status_item(
                "Provider / model",
                &format!(
                    "{}/{}",
                    job.model_provider.as_deref().unwrap_or("unknown"),
                    job.model.as_deref().unwrap_or("unknown")
                ),
            ),
            status_item("CWD", job.cwd.as_deref().unwrap_or("unknown")),
            status_item("Created", &format_timestamp(job.created_at)),
            status_item("Updated", &format_timestamp(job.updated_at)),
        ];
        if let Some(started_at) = job.started_at {
            items.push(status_item("Started", &format_timestamp(started_at)));
        }
        if let Some(finished_at) = job.finished_at {
            items.push(status_item("Finished", &format_timestamp(finished_at)));
        }
        if let Some(parent) = job.parent_run_id.as_deref() {
            items.push(status_item("Parent run", parent));
        }
        if let Some(key) = job.idempotency_key.as_deref() {
            items.push(status_item("Idempotency", key));
        }
        if can_cancel(&job) {
            let job_id = job.id.clone();
            items.push(SelectionItem {
                name: "Cancel this job".to_string(),
                description: Some("Send an explicit cancellation request".to_string()),
                actions: vec![Box::new(move |tx| {
                    tx.send(AppEvent::OpenJobCancelConfirmation {
                        job_id: job_id.clone(),
                    });
                })],
                dismiss_on_select: true,
                ..Default::default()
            });
        }

        self.bottom_pane.show_selection_view(SelectionViewParams {
            view_id: Some(JOBS_PICKER_VIEW_ID),
            title: Some(format!("Job {}", short_id(&job.id))),
            subtitle: Some(format!("{} · {}", job.id, job_description(&job))),
            footer_hint: Some(standard_popup_hint_line()),
            items,
            ..Default::default()
        });
    }

    pub(crate) fn show_job_cancel_confirmation(&mut self, job_id: String) {
        let cancel_id = job_id.clone();
        self.bottom_pane.show_selection_view(SelectionViewParams {
            view_id: Some(JOBS_PICKER_VIEW_ID),
            title: Some("Cancel this job?".to_string()),
            subtitle: Some(format!(
                "This sends an explicit job/cancel request for {job_id}."
            )),
            footer_hint: Some(standard_popup_hint_line()),
            items: vec![
                SelectionItem {
                    name: "Keep job running".to_string(),
                    dismiss_on_select: true,
                    ..Default::default()
                },
                SelectionItem {
                    name: "Cancel job".to_string(),
                    description: Some(
                        "No retry or automatic replacement will be started.".to_string(),
                    ),
                    actions: vec![Box::new(move |tx| {
                        tx.send(AppEvent::CancelJob {
                            job_id: cancel_id.clone(),
                        });
                    })],
                    dismiss_on_select: true,
                    ..Default::default()
                },
            ],
            ..Default::default()
        });
    }

    pub(crate) fn show_job_read_error(&mut self, error: String) {
        self.show_jobs_error(format!("Could not read job: {error}"));
    }

    pub(crate) fn show_job_cancel_result(&mut self, job: &Job) {
        self.add_info_message(
            format!(
                "Job {} is now {}.",
                short_id(&job.id),
                job_status_label(job.status)
            ),
            Some("Run /jobs to refresh the durable job list.".to_string()),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use codex_app_server_protocol::ThreadClass;
    use insta::assert_snapshot;

    fn job(id: &str, status: JobStatus) -> Job {
        Job {
            id: id.to_string(),
            thread_class: ThreadClass::TransientJob,
            thread_id: id.to_string(),
            root_thread_id: None,
            parent_run_id: None,
            status,
            outcome: None,
            idempotency_key: None,
            model_provider: Some("openai".to_string()),
            model: Some("gpt-5".to_string()),
            cwd: Some("C:/work/project".to_string()),
            created_at: 1_756_000_000,
            updated_at: 1_756_000_060,
            started_at: None,
            finished_at: None,
            version: 1,
        }
    }

    #[test]
    fn picker_has_deterministic_loading_empty_and_error_states() {
        let loading = loading_params();
        let empty = empty_params();
        let error = error_params("server unavailable");
        assert_snapshot!(loading.items[0].name.as_str(), @r"Loading transient jobs…");
        assert_snapshot!(empty.items[1].name.as_str(), @r"No transient jobs found");
        assert_snapshot!(
            error.items[1].description.as_deref().unwrap(),
            @r"server unavailable"
        );
    }

    #[test]
    fn picker_groups_only_explicit_attempt_metadata() {
        let mut first = job("job-0000001", JobStatus::Running);
        first.idempotency_key = Some("same-request".to_string());
        let mut second = job("job-0000002", JobStatus::Succeeded);
        second.idempotency_key = Some("same-request".to_string());
        let params = picker_params(&[first, second], false);

        assert_eq!(params.items.len(), 4);
        assert!(params.items[1].is_disabled);
        assert!(params.items[2].name.contains("attempt 1"));
        assert!(params.items[3].name.contains("attempt 2"));
        let row_names = params
            .items
            .iter()
            .map(|item| item.name.as_str())
            .collect::<Vec<_>>()
            .join("\n");
        assert_snapshot!(
            row_names.as_str(),
            @r###"
Refresh jobs
Group · idempotency same-request
  attempt 1 · succeeded · job-0000
  attempt 2 · running · job-0000
"###
        );
    }

    #[test]
    fn terminal_jobs_do_not_offer_cancel() {
        let params = picker_params(&[job("job-0000001", JobStatus::Succeeded)], false);
        assert_eq!(params.items.len(), 2);
        let details = can_cancel(&job("job-0000001", JobStatus::Succeeded));
        assert!(!details);
    }
}
