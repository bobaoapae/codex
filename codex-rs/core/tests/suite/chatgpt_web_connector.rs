//! FORK: the chatgpt_web connector daemon, driven the way ChatGPT drives it.
//!
//! An in-process daemon with a `NoopTunnel`; `codex_rmcp_client` plays the
//! ChatGPT MCP client, and a plain HTTP client plays a Codex session on the
//! loopback control API.

use codex_config::types::AuthKeyringBackendKind;
use codex_config::types::OAuthCredentialsStoreMode;
use codex_core::chatgpt_web_daemon;
use codex_core::chatgpt_web_daemon::contract;
use codex_core::chatgpt_web_daemon::tunnel;
use codex_core::chatgpt_web_daemon::wire;
use codex_core::config::ChatGptWebSettings;
use codex_exec_server::HttpClient;
use codex_exec_server::RouteAwareHttpClient;
use codex_http_client::HttpClientFactory;
use codex_http_client::OutboundProxyPolicy;
use codex_rmcp_client::ElicitationAction;
use codex_rmcp_client::ElicitationResponse;
use codex_rmcp_client::RmcpClient;
use codex_rmcp_client::SendElicitation;
use futures::FutureExt;
use pretty_assertions::assert_eq;
use rmcp::model::ClientCapabilities;
use rmcp::model::Implementation;
use rmcp::model::InitializeRequestParams;
use rmcp::model::ProtocolVersion;
use serde_json::Value;
use serde_json::json;
use std::sync::Arc;
use std::time::Duration;

const TOKEN: &str = "turn_0123456789abcdef0123456789abcdef";

async fn start_daemon(temp: &tempfile::TempDir) -> chatgpt_web_daemon::RunningDaemon {
    let mut config = chatgpt_web_daemon::DaemonRunConfig::new(
        ChatGptWebSettings::default(),
        temp.path().to_path_buf(),
    );
    config.tunnel_override = Some(Arc::new(tunnel::NoopTunnel {
        endpoint: tunnel::TunnelEndpoint::Public {
            mcp_url: "https://example.trycloudflare.com/mcp/x".into(),
        },
    }));
    chatgpt_web_daemon::start(config)
        .await
        .expect("daemon starts")
}

fn http_client() -> Arc<dyn HttpClient> {
    Arc::new(
        RouteAwareHttpClient::new(HttpClientFactory::new(OutboundProxyPolicy::ReqwestDefault))
            .with_tls_backend_fallback(),
    )
}

async fn chatgpt_client(mcp_url: &str) -> RmcpClient {
    let client = RmcpClient::new_streamable_http_client(
        "chatgpt",
        mcp_url,
        None,
        None,
        None,
        OAuthCredentialsStoreMode::File,
        AuthKeyringBackendKind::default(),
        http_client(),
        None,
    )
    .await
    .expect("client");
    let decline: SendElicitation = Box::new(|_, _| {
        async {
            Ok(ElicitationResponse {
                action: ElicitationAction::Decline,
                content: None,
                meta: None,
            })
        }
        .boxed()
    });
    client
        .initialize(
            InitializeRequestParams::new(
                ClientCapabilities::default(),
                Implementation::new("openai-mcp", "1.0.0"),
            )
            .with_protocol_version(ProtocolVersion::V_2025_06_18),
            Some(Duration::from_secs(10)),
            decline,
        )
        .await
        .expect("initialize");
    client
}

struct FakeSession {
    http: reqwest::Client,
    base: String,
    token: String,
    id: String,
}

impl FakeSession {
    async fn register(daemon: &chatgpt_web_daemon::RunningDaemon, id: &str) -> Self {
        let session = Self {
            http: reqwest::Client::new(),
            base: daemon.endpoint.control_url.clone(),
            token: daemon.endpoint.token.clone(),
            id: id.to_string(),
        };
        let response = session
            .http
            .post(format!("{}/v1/sessions", session.base))
            .bearer_auth(&session.token)
            .json(&wire::RegisterSessionRequest {
                codex_pid: std::process::id(),
                session_id: id.to_string(),
                codex_version: "test".into(),
            })
            .send()
            .await
            .expect("register");
        assert_eq!(response.status(), 200);
        session
    }

