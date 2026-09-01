use anyhow::Result;
use chrono::Utc;
use codex_app_server_protocol::ArtifactReadParams;
use codex_app_server_protocol::ClientResponsePayload;
use codex_protocol::ThreadId;
use codex_protocol::protocol::SessionSource;
use codex_state::SqliteConfig;
use codex_state::StateRuntime;
use codex_state::ThreadArtifactAttachmentOutcome;
use codex_state::ThreadMetadataBuilder;
use codex_utils_absolute_path::test_support::PathExt;
use pretty_assertions::assert_eq;
use serde_json::json;
use std::sync::Arc;
use tempfile::TempDir;

use super::ArtifactRequestProcessor;

async fn runtime_with_thread() -> Result<(Arc<StateRuntime>, ThreadId, TempDir)> {
    let home = TempDir::new()?;
    let thread_id = ThreadId::from_u128(1);
    let sqlite = SqliteConfig::new_for_testing(home.path().abs());
    let runtime = StateRuntime::init(sqlite, "test-provider".to_string()).await?;
    let mut builder = ThreadMetadataBuilder::new(
        thread_id,
        home.path().join("rollout.jsonl"),
        Utc::now(),
        SessionSource::Cli,
    );
    builder.cwd = home.path().to_path_buf();
    builder.model_provider = Some("test-provider".to_string());
    runtime
        .upsert_thread(&builder.build("test-provider"))
        .await?;
    Ok((runtime, thread_id, home))
}

async fn read(
    processor: &ArtifactRequestProcessor,
    artifact_id: &str,
    cursor: Option<String>,
    limit: u32,
) -> Result<codex_app_server_protocol::ArtifactReadResponse> {
    let response = processor
        .artifact_read(ArtifactReadParams {
            artifact_id: artifact_id.to_string(),
            cursor,
            limit: Some(limit),
        })
        .await
        .map_err(|error| anyhow::anyhow!(error.message))?;
    let Some(ClientResponsePayload::ArtifactRead(response)) = response else {
        anyhow::bail!("expected artifact/read response");
    };
    Ok(response)
}

#[tokio::test]
async fn artifact_read_returns_metadata_and_reassembles_utf8_pages() -> Result<()> {
    let (runtime, thread_id, _home) = runtime_with_thread().await?;
    let payload = json!({
        "message": "olá 🌊 — artifact payload",
        "values": [1, 2, 3],
    });
    let outcome = runtime
        .attach_thread_artifact(thread_id, "test.result", "read-1", payload.clone())
        .await?;
    let ThreadArtifactAttachmentOutcome::Created(artifact) = outcome else {
        anyhow::bail!("expected a newly created artifact");
    };
    let processor = ArtifactRequestProcessor::new(Some(runtime.clone()));
    let expected = serde_json::to_string(&payload)?;
    let mut cursor = None;
    let mut combined = String::new();
    loop {
        let page = read(&processor, &artifact.id, cursor, 7).await?;
        assert_eq!(page.artifact.artifact_id, artifact.id);
        assert_eq!(page.artifact.thread_id, thread_id.to_string());
        assert_eq!(page.artifact.artifact_type, "test.result");
        assert_eq!(page.artifact.identity_key, "read-1");
        assert!(page.chunk.len() <= 7);
        combined.push_str(&page.chunk);
        let Some(next_cursor) = page.next_cursor else {
            break;
        };
        cursor = Some(next_cursor);
    }
    assert_eq!(combined, expected);
    runtime.close().await;
    Ok(())
}

#[tokio::test]
async fn artifact_read_rejects_stale_or_unknown_ids_without_path_authority() -> Result<()> {
    let (runtime, thread_id, _home) = runtime_with_thread().await?;
    let first = runtime
        .attach_thread_artifact(thread_id, "test.result", "first", json!({"safe": true}))
        .await?;
    let second = runtime
        .attach_thread_artifact(thread_id, "test.result", "second", json!({"safe": false}))
        .await?;
    let ThreadArtifactAttachmentOutcome::Created(first) = first else {
        anyhow::bail!("expected first artifact");
    };
    let ThreadArtifactAttachmentOutcome::Created(second) = second else {
        anyhow::bail!("expected second artifact");
    };
    let processor = ArtifactRequestProcessor::new(Some(runtime.clone()));
    let first_page = read(&processor, &first.id, None, 1).await?;
    let cursor = first_page
        .next_cursor
        .expect("first page should have cursor");
    let stale = processor
        .artifact_read(ArtifactReadParams {
            artifact_id: second.id,
            cursor: Some(cursor),
            limit: Some(1),
        })
        .await
        .expect_err("cross-artifact cursor must fail");
    assert!(stale.message.contains("invalid or stale"));
    assert!(!stale.message.contains("rollout"));

    let unknown = processor
        .artifact_read(ArtifactReadParams {
            artifact_id: "not-an-artifact".to_string(),
            cursor: None,
            limit: None,
        })
        .await
        .expect_err("unknown artifact must fail");
    assert_eq!(unknown.message, "artifact not found");
    runtime.close().await;
    Ok(())
}

#[tokio::test]
async fn artifact_read_caps_oversized_chunk_requests_at_64_kib() -> Result<()> {
    let (runtime, thread_id, _home) = runtime_with_thread().await?;
    let payload = json!({"data": "x".repeat(60_000)});
    let outcome = runtime
        .attach_thread_artifact(thread_id, "test.result", "large", payload)
        .await?;
    let ThreadArtifactAttachmentOutcome::Created(artifact) = outcome else {
        anyhow::bail!("expected a large artifact");
    };
    let processor = ArtifactRequestProcessor::new(Some(runtime.clone()));
    let page = read(&processor, &artifact.id, None, u32::MAX).await?;
    assert!(page.total_bytes <= 64 * 1024);
    assert!(page.chunk.len() <= 64 * 1024);
    runtime.close().await;
    Ok(())
}

#[tokio::test]
async fn artifact_read_requires_local_state() {
    let processor = ArtifactRequestProcessor::new(None);
    let error = processor
        .artifact_read(ArtifactReadParams {
            artifact_id: "artifact-1".to_string(),
            cursor: None,
            limit: None,
        })
        .await
        .expect_err("remote/absent state must be rejected");
    assert!(error.message.contains("local SQLite state"));
}
