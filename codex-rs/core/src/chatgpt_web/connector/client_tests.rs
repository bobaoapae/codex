//! FORK: tests for the connector session client's wire mapping.

use super::*;
use codex_protocol::models::FunctionCallOutputBody;
use codex_protocol::models::FunctionCallOutputContentItem;
use codex_protocol::models::FunctionCallOutputPayload;

#[test]
fn a_text_output_maps_to_a_text_result() {
    let payload = FunctionCallOutputPayload {
        body: FunctionCallOutputBody::Text("done\n".to_string()),
        success: Some(true),
    };
    let result = payload_to_result("session-1", &payload);
    assert_eq!(result.session_id, "session-1");
    assert!(!result.is_error);
    assert_eq!(result.content.len(), 1);
    assert!(matches!(
        &result.content[0],
        wire::ResultContent::Text { text } if text == "done\n"
    ));
}

#[test]
fn a_failed_output_sets_is_error() {
    let payload = FunctionCallOutputPayload {
        body: FunctionCallOutputBody::Text("boom".to_string()),
        success: Some(false),
    };
    assert!(payload_to_result("s", &payload).is_error);
}

#[test]
fn content_items_split_into_text_and_data_url_images() {
    let payload = FunctionCallOutputPayload {
        body: FunctionCallOutputBody::ContentItems(vec![
            FunctionCallOutputContentItem::InputText {
                text: "here".to_string(),
            },
            FunctionCallOutputContentItem::InputImage {
                image_url: "data:image/png;base64,AAAA".to_string(),
                detail: None,
            },
            // A non-data image URL is dropped (cannot be forwarded).
            FunctionCallOutputContentItem::InputImage {
                image_url: "https://example.com/x.png".to_string(),
                detail: None,
            },
        ]),
        success: None,
    };
    let result = payload_to_result("s", &payload);
    assert_eq!(result.content.len(), 2);
    assert!(matches!(
        &result.content[0],
        wire::ResultContent::Text { text } if text == "here"
    ));
    assert!(matches!(
        &result.content[1],
        wire::ResultContent::Image { data, mime_type } if data == "AAAA" && mime_type == "image/png"
    ));
}

#[test]
fn only_data_url_images_convert() {
    assert!(image_to_result("data:image/jpeg;base64,ZZ").is_some());
    assert!(image_to_result("https://example.com/a.png").is_none());
    assert!(image_to_result("data:image/png,notbase64").is_none());
}

// -- full round trip against an in-process daemon (no browser) ---------------

use crate::chatgpt_web::connector::BeginTurn;
use crate::chatgpt_web::connector::ConnectorBroker;
use crate::chatgpt_web::connector::contract::CallTarget;
use crate::chatgpt_web::connector::contract::ExecTool;
use crate::chatgpt_web::connector::contract::ToolSummary;
use crate::chatgpt_web::connector::daemon;
use crate::chatgpt_web::connector::daemon::state::RegistryStatus;
use crate::chatgpt_web::connector::daemon::tunnel::NoopTunnel;
use crate::chatgpt_web::connector::daemon::tunnel::TunnelEndpoint;
use std::sync::Arc;
use std::time::Duration;

async fn start_daemon(temp: &tempfile::TempDir) -> daemon::RunningDaemon {
    let mut config = daemon::DaemonRunConfig::new(
        crate::config::ChatGptWebSettings::default(),
        temp.path().to_path_buf(),
    );
    config.tunnel_override = Some(Arc::new(NoopTunnel {
        endpoint: TunnelEndpoint::Public {
            mcp_url: "https://example.trycloudflare.com/mcp/x".into(),
        },
    }));
    daemon::start(config).await.expect("daemon starts")
}