    async fn register_turn(&self, turn_token: &str, apply_patch: bool) {
        let response = self
            .http
            .post(format!("{}/v1/turns", self.base))
            .bearer_auth(&self.token)
            .json(&wire::RegisterTurnRequest {
                session_id: self.id.clone(),
                turn_token: turn_token.into(),
                thread_id: "thread-1".into(),
                turn_id: "turn-1".into(),
                ttl_ms: 60_000,
                tools: vec![
                    contract::ToolSummary {
                        name: "exec_command".into(),
                        namespace: None,
                        kind: contract::ToolKind::Function,
                        description: "Runs a command".into(),
                        schema: Some(json!({"type": "object"})),
                    },
                    contract::ToolSummary {
                        name: "apply_patch".into(),
                        namespace: None,
                        kind: contract::ToolKind::Freeform,
                        description: "Applies a patch".into(),
                        schema: None,
                    },
                ],
                exec_tool: contract::ExecTool::ExecCommand,
                apply_patch,
            })
            .send()
            .await
            .expect("register turn");
        assert_eq!(
            response.status(),
            200,
            "{}",
            response.text().await.unwrap_or_default()
        );
    }

    async fn poll(&self, after: u64, wait_ms: u64) -> wire::CallsResponse {
        self.http
            .get(format!(
                "{}/v1/sessions/{}/calls?after={after}&wait_ms={wait_ms}",
                self.base, self.id
            ))
            .bearer_auth(&self.token)
            .send()
            .await
            .expect("poll")
            .json()
            .await
            .expect("json")
    }

    async fn complete(&self, call_id: &str, text: &str) -> reqwest::StatusCode {
        self.http
            .post(format!("{}/v1/calls/{call_id}/result", self.base))
            .bearer_auth(&self.token)
            .json(&wire::CallResultRequest {
                session_id: self.id.clone(),
                content: vec![wire::ResultContent::Text { text: text.into() }],
                is_error: false,
                structured: Some(json!({"exit_code": 0})),
            })
            .send()
            .await
            .expect("result")
            .status()
    }

    async fn end_turn(&self, turn_token: &str) {
        let response = self
            .http
            .delete(format!("{}/v1/turns/{turn_token}", self.base))
            .bearer_auth(&self.token)
            .send()
            .await
            .expect("end turn");
        assert_eq!(response.status(), 200);
    }

    async fn disconnect(&self) {
        let response = self
            .http
            .delete(format!("{}/v1/sessions/{}", self.base, self.id))
            .bearer_auth(&self.token)
            .send()
            .await
            .expect("disconnect");
        assert_eq!(response.status(), 200);
    }
}

fn text_of(result: &rmcp::model::CallToolResult) -> String {
    result
        .content
        .iter()
        .filter_map(|block| block.as_text().map(|text| text.text.clone()))
        .collect::<Vec<_>>()
        .join("\n")
}

#[tokio::test]
async fn chatgpt_sees_the_fixed_contract_after_initialize() {
    let temp = tempfile::tempdir().expect("tempdir");
    let daemon = start_daemon(&temp).await;
    let client = chatgpt_client(&daemon.local_mcp_url()).await;

    let tools = client
        .list_tools(None, Some(Duration::from_secs(10)))
        .await
        .expect("tools");
    let names: Vec<&str> = tools.tools.iter().map(|tool| tool.name.as_ref()).collect();
    assert_eq!(names, contract::TOOL_NAMES.to_vec());

    client.shutdown().await;
    daemon.shutdown().await;
}

