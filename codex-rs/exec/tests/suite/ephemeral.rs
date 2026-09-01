#![cfg(not(target_os = "windows"))]
#![allow(clippy::unwrap_used)]

use core_test_support::responses;
use core_test_support::skip_if_no_network;
use core_test_support::test_codex_exec::test_codex_exec;
use walkdir::WalkDir;
use wiremock::MockServer;

fn exec_sse_response() -> String {
    responses::sse(vec![
        responses::ev_response_created("resp-ephemeral"),
        responses::ev_assistant_message("msg-ephemeral", "ephemeral response"),
        responses::ev_completed("resp-ephemeral"),
    ])
}

fn session_rollout_count(home_path: &std::path::Path) -> usize {
    let sessions_dir = home_path.join("sessions");
    if !sessions_dir.exists() {
        return 0;
    }

    WalkDir::new(sessions_dir)
        .into_iter()
        .filter_map(Result::ok)
        .filter(|entry| entry.file_type().is_file())
        .filter(|entry| entry.file_name().to_string_lossy().ends_with(".jsonl"))
        .count()
}

fn first_session_rollout(home_path: &std::path::Path) -> Option<std::path::PathBuf> {
    WalkDir::new(home_path.join("sessions"))
        .into_iter()
        .filter_map(Result::ok)
        .find_map(|entry| {
            (entry.file_type().is_file() && entry.file_name().to_string_lossy().ends_with(".jsonl"))
                .then(|| entry.path().to_path_buf())
        })
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn persists_rollout_file_by_default() -> anyhow::Result<()> {
    skip_if_no_network!(Ok(()));

    let test = test_codex_exec();
    let server = MockServer::start().await;
    let _response_mock = responses::mount_sse_once(&server, exec_sse_response()).await;

    test.cmd_with_server(&server)
        .arg("--skip-git-repo-check")
        .arg("default persistence behavior")
        .assert()
        .code(0);

    assert_eq!(session_rollout_count(test.home_path()), 1);
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn does_not_persist_rollout_file_in_ephemeral_mode() -> anyhow::Result<()> {
    skip_if_no_network!(Ok(()));

    let test = test_codex_exec();
    let server = MockServer::start().await;
    let _response_mock = responses::mount_sse_once(&server, exec_sse_response()).await;

    test.cmd_with_server(&server)
        .arg("--skip-git-repo-check")
        .arg("--ephemeral")
        .arg("ephemeral behavior")
        .assert()
        .code(0);

    assert_eq!(session_rollout_count(test.home_path()), 0);
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn fork_invariant_transient_mode_persists_distinct_from_ephemeral() -> anyhow::Result<()> {
    skip_if_no_network!(Ok(()));

    let test = test_codex_exec();
    let server = MockServer::start().await;
    let response_mock = responses::mount_sse_once(&server, exec_sse_response()).await;

    let output = test
        .cmd_with_server(&server)
        .arg("--skip-git-repo-check")
        .arg("--transient")
        .arg("transient behavior")
        .output()?;
    assert!(
        output.status.success(),
        "transient run failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(response_mock.requests().len(), 1);
    assert_eq!(session_rollout_count(test.home_path()), 1);

    let rollout_path = first_session_rollout(test.home_path()).expect("transient rollout path");
    let first_line = std::fs::read_to_string(rollout_path)?;
    let metadata: serde_json::Value = serde_json::from_str(
        first_line
            .lines()
            .next()
            .expect("transient rollout should have metadata"),
    )?;
    assert_eq!(metadata["payload"]["thread_source"], "user");
    Ok(())
}
