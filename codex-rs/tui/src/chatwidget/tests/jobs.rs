use super::*;

use crate::chatwidget::jobs;
use codex_app_server_protocol::Job;
use codex_app_server_protocol::JobStatus;
use codex_app_server_protocol::TerminalOutcome;
use codex_app_server_protocol::ThreadClass;
use insta::assert_snapshot;
use pretty_assertions::assert_eq;

fn job(id: &str, status: JobStatus) -> Job {
    Job {
        id: id.to_string(),
        thread_class: ThreadClass::TransientJob,
        thread_id: id.to_string(),
        root_thread_id: None,
        parent_run_id: None,
        status,
        outcome: Some(TerminalOutcome::Succeeded).filter(|_| status == JobStatus::Succeeded),
        idempotency_key: None,
        model_provider: Some("openai".to_string()),
        model: Some("gpt-5".to_string()),
        cwd: Some("C:/work/project".to_string()),
        created_at: 1_756_000_000,
        updated_at: 1_756_000_060,
        started_at: Some(1_756_000_010),
        finished_at: (status == JobStatus::Succeeded).then_some(1_756_000_060),
        version: 1,
    }
}

#[tokio::test]
async fn jobs_slash_command_opens_the_picker() {
    let (mut chat, mut rx, _op_rx) = make_chatwidget_manual(/*model_override*/ None).await;

    chat.dispatch_command(SlashCommand::Jobs);

    let events = std::iter::from_fn(|| rx.try_recv().ok()).collect::<Vec<_>>();
    assert!(
        events
            .iter()
            .any(|event| matches!(event, AppEvent::OpenJobsPicker)),
        "expected OpenJobsPicker; events: {events:?}"
    );
}

#[test]
fn jobs_picker_exposes_lifecycle_metadata_and_groups_explicit_attempts() {
    let mut first = job("job-0000001", JobStatus::Running);
    first.idempotency_key = Some("same-request".to_string());
    let mut second = job("job-0000002", JobStatus::Succeeded);
    second.idempotency_key = Some("same-request".to_string());
    let params = jobs::picker_params(&[first, second], false);

    assert_eq!(params.view_id, Some(jobs::JOBS_PICKER_VIEW_ID));
    assert!(params.is_searchable);
    assert!(
        params
            .items
            .iter()
            .any(|item| item.description.as_deref().is_some_and(|text| {
                text.contains("openai/gpt-5")
                    && text.contains("C:/work/project")
                    && text.contains("created")
                    && text.contains("updated")
            }))
    );
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
fn jobs_picker_has_deterministic_loading_empty_and_error_states() {
    let loading = jobs::loading_params();
    let empty = jobs::empty_params();
    let error = jobs::error_params("job service unavailable");

    assert_snapshot!(
        loading.items[0].name.as_str(),
        @r"Loading transient jobs…"
    );
    assert_snapshot!(
        empty.items[1].name.as_str(),
        @r"No transient jobs found"
    );
    assert_snapshot!(
        error.items[1].description.as_deref().unwrap(),
        @r"job service unavailable"
    );
}

#[tokio::test]
async fn job_details_offer_cancel_only_for_non_terminal_jobs() {
    let (mut chat, _rx, _op_rx) = make_chatwidget_manual(/*model_override*/ None).await;
    chat.show_job_details(job("job-running", JobStatus::Running));
    assert!(
        chat.bottom_pane
            .selected_index_for_present_view(jobs::JOBS_PICKER_VIEW_ID)
            .is_some()
    );

    chat.show_job_details(job("job-done", JobStatus::Succeeded));
    assert!(
        chat.bottom_pane
            .selected_index_for_present_view(jobs::JOBS_PICKER_VIEW_ID)
            .is_some()
    );
}
