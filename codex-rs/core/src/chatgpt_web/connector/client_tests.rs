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