#[tokio::test]
async fn a_tools_call_without_initialize_is_answered_not_dropped() {
    let temp = tempfile::tempdir().expect("tempdir");
    let daemon = start_daemon(&temp).await;
    let http = reqwest::Client::new();

    // ChatGPT's stateless client: no session, straight to tools/call.
    let response = http
        .post(daemon.local_mcp_url())
        .header("accept", "application/json, text/event-stream")
        .header("content-type", "application/json")
        .json(&json!({
            "jsonrpc": "2.0", "id": 7, "method": "tools/call",
            "params": {"name": "codex_exec", "arguments": {"cmd": "echo hi", "turn_token": "turn_unknownunknownunknownunknownxx"}}
        }))
        .send()
        .await
        .expect("request");
    assert!(
        response.status().is_success(),
        "status {}",
        response.status()
    );
    let body = response.text().await.expect("body");
    assert!(
        body.contains("turn_token is invalid"),
        "expected the claim refusal, got: {body}"
    );

    // The pre-init `server/discover` probe gets a JSON-RPC answer too.
    let response = http
        .post(daemon.local_mcp_url())
        .header("accept", "application/json, text/event-stream")
        .header("content-type", "application/json")
        .json(&json!({
            "jsonrpc": "2.0", "id": "openai-mcp-discover", "method": "server/discover",
            "params": {"_meta": {"io.modelcontextprotocol/protocolVersion": "2026-07-28"}}
        }))
        .send()
        .await
        .expect("request");
    let status = response.status();
    let body = response.text().await.expect("body");
    assert!(
        status.as_u16() < 500,
        "server/discover must not blow up: {status} {body}"
    );
    assert!(
        body.contains("jsonrpc"),
        "JSON-RPC shaped answer, got: {body}"
    );

    daemon.shutdown().await;
}

#[tokio::test]
async fn a_codex_exec_call_round_trips_through_the_owning_session() {
    let temp = tempfile::tempdir().expect("tempdir");
    let daemon = start_daemon(&temp).await;
    let session = FakeSession::register(&daemon, "session-a").await;
    session.register_turn(TOKEN, true).await;

    let client = chatgpt_client(&daemon.local_mcp_url()).await;
    let call = tokio::spawn({
        let mcp_url = daemon.local_mcp_url();
        async move {
            let client = chatgpt_client(&mcp_url).await;
            let result = client
                .call_tool(
                    contract::CODEX_EXEC.to_string(),
                    Some(json!({"cmd": "echo hi", "turn_token": TOKEN, "yield_time_ms": 5000})),
                    None,
                    Some(Duration::from_secs(20)),
                )
                .await
                .expect("call");
            client.shutdown().await;
            result
        }
    });

    // The session long-polls and receives the batch.
    let polled = session.poll(0, 5_000).await;
    assert_eq!(polled.batches.len(), 1, "{polled:?}");
    let batch = &polled.batches[0];
    assert_eq!(batch.turn_token, TOKEN);
    assert_eq!(batch.calls.len(), 1);
    let pending = &batch.calls[0];
    match &pending.target {
        contract::CallTarget::Function {
            namespace,
            name,
            arguments,
        } => {
            assert_eq!(namespace, &None);
            assert_eq!(name, "exec_command");
            assert_eq!(arguments["cmd"], "echo hi");
            assert_eq!(arguments["yield_time_ms"], 5000);
        }
        other => panic!("unexpected target {other:?}"),
    }

    assert_eq!(session.complete(&pending.call_id, "hi\n").await, 200);
    let result = call.await.expect("join");
    assert_eq!(result.is_error, Some(false));
    assert_eq!(text_of(&result), "hi\n");
    assert_eq!(result.structured_content, Some(json!({"exit_code": 0})));

    // Acked by echoing seq: nothing left.
    let again = session.poll(polled.seq, 100).await;
    assert!(again.batches.is_empty());

    // apply_patch resolves to a custom call when announced.
    let patch_call = tokio::spawn({
        let mcp_url = daemon.local_mcp_url();
        async move {
            let client = chatgpt_client(&mcp_url).await;
            let result = client
                .call_tool(
                    contract::CODEX_APPLY_PATCH.to_string(),
                    Some(json!({"patch": "*** Begin Patch\n*** End Patch", "turn_token": TOKEN})),
                    None,
                    Some(Duration::from_secs(20)),
                )
                .await
                .expect("call");
            client.shutdown().await;
            result
        }
    });
    let polled = session.poll(polled.seq, 5_000).await;
    let pending = &polled.batches[0].calls[0];
    assert!(matches!(
        &pending.target,
        contract::CallTarget::Custom { name, input } if name == "apply_patch" && input.starts_with("*** Begin Patch")
    ));
    assert_eq!(session.complete(&pending.call_id, "Done!").await, 200);
    assert_eq!(text_of(&patch_call.await.expect("join")), "Done!");

    // Inventory is served by the daemon without touching the session.
    let inventory = client
        .call_tool(
            contract::CODEX_TOOL_INVENTORY.to_string(),
            Some(json!({"turn_token": TOKEN})),
            None,
            Some(Duration::from_secs(10)),
        )
        .await
        .expect("inventory");
    let body: Value = serde_json::from_str(&text_of(&inventory)).expect("json");
    assert_eq!(body["total"], 2);

    // After the turn ends, the token reads as finished.
    session.end_turn(TOKEN).await;
    let stale = client
        .call_tool(
            contract::CODEX_EXEC.to_string(),
            Some(json!({"cmd": "echo again", "turn_token": TOKEN})),
            None,
            Some(Duration::from_secs(10)),
        )
        .await
        .expect("call");
    assert_eq!(stale.is_error, Some(true));
    assert!(
        text_of(&stale).contains("already finished"),
        "{}",
        text_of(&stale)
    );

    client.shutdown().await;
    session.disconnect().await;
    daemon.shutdown().await;
}