/// The session client registers, waits for `Verified`, opens a turn, and a tool
/// call the daemon delivers round-trips back to ChatGPT through the responder.
#[tokio::test]
async fn a_connector_turn_delivers_a_tool_call_and_posts_its_result() {
    let temp = tempfile::tempdir().expect("tempdir");
    let daemon = start_daemon(&temp).await;
    // Pretend the registry reconciled so `begin_turn` stops waiting.
    daemon
        .control
        .set_registry_status(RegistryStatus::Verified {
            connector_id: "asdk_app_test".into(),
            link_id: "link_test".into(),
            mcp_url: "https://example.trycloudflare.com/mcp/x".into(),
        });

    let broker = DaemonSessionBroker::connect(
        daemon.endpoint.control_url.clone(),
        daemon.endpoint.token.clone(),
        "Codex Native".into(),
    )
    .await
    .expect("broker connects");

    let mut turn = broker
        .begin_turn(BeginTurn {
            thread_id: codex_protocol::ThreadId::new(),
            turn_id: "turn-1",
            tools: vec![ToolSummary {
                name: "exec_command".into(),
                namespace: None,
                kind: crate::chatgpt_web::connector::contract::ToolKind::Function,
                description: "run".into(),
                schema: None,
            }],
            exec_tool: ExecTool::ExecCommand,
            apply_patch: false,
            ttl_ms: 60_000,
            ready_timeout: Duration::from_secs(10),
        })
        .await
        .expect("begin turn");

    // Simulate ChatGPT calling a tool: the daemon's broker enqueues it, and the
    // session's long-poll delivers it to us.
    let claim = daemon.broker.claim(&turn.turn_token).expect("claim");
    let invoke = {
        let broker = Arc::clone(&daemon.broker);
        let binding = claim.binding.clone();
        tokio::spawn(async move {
            broker
                .invoke(
                    &binding,
                    CallTarget::Function {
                        namespace: None,
                        name: "exec_command".into(),
                        arguments: serde_json::json!({"cmd": "echo hi"}),
                    },
                )
                .await
        })
    };

    let request = tokio::time::timeout(Duration::from_secs(10), turn.requests.recv())
        .await
        .expect("a tool request arrives")
        .expect("channel open");
    match &request.target {
        CallTarget::Function {
            name, arguments, ..
        } => {
            assert_eq!(name, "exec_command");
            assert_eq!(arguments["cmd"], "echo hi");
        }
        other => panic!("unexpected target {other:?}"),
    }
    request
        .respond
        .send(codex_protocol::models::FunctionCallOutputPayload {
            body: codex_protocol::models::FunctionCallOutputBody::Text("hi\n".into()),
            success: Some(true),
        })
        .expect("respond");

    let result = invoke.await.expect("join");
    assert!(!result.is_error);
    assert!(
        matches!(&result.content[0], wire::ResultContent::Text { text } if text == "hi\n"),
        "{:?}",
        result.content
    );

    broker.end_turn(&turn.turn_token, "done").await;
    drop(broker);
    daemon.shutdown().await;
}

fn probe_turn(ready_timeout: Duration) -> BeginTurn<'static> {
    BeginTurn {
        thread_id: codex_protocol::ThreadId::new(),
        turn_id: "turn-probe",
        tools: Vec::new(),
        exec_tool: ExecTool::ExecCommand,
        apply_patch: false,
        ttl_ms: 60_000,
        ready_timeout,
    }
}

/// FORK (verified live): `browser_unavailable` means the chrome-mcp extension's
/// service worker is asleep, and the daemon's next reconcile brings it back —
/// 67s, once observed. Failing the turn on the first sighting killed an agent
/// 19s into a 90s budget, so the status is now retried like any other
/// not-yet-verified one.
#[tokio::test]
async fn a_transient_browser_unavailable_is_waited_out() {
    let temp = tempfile::tempdir().expect("tempdir");
    let daemon = start_daemon(&temp).await;
    daemon
        .control
        .set_registry_status(RegistryStatus::BrowserUnavailable);

    let broker = DaemonSessionBroker::connect(
        daemon.endpoint.control_url.clone(),
        daemon.endpoint.token.clone(),
        "Codex Native".into(),
    )
    .await
    .expect("broker connects");

    let control = daemon.control.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(1_500)).await;
        control.set_registry_status(RegistryStatus::Verified {
            connector_id: "asdk_app_test".into(),
            link_id: "link_test".into(),
            mcp_url: "https://example.trycloudflare.com/mcp/x".into(),
        });
    });

    let turn = broker
        .begin_turn(probe_turn(Duration::from_secs(20)))
        .await
        .expect("the extension came back within the budget");

    broker.end_turn(&turn.turn_token, "done").await;
    drop(broker);
    daemon.shutdown().await;
}

