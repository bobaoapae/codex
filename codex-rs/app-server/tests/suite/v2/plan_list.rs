//! FORK extension: `plan/list` and `plan/read` over fixtures written directly into
//! `$CODEX_HOME/plans/`, so pagination and validation are covered without running a turn.

#![allow(clippy::unwrap_used)]

use std::time::Duration;

use anyhow::Result;
use app_test_support::TestAppServer;
use codex_app_server_protocol::ClientRequest;
use codex_app_server_protocol::PlanApproveParams;
use codex_app_server_protocol::PlanApproveResponse;
use codex_app_server_protocol::PlanLifecycle;
use codex_app_server_protocol::PlanListParams;
use codex_app_server_protocol::PlanListResponse;
use codex_app_server_protocol::PlanReadParams;
use codex_app_server_protocol::PlanReadResponse;
use codex_app_server_protocol::RequestId;
use pretty_assertions::assert_eq;
use tempfile::TempDir;
use tokio::time::timeout;

const DEFAULT_TIMEOUT: Duration = Duration::from_secs(60);

fn write_plan(codex_home: &TempDir, name: &str, title: &str, updated_at: &str, body: &str) {
    let dir = codex_home.path().join("plans");
    std::fs::create_dir_all(&dir).expect("create plans dir");
    let document = format!(
        "---\ntitle: {title}\nthread_id: thread-{name}\ncreated_at: 2026-08-27T09:00:00Z\nupdated_at: {updated_at}\nrevision: 1\n---\n\n{body}"
    );
    std::fs::write(dir.join(format!("{name}.md")), document).expect("write plan fixture");
}

fn write_approved_plan(codex_home: &TempDir, name: &str, revision: u32, title: &str, body: &str) {
    let dir = codex_home.path().join("plans").join("approved").join(name);
    std::fs::create_dir_all(&dir).expect("create approved plans dir");
    let document = format!(
        "---\ntitle: {title}\nthread_id: thread-{name}\ncreated_at: 2026-08-27T09:00:00Z\nupdated_at: 2026-08-27T12:00:00Z\napproved_at: 2026-08-27T12:00:00Z\nrevision: {revision}\n---\n\n{body}"
    );
    std::fs::write(dir.join(format!("{revision}.md")), document)
        .expect("write approved plan fixture");
}

async fn start_server(codex_home: &TempDir) -> Result<TestAppServer> {
    let mut mcp = TestAppServer::builder()
        .with_codex_home(codex_home.path())
        .without_auto_env()
        .build()
        .await?;
    timeout(DEFAULT_TIMEOUT, mcp.initialize()).await??;
    Ok(mcp)
}

#[tokio::test]
async fn plan_list_returns_newest_first_and_paginates() -> Result<()> {
    let codex_home = TempDir::new()?;
    write_plan(
        &codex_home,
        "older",
        "Older plan",
        "2026-08-27T10:00:00Z",
        "# Older plan\n",
    );
    write_plan(
        &codex_home,
        "newer",
        "Newer plan",
        "2026-08-27T12:00:00Z",
        "# Newer plan\n",
    );

    let mut mcp = start_server(&codex_home).await?;

    let all: PlanListResponse = mcp
        .request(|request_id| ClientRequest::PlanList {
            request_id,
            params: PlanListParams::default(),
        })
        .await?;
    assert_eq!(
        all.data
            .iter()
            .map(|plan| plan.title.as_str())
            .collect::<Vec<_>>(),
        vec!["Newer plan", "Older plan"]
    );
    assert_eq!(all.next_cursor, None);

    let first_page: PlanListResponse = mcp
        .request(|request_id| ClientRequest::PlanList {
            request_id,
            params: PlanListParams {
                cursor: None,
                limit: Some(1),
            },
        })
        .await?;
    assert_eq!(first_page.data.len(), 1);
    assert_eq!(first_page.data[0].title, "Newer plan");
    let cursor = first_page
        .next_cursor
        .clone()
        .expect("first page should carry a cursor");
    assert_ne!(cursor, first_page.data[0].id);

    let second_page: PlanListResponse = mcp
        .request(|request_id| ClientRequest::PlanList {
            request_id,
            params: PlanListParams {
                cursor: Some(cursor),
                limit: Some(1),
            },
        })
        .await?;
    assert_eq!(second_page.data.len(), 1);
    assert_eq!(second_page.data[0].title, "Older plan");
    assert_eq!(second_page.next_cursor, None);

    Ok(())
}

#[tokio::test]
async fn plan_list_skips_files_without_front_matter() -> Result<()> {
    let codex_home = TempDir::new()?;
    write_plan(
        &codex_home,
        "valid",
        "Valid plan",
        "2026-08-27T10:00:00Z",
        "# Valid plan\n",
    );
    std::fs::write(
        codex_home.path().join("plans").join("bare.md"),
        "# no front matter\n",
    )?;

    let mut mcp = start_server(&codex_home).await?;

    let listed: PlanListResponse = mcp
        .request(|request_id| ClientRequest::PlanList {
            request_id,
            params: PlanListParams::default(),
        })
        .await?;
    assert_eq!(listed.data.len(), 1);
    assert_eq!(listed.data[0].title, "Valid plan");

    Ok(())
}

