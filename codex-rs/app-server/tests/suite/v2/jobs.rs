use anyhow::Result;
use app_test_support::TestAppServer;
use app_test_support::create_mock_responses_server_repeating_assistant;
use codex_app_server_protocol::ClientRequest;
use codex_app_server_protocol::JobListParams;
use codex_app_server_protocol::JobListResponse;
use codex_app_server_protocol::JobReadParams;
use codex_app_server_protocol::JobReadResponse;
use codex_app_server_protocol::JobRunParams;
use codex_app_server_protocol::JobRunResponse;
use codex_app_server_protocol::ThreadClass;
use codex_app_server_protocol::ThreadStartParams;
use codex_app_server_protocol::UserInput;
use core_test_support::PathExt;
use core_test_support::skip_if_remote;
use pretty_assertions::assert_eq;
use serde_json::json;
use std::collections::HashMap;
use tempfile::TempDir;

#[tokio::test]
async fn job_list_is_empty_and_keyset_compatible() -> Result<()> {
    let mut app = TestAppServer::builder().build_initialized().await?;
    let response: JobListResponse = app
        .request(|request_id| ClientRequest::JobList {
            request_id,
            params: JobListParams::default(),
        })
        .await?;
    assert!(response.data.is_empty());
    assert!(response.next_cursor.is_none());
    Ok(())
}

#[tokio::test]
async fn fork_invariant_job_run_is_transient_and_idempotent() -> Result<()> {
    skip_if_remote!(
        Ok(()),
        "uses a host-local mock provider and durable workflow database"
    );
    let model_server = create_mock_responses_server_repeating_assistant("Done").await;
    let codex_home = TempDir::new()?;
    std::fs::write(
        codex_home.path().join("config.toml"),
        format!(
            r#"
model = "mock-model"
approval_policy = "never"
sandbox_mode = "read-only"
model_provider = "mock_provider"

[model_providers.mock_provider]
name = "Mock provider for jobs"
base_url = "{}/v1"
wire_api = "responses"
request_max_retries = 0
stream_max_retries = 0
"#,
            model_server.uri()
        ),
    )?;
    let mut app = TestAppServer::builder()
        .with_codex_home(codex_home.path())
        .build_initialized()
        .await?;
    let params = JobRunParams {
        input: vec![UserInput::Text {
            text: "job input".to_string(),
            text_elements: Vec::new(),
        }],
        idempotency_key: Some("job-idempotency".to_string()),
        ..Default::default()
    };
    let first: JobRunResponse = app
        .request(|request_id| ClientRequest::JobRun {
            request_id,
            params: params.clone(),
        })
        .await?;
    let second: JobRunResponse = app
        .request(|request_id| ClientRequest::JobRun { request_id, params })
        .await?;
    assert_eq!(first.job.id, second.job.id);
    assert_eq!(first.job.thread_id, first.job.id);
    assert_eq!(first.job.thread_class, ThreadClass::TransientJob);
    assert!(first.job.root_thread_id.is_none());

    let thread = app
        .start_thread(ThreadStartParams {
            thread_class: Some(ThreadClass::TransientJob),
            ..Default::default()
        })
        .await?
        .thread;
    let jobs: JobListResponse = app
        .request(|request_id| ClientRequest::JobList {
            request_id,
            params: JobListParams::default(),
        })
        .await?;
    assert!(jobs.data.iter().any(|job| job.thread_id == thread.id
        && job.status == codex_app_server_protocol::JobStatus::Pending));
    Ok(())
}

#[tokio::test]
async fn job_run_keeps_large_prompt_out_of_workflow_metadata() -> Result<()> {
    skip_if_remote!(
        Ok(()),
        "uses a host-local mock provider and durable workflow database"
    );
    let model_server = create_mock_responses_server_repeating_assistant("Done").await;
    let codex_home = TempDir::new()?;
    std::fs::write(
        codex_home.path().join("config.toml"),
        format!(
            r#"
model = "mock-model"
approval_policy = "never"
sandbox_mode = "read-only"
model_provider = "mock_provider"

[model_providers.mock_provider]
name = "Mock provider for large jobs"
base_url = "{}/v1"
wire_api = "responses"
request_max_retries = 0
stream_max_retries = 0
"#,
            model_server.uri()
        ),
    )?;
    let mut app = TestAppServer::builder()
        .with_codex_home(codex_home.path())
        .build_initialized()
        .await?;

    let secret_prompt = "JOB_PROMPT_SECRET";
    let params = JobRunParams {
        input: vec![UserInput::Text {
            text: format!("{secret_prompt}{}", "x".repeat(70 * 1024)),
            text_elements: Vec::new(),
        }],
        config: Some(HashMap::from([(
            "features.connectors".to_string(),
            json!(false),
        )])),
        ..Default::default()
    };
    let response: JobRunResponse = app
        .request(|request_id| ClientRequest::JobRun { request_id, params })
        .await?;
    assert_eq!(response.job.thread_class, ThreadClass::TransientJob);

    let read: JobReadResponse = app
        .request(|request_id| ClientRequest::JobRead {
            request_id,
            params: JobReadParams {
                job_id: response.job.id.clone(),
            },
        })
        .await?;
    assert_eq!(read.job.id, response.job.id);

    let workflow = codex_state::WorkflowStore::open(&codex_state::SqliteConfig::new_for_testing(
        codex_home.path().abs(),
    ))
    .await?;
    let run = workflow
        .get_run(&response.job.id)
        .await?
        .expect("workflow run should be persisted");
    let metadata = run.metadata.expect("allowlisted job metadata");
    let metadata_json = serde_json::to_string(&metadata)?;
    assert!(metadata_json.len() < 64 * 1024);
    assert!(!metadata_json.contains(secret_prompt));
    assert!(!metadata_json.contains("features.connectors"));
    assert_eq!(metadata["inputItemCount"], json!(1));
    assert_eq!(metadata["requestedThreadClass"], json!("transientJob"));
    workflow.close().await;
    Ok(())
}