/// An extension that never comes back still fails, with the message that names
/// what to fix — only now at the deadline instead of on the first poll.
#[tokio::test]
async fn a_browser_that_never_comes_back_fails_at_the_deadline() {
    let temp = tempfile::tempdir().expect("tempdir");
    let daemon = start_daemon(&temp).await;
    daemon
        .control
        .set_registry_status(RegistryStatus::BrowserUnavailable);

    let broker = DaemonSessionBroker::connect(
        daemon.endpoint.control_url.clone(),
        daemon.endpoint.token.clone(),
        "Codex Native".into(),
    )
    .await
    .expect("broker connects");

    let started = std::time::Instant::now();
    let error = match broker.begin_turn(probe_turn(Duration::from_secs(2))).await {
        Ok(_) => panic!("the turn must not start while the extension is away"),
        Err(error) => error,
    };
    assert!(error.contains("chrome-mcp"), "{error}");
    assert!(
        started.elapsed() >= Duration::from_secs(2),
        "gave up after {:?}",
        started.elapsed()
    );

    drop(broker);
    daemon.shutdown().await;
}

/// FORK: a failure the user has to fix should not burn the turn's whole budget.
/// Before this, a tunnel the ChatGPT account could not see cost every consultant
/// turn the full 90s before failing with "not ready within 90s".
#[tokio::test]
async fn a_terminal_registry_failure_fails_the_turn_at_once() {
    let temp = tempfile::tempdir().expect("tempdir");
    let daemon = start_daemon(&temp).await;
    daemon.control.set_registry_status(RegistryStatus::Failed {
        reason: "tunnel `tunnel_abc` is not visible to the ChatGPT account logged in Chrome"
            .into(),
        retry_at_ms: now_ms() + 300_000,
        kind: FailureKind::TunnelNotVisible,
        parked: false,
    });

    let broker = DaemonSessionBroker::connect(
        daemon.endpoint.control_url.clone(),
        daemon.endpoint.token.clone(),
        "Codex Native".into(),
    )
    .await
    .expect("broker connects");

    let started = std::time::Instant::now();
    let error = match broker.begin_turn(probe_turn(Duration::from_secs(90))).await {
        Ok(_) => panic!("a terminal registry failure must fail the turn"),
        Err(error) => error,
    };

    assert!(
        started.elapsed() < Duration::from_secs(20),
        "took {:?}, which is most of the budget",
        started.elapsed()
    );
    assert!(error.contains("not visible"), "{error}");
    assert!(error.contains("tunnel_not_visible"), "{error}");
    assert!(error.contains("registry reconcile"), "{error}");

    drop(broker);
    daemon.shutdown().await;
}

/// A transient failure is still waited out — it is exactly the kind the daemon
/// may well fix on its next attempt.
#[tokio::test]
async fn a_transient_registry_failure_is_waited_out() {
    let temp = tempfile::tempdir().expect("tempdir");
    let daemon = start_daemon(&temp).await;
    daemon.control.set_registry_status(RegistryStatus::Failed {
        reason: "HTTP 500: boom".into(),
        retry_at_ms: now_ms() + 1_000,
        kind: FailureKind::Transient,
        parked: false,
    });

    let broker = DaemonSessionBroker::connect(
        daemon.endpoint.control_url.clone(),
        daemon.endpoint.token.clone(),
        "Codex Native".into(),
    )
    .await
    .expect("broker connects");

    let control = daemon.control.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(1_500)).await;
        control.set_registry_status(RegistryStatus::Verified {
            connector_id: "asdk_app_test".into(),
            link_id: "link_test".into(),
            mcp_url: "https://example.trycloudflare.com/mcp/x".into(),
        });
    });

    let turn = broker
        .begin_turn(probe_turn(Duration::from_secs(20)))
        .await
        .expect("a transient failure resolves within the budget");

    broker.end_turn(&turn.turn_token, "done").await;
    drop(broker);
    daemon.shutdown().await;
}