#[tokio::test]
async fn plan_list_returns_lifecycle_for_draft_and_approved_revisions() -> Result<()> {
    let codex_home = TempDir::new()?;
    write_plan(
        &codex_home,
        "same",
        "Same plan",
        "2026-08-27T12:00:00Z",
        "# Same plan\n",
    );
    write_approved_plan(&codex_home, "same.md", 1, "Same plan", "# Same plan\n");

    let mut mcp = start_server(&codex_home).await?;
    let listed: PlanListResponse = mcp
        .request(|request_id| ClientRequest::PlanList {
            request_id,
            params: PlanListParams::default(),
        })
        .await?;
    assert_eq!(listed.data.len(), 2);
    assert!(listed.data.iter().any(|plan| {
        plan.id == "same.md" && plan.revision == 1 && plan.lifecycle == PlanLifecycle::Draft
    }));
    assert!(listed.data.iter().any(|plan| {
        plan.id == "same.md" && plan.revision == 1 && plan.lifecycle == PlanLifecycle::Approved
    }));
    Ok(())
}

#[tokio::test]
async fn plan_read_accepts_an_approved_revision() -> Result<()> {
    let codex_home = TempDir::new()?;
    write_approved_plan(
        &codex_home,
        "approved.md",
        3,
        "Approved plan",
        "# Approved plan\n- immutable\n",
    );

    let mut mcp = start_server(&codex_home).await?;
    let read: PlanReadResponse = mcp
        .request(|request_id| ClientRequest::PlanRead {
            request_id,
            params: PlanReadParams {
                id: "approved.md".to_string(),
                revision: Some(3),
            },
        })
        .await?;
    assert_eq!(read.plan.lifecycle, PlanLifecycle::Approved);
    assert_eq!(read.plan.revision, 3);
    assert_eq!(read.markdown, "# Approved plan\n- immutable\n");
    Ok(())
}

#[tokio::test]
async fn plan_list_rejects_unknown_cursor() -> Result<()> {
    let codex_home = TempDir::new()?;
    write_plan(
        &codex_home,
        "only",
        "Only plan",
        "2026-08-27T10:00:00Z",
        "# Only plan\n",
    );

    let mut mcp = start_server(&codex_home).await?;
    let request_id = mcp
        .send_request(
            "plan/list",
            Some(serde_json::to_value(PlanListParams {
                cursor: Some("unknown-cursor".to_string()),
                limit: Some(1),
            })?),
        )
        .await?;
    let error = timeout(
        DEFAULT_TIMEOUT,
        mcp.read_stream_until_error_message(RequestId::Integer(request_id)),
    )
    .await??;
    assert_eq!(error.error.code, -32600);
    Ok(())
}

#[tokio::test]
async fn fork_invariant_plan_approve_uses_cas_and_is_idempotent() -> Result<()> {
    let codex_home = TempDir::new()?;
    write_plan(
        &codex_home,
        "cas",
        "CAS plan",
        "2026-08-27T10:00:00Z",
        "# CAS plan\n- step\n",
    );

    let mut mcp = start_server(&codex_home).await?;
    let stale_id = mcp
        .send_request(
            "plan/approve",
            Some(serde_json::to_value(PlanApproveParams {
                id: "cas.md".to_string(),
                expected_revision: 2,
            })?),
        )
        .await?;
    let stale = timeout(
        DEFAULT_TIMEOUT,
        mcp.read_stream_until_error_message(RequestId::Integer(stale_id)),
    )
    .await??;
    assert_eq!(stale.error.code, -32600);

    let first: PlanApproveResponse = mcp
        .request(|request_id| ClientRequest::PlanApprove {
            request_id,
            params: PlanApproveParams {
                id: "cas.md".to_string(),
                expected_revision: 1,
            },
        })
        .await?;
    let second: PlanApproveResponse = mcp
        .request(|request_id| ClientRequest::PlanApprove {
            request_id,
            params: PlanApproveParams {
                id: "cas.md".to_string(),
                expected_revision: 1,
            },
        })
        .await?;
    assert_eq!(first.approved_plan, second.approved_plan);
    assert_eq!(first.plan.lifecycle, PlanLifecycle::Approved);
    assert_eq!(first.approved_plan.id, "cas.md");
    assert_eq!(first.approved_plan.revision, 1);
    Ok(())
}

#[tokio::test]
async fn plan_read_returns_the_markdown_body() -> Result<()> {
    let codex_home = TempDir::new()?;
    write_plan(
        &codex_home,
        "only",
        "Only plan",
        "2026-08-27T10:00:00Z",
        "# Only plan\n- step\n",
    );

    let mut mcp = start_server(&codex_home).await?;

    let read: PlanReadResponse = mcp
        .request(|request_id| ClientRequest::PlanRead {
            request_id,
            params: PlanReadParams {
                id: "only.md".to_string(),
                revision: None,
            },
        })
        .await?;
    assert_eq!(read.plan.id, "only.md");
    assert_eq!(read.plan.title, "Only plan");
    assert_eq!(read.markdown, "# Only plan\n- step\n");
    assert!(read.plan.path.ends_with("only.md"));

    Ok(())
}

#[tokio::test]
async fn plan_read_rejects_unknown_and_unsafe_ids() -> Result<()> {
    let codex_home = TempDir::new()?;
    write_plan(
        &codex_home,
        "only",
        "Only plan",
        "2026-08-27T10:00:00Z",
        "# Only plan\n",
    );

    let mut mcp = start_server(&codex_home).await?;

    for id in ["../escape.md", "missing.md"] {
        let request_id = mcp
            .send_request(
                "plan/read",
                Some(serde_json::to_value(PlanReadParams {
                    id: id.to_string(),
                    revision: None,
                })?),
            )
            .await?;
        let error = timeout(
            DEFAULT_TIMEOUT,
            mcp.read_stream_until_error_message(RequestId::Integer(request_id)),
        )
        .await??;
        assert_eq!(error.error.code, -32602, "id {id:?} should be rejected");
    }

    Ok(())
}
