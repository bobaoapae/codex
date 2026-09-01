use anyhow::Result;
use app_test_support::TestAppServer;
use app_test_support::create_fake_paginated_rollout;
use codex_app_server_protocol::ClientInfo;
use codex_app_server_protocol::ClientRequest;
use codex_app_server_protocol::ContextInspectParams;
use codex_app_server_protocol::ContextInspectResponse;
use codex_app_server_protocol::ContextSnapshotKind;
use codex_app_server_protocol::InitializeCapabilities;
use codex_app_server_protocol::RequestId;
use codex_app_server_protocol::ThreadLoadedListResponse;
use codex_app_server_protocol::ThreadStartParams;
use codex_app_server_protocol::ThreadStartResponse;
use pretty_assertions::assert_eq;
use std::collections::BTreeMap;
use std::fs;
use std::path::Path;
use std::path::PathBuf;
use std::time::Duration;
use tempfile::TempDir;
use tokio::time::timeout;

const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

#[tokio::test]
async fn fork_invariant_context_inspect_requires_experimental_api_capability() -> Result<()> {
    let codex_home = TempDir::new()?;
    let mut server = TestAppServer::builder()
        .with_codex_home(codex_home.path())
        .without_auto_env()
        .build()
        .await?;
    server
        .initialize_with_capabilities(
            ClientInfo {
                name: "context-inspect-test".to_string(),
                title: None,
                version: "0.1.0".to_string(),
            },
            Some(InitializeCapabilities {
                experimental_api: false,
                ..Default::default()
            }),
        )
        .await?;

    let request_id = server
        .send_request(
            "context/inspect",
            Some(serde_json::json!({"threadId": "not-a-thread"})),
        )
        .await?;
    let error = timeout(
        REQUEST_TIMEOUT,
        server.read_stream_until_error_message(RequestId::Integer(request_id)),
    )
    .await??;
    assert_eq!(error.error.code, -32600);
    assert_eq!(
        error.error.message,
        "context/inspect requires experimentalApi capability"
    );
    Ok(())
}

#[tokio::test]
async fn loaded_context_inspect_omits_previews_by_default() -> Result<()> {
    let codex_home = TempDir::new()?;
    let mut server = TestAppServer::builder()
        .with_codex_home(codex_home.path())
        .without_auto_env()
        .build_initialized()
        .await?;

    let request_id = server
        .send_thread_start_request(ThreadStartParams {
            model: Some("mock-model".to_string()),
            ..Default::default()
        })
        .await?;
    let started: ThreadStartResponse =
        timeout(REQUEST_TIMEOUT, server.read_response(request_id)).await??;

    let inspected: ContextInspectResponse = server
        .request(|request_id| ClientRequest::ContextInspect {
            request_id,
            params: ContextInspectParams {
                thread_id: started.thread.id.clone(),
                include_preview: false,
            },
        })
        .await?;

    assert_eq!(inspected.context.thread_id, started.thread.id);
    assert_eq!(
        inspected.context.snapshot_kind,
        ContextSnapshotKind::Speculative
    );
    assert!(inspected.context.partial);
    assert!(
        inspected
            .context
            .items
            .iter()
            .all(|item| item.preview.is_none())
    );
    assert!(inspected.context.base_instructions.preview.is_none());
    Ok(())
}

#[tokio::test]
async fn unloaded_context_inspect_is_cold_without_loading_or_writing() -> Result<()> {
    let codex_home = TempDir::new()?;
    let thread_id = create_fake_paginated_rollout(
        codex_home.path(),
        "2026-08-31T00-00-00",
        "2026-08-31T00:00:00Z",
        "stored prompt",
        Some("openai"),
        None,
    )?;

    let mut server = TestAppServer::builder()
        .with_codex_home(codex_home.path())
        .without_auto_env()
        .build_initialized()
        .await?;
    let loaded_before_id = server
        .send_request("thread/loaded/list", Some(serde_json::json!({})))
        .await?;
    let loaded_before: ThreadLoadedListResponse =
        timeout(REQUEST_TIMEOUT, server.read_response(loaded_before_id)).await??;
    assert!(loaded_before.data.is_empty());
    let files_before = snapshot_files(codex_home.path())?;

    let inspected: ContextInspectResponse = server
        .request(|request_id| ClientRequest::ContextInspect {
            request_id,
            params: ContextInspectParams {
                thread_id: thread_id.clone(),
                include_preview: true,
            },
        })
        .await?;

    assert_eq!(inspected.context.thread_id, thread_id);
    assert_eq!(inspected.context.snapshot_kind, ContextSnapshotKind::Cold);
    assert!(inspected.context.partial);
    assert_eq!(snapshot_files(codex_home.path())?, files_before);

    let loaded_after_id = server
        .send_request("thread/loaded/list", Some(serde_json::json!({})))
        .await?;
    let loaded_after: ThreadLoadedListResponse =
        timeout(REQUEST_TIMEOUT, server.read_response(loaded_after_id)).await??;
    assert!(loaded_after.data.is_empty());
    Ok(())
}

fn snapshot_files(root: &Path) -> Result<BTreeMap<PathBuf, (u64, std::time::SystemTime)>> {
    fn visit(
        root: &Path,
        path: &Path,
        files: &mut BTreeMap<PathBuf, (u64, std::time::SystemTime)>,
    ) -> Result<()> {
        for entry in fs::read_dir(path)? {
            let entry = entry?;
            let path = entry.path();
            if path.is_dir() {
                visit(root, &path, files)?;
            } else if path.is_file() {
                let metadata = fs::metadata(&path)?;
                files.insert(
                    path.strip_prefix(root)?.to_path_buf(),
                    (metadata.len(), metadata.modified()?),
                );
            }
        }
        Ok(())
    }

    let mut files = BTreeMap::new();
    visit(root, root, &mut files)?;
    Ok(files)
}