/// FORK: `/healthz` carries the failure detail, not only its label — that is
/// what lets the turn-side gate decide whether waiting is worth anything.
#[tokio::test]
async fn healthz_carries_the_registry_failure() {
    let temp = tempfile::tempdir().expect("tempdir");
    let daemon = start_daemon(&temp).await;
    let retry_at_ms = now_ms() + 300_000;
    daemon.control.set_registry_status(RegistryStatus::Failed {
        reason: "tunnel `tunnel_abc` is not visible".into(),
        retry_at_ms,
        kind: FailureKind::TunnelNotVisible,
        parked: true,
    });

    let health = daemon.control.health();

    assert_eq!(health.registry_status, "failed");
    assert_eq!(
        health.registry_failure_kind.as_deref(),
        Some("tunnel_not_visible")
    );
    assert_eq!(
        health.registry_reason.as_deref(),
        Some("tunnel `tunnel_abc` is not visible")
    );
    assert_eq!(health.registry_retry_at_ms, Some(retry_at_ms));
    assert!(health.registry_parked);

    // A healthy registry carries none of it.
    daemon
        .control
        .set_registry_status(RegistryStatus::Verified {
            connector_id: "asdk_app_test".into(),
            link_id: "link_test".into(),
            mcp_url: "https://example.trycloudflare.com/mcp/x".into(),
        });
    let health = daemon.control.health();
    assert_eq!(health.registry_reason, None);
    assert_eq!(health.registry_failure_kind, None);
    assert!(!health.registry_parked);

    daemon.shutdown().await;
}

/// FORK: the refresh route is the turn's synchronous ask. It is behind the
/// bearer like every other control route, and answers with the resulting
/// status — 501 on a build with no registry, which is what makes it safe for
/// `wait_verified` to fall through to its poll.
#[tokio::test]
async fn the_refresh_route_needs_the_bearer_and_answers_the_resulting_status() {
    let temp = tempfile::tempdir().expect("tempdir");
    let daemon = start_daemon(&temp).await;
    let url = format!("{}/v1/registry/refresh", daemon.endpoint.control_url);
    let http = reqwest::Client::new();

    let unauthorized = http.post(&url).send().await.expect("request");
    assert_eq!(unauthorized.status(), reqwest::StatusCode::UNAUTHORIZED);

    let no_registry = http
        .post(&url)
        .bearer_auth(&daemon.endpoint.token)
        .send()
        .await
        .expect("request");
    assert_eq!(no_registry.status(), reqwest::StatusCode::NOT_IMPLEMENTED);

    daemon.shutdown().await;
}

/// FORK: a starting turn asks the daemon to reconcile rather than polling a
/// status its own backoff may not revisit for half an hour. The trigger is
/// `Turn`, the only one allowed to override a parked registry.
#[tokio::test]
async fn begin_turn_asks_the_daemon_to_refresh_the_registry_first() {
    use crate::chatgpt_web::connector::daemon::control::ReconcileHook;
    use crate::chatgpt_web::connector::daemon::control::ReconcileTrigger;
    use std::sync::atomic::AtomicUsize;
    use std::sync::atomic::Ordering;

    let temp = tempfile::tempdir().expect("tempdir");
    let refreshes = Arc::new(AtomicUsize::new(0));
    let hook_refreshes = Arc::clone(&refreshes);
    let hook: ReconcileHook = Arc::new(move |trigger| {
        assert_eq!(trigger, ReconcileTrigger::Turn);
        let hook_refreshes = Arc::clone(&hook_refreshes);
        Box::pin(async move {
            hook_refreshes.fetch_add(1, Ordering::SeqCst);
            Ok(RegistryStatus::Verified {
                connector_id: "asdk_app_test".into(),
                link_id: "link_test".into(),
                mcp_url: "https://example.trycloudflare.com/mcp/x".into(),
            })
        })
    });

    let mut config = daemon::DaemonRunConfig::new(
        crate::config::ChatGptWebSettings::default(),
        temp.path().to_path_buf(),
    );
    config.tunnel_override = Some(Arc::new(NoopTunnel {
        endpoint: TunnelEndpoint::Public {
            mcp_url: "https://example.trycloudflare.com/mcp/x".into(),
        },
    }));
    config.reconcile = Some(hook);
    let daemon = daemon::start(config).await.expect("daemon starts");

    let broker = DaemonSessionBroker::connect(
        daemon.endpoint.control_url.clone(),
        daemon.endpoint.token.clone(),
        "Codex Native".into(),
    )
    .await
    .expect("broker connects");

    // The registry has never been reconciled; the turn's own refresh is what
    // brings it to `Verified`.
    let turn = broker
        .begin_turn(probe_turn(Duration::from_secs(20)))
        .await
        .expect("the refresh verified the connector");
    assert!(
        refreshes.load(Ordering::SeqCst) >= 1,
        "begin_turn did not ask for a refresh"
    );

    broker.end_turn(&turn.turn_token, "done").await;
    drop(broker);
    daemon.shutdown().await;
}