#[tokio::test]
async fn a_disconnected_session_fails_its_pending_call() {
    let temp = tempfile::tempdir().expect("tempdir");
    let daemon = start_daemon(&temp).await;
    let session = FakeSession::register(&daemon, "session-b").await;
    session.register_turn(TOKEN, false).await;

    let call = tokio::spawn({
        let mcp_url = daemon.local_mcp_url();
        async move {
            let client = chatgpt_client(&mcp_url).await;
            let result = client
                .call_tool(
                    contract::CODEX_EXEC.to_string(),
                    Some(json!({"cmd": "sleep 100", "turn_token": TOKEN})),
                    None,
                    Some(Duration::from_secs(20)),
                )
                .await
                .expect("call");
            client.shutdown().await;
            result
        }
    });
    let polled = session.poll(0, 5_000).await;
    assert_eq!(polled.batches.len(), 1);

    // A result from another session is refused.
    let intruder = FakeSession::register(&daemon, "session-c").await;
    assert_eq!(
        intruder
            .complete(&polled.batches[0].calls[0].call_id, "stolen")
            .await,
        403
    );

    session.disconnect().await;
    let result = call.await.expect("join");
    assert_eq!(result.is_error, Some(true));
    assert!(text_of(&result).contains("Codex session disconnected"));

    // apply_patch was not announced for this turn.
    let client = chatgpt_client(&daemon.local_mcp_url()).await;
    intruder
        .register_turn("turn_ffffffffffffffffffffffffffffffff", false)
        .await;
    let refused = client
        .call_tool(
            contract::CODEX_APPLY_PATCH.to_string(),
            Some(json!({"patch": "x", "turn_token": "turn_ffffffffffffffffffffffffffffffff"})),
            None,
            Some(Duration::from_secs(10)),
        )
        .await;
    match refused {
        Ok(result) => {
            assert_eq!(result.is_error, Some(true));
            assert!(text_of(&result).contains("apply_patch"));
        }
        Err(error) => assert!(error.to_string().contains("apply_patch"), "{error}"),
    }
    client.shutdown().await;
    daemon.shutdown().await;
}
