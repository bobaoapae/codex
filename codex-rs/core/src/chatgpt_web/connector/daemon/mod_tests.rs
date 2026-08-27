use super::*;
use crate::chatgpt_web::connector::contract;
use pretty_assertions::assert_eq;

fn test_config(codex_home: &Path) -> DaemonRunConfig {
    let mut config = DaemonRunConfig::new(ChatGptWebSettings::default(), codex_home.to_path_buf());
    config.tunnel_override = Some(Arc::new(tunnel::NoopTunnel {
        endpoint: TunnelEndpoint::Public {
            mcp_url: "https://example.trycloudflare.com/mcp/x".into(),
        },
    }));
    config
}

#[tokio::test]
async fn a_daemon_starts_writes_its_state_and_answers_healthz() {
    let temp = tempfile::tempdir().expect("tempdir");
    let daemon = start(test_config(temp.path())).await.expect("starts");

    let paths = DaemonPaths::new(temp.path());
    assert!(paths.token.is_file());
    assert_eq!(
        state::read_secret(&paths.token).as_deref(),
        Some(daemon.endpoint.token.as_str())
    );

    let health = fetch_health(&daemon.endpoint.control_url)
        .await
        .expect("healthz");
    assert!(health.ok);
    assert_eq!(health.pid, std::process::id());
    assert_eq!(health.registry_status, "not_implemented");
    assert_eq!(health.tunnel_state, "ready");
    assert_eq!(
        health.public_url.as_deref(),
        Some("https://example.trycloudflare.com")
    );
    assert_eq!((health.sessions, health.active_turns), (0, 0));

    // daemon.json follows shortly after start.
    let mut state_file: Option<DaemonState> = None;
    for _ in 0..50 {
        state_file = state::read_json_opt(&paths.state);
        if state_file
            .as_ref()
            .is_some_and(|state| state.public_url.is_some())
        {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let state_file = state_file.expect("daemon.json");
    assert_eq!(state_file.pid, std::process::id());
    assert_eq!(state_file.control_port, daemon.control_addr.port());
    assert_eq!(state_file.codex_version, DAEMON_VERSION);

    let status = status(temp.path()).await;
    assert!(status.alive);
    assert!(status.health.is_some());

    // A second daemon for the same home is refused.
    let err = start(test_config(temp.path()))
        .await
        .err()
        .expect("second start fails");
    assert!(err.to_string().contains("already running"), "{err}");

    let endpoint = running_endpoint(temp.path()).await.expect("endpoint");
    assert_eq!(endpoint, daemon.endpoint);

    daemon.shutdown().await;
    assert!(!paths.state.exists());
    assert!(running_endpoint(temp.path()).await.is_none());
}

#[tokio::test]
async fn the_public_server_hides_behind_its_secret_and_serves_prm_json() {
    let temp = tempfile::tempdir().expect("tempdir");
    let daemon = start(test_config(temp.path())).await.expect("starts");
    let http = reqwest::Client::new();
    let base = format!("http://{}", daemon.public.local_addr());

    let wrong = http
        .get(format!("{base}/mcp/not-the-secret/healthz"))
        .send()
        .await
        .expect("request");
    assert_eq!(wrong.status(), 404);
    assert_eq!(
        wrong.headers()["content-type"],
        "application/json",
        "404s are JSON"
    );

    let right = http
        .get(format!("{}/healthz", daemon.local_mcp_url()))
        .send()
        .await
        .expect("request");
    assert_eq!(right.status(), 200);

    let prm = http
        .get(format!(
            "{base}/.well-known/oauth-protected-resource{}",
            daemon.public.mcp_path()
        ))
        .send()
        .await
        .expect("request");
    assert_eq!(prm.status(), 200);
    assert_eq!(prm.headers()["cache-control"], "no-store");
    let body: serde_json::Value = prm.json().await.expect("json");
    assert_eq!(body["authorization_servers"], serde_json::json!([]));
    assert_eq!(body["resource_name"], public_server::SERVER_NAME);

    let nowhere = http.get(format!("{base}/")).send().await.expect("request");
    assert_eq!(nowhere.status(), 404);
    daemon.shutdown().await;
}

#[tokio::test]
async fn the_control_api_needs_the_bearer_and_registers_turns() {
    let temp = tempfile::tempdir().expect("tempdir");
    let daemon = start(test_config(temp.path())).await.expect("starts");
    let http = reqwest::Client::new();
    let base = &daemon.endpoint.control_url;

    let unauthorized = http
        .post(format!("{base}/v1/sessions"))
        .json(&wire::RegisterSessionRequest {
            codex_pid: 1,
            session_id: "s1".into(),
            codex_version: "test".into(),
        })
        .send()
        .await
        .expect("request");
    assert_eq!(unauthorized.status(), 401);

    let registered = http
        .post(format!("{base}/v1/sessions"))
        .bearer_auth(&daemon.endpoint.token)
        .json(&wire::RegisterSessionRequest {
            codex_pid: 1,
            session_id: "s1".into(),
            codex_version: "test".into(),
        })
        .send()
        .await
        .expect("request");
    assert_eq!(registered.status(), 200);
    let body: wire::RegisterSessionResponse = registered.json().await.expect("json");
    assert_eq!(body.poll_url, "/v1/sessions/s1/calls");

    let turn = wire::RegisterTurnRequest {
        session_id: "s1".into(),
        turn_token: "turn_0123456789abcdef0123456789abcdef".into(),
        thread_id: "thread".into(),
        turn_id: "turn".into(),
        ttl_ms: 60_000,
        tools: vec![contract::ToolSummary {
            name: "exec_command".into(),
            namespace: None,
            kind: contract::ToolKind::Function,
            description: "run".into(),
            schema: None,
        }],
        exec_tool: contract::ExecTool::ExecCommand,
        apply_patch: false,
    };
    let created = http
        .post(format!("{base}/v1/turns"))
        .bearer_auth(&daemon.endpoint.token)
        .json(&turn)
        .send()
        .await
        .expect("request");
    assert_eq!(created.status(), 200);
    let body: wire::RegisterTurnResponse = created.json().await.expect("json");
    assert_eq!(body.tunnel_state, "ready");
    assert_eq!(body.registry_status, "not_implemented");

    let duplicate = http
        .post(format!("{base}/v1/turns"))
        .bearer_auth(&daemon.endpoint.token)
        .json(&turn)
        .send()
        .await
        .expect("request");
    assert_eq!(duplicate.status(), 409);

    let health = daemon.control.health();
    assert_eq!((health.sessions, health.active_turns), (1, 1));

    let ended = http
        .delete(format!("{base}/v1/turns/{}", turn.turn_token))
        .bearer_auth(&daemon.endpoint.token)
        .json(&wire::EndTurnRequest {
            reason: Some("done".into()),
        })
        .send()
        .await
        .expect("request");
    assert_eq!(ended.status(), 200);

    let not_implemented = http
        .post(format!("{base}/v1/registry/reconcile"))
        .bearer_auth(&daemon.endpoint.token)
        .send()
        .await
        .expect("request");
    assert_eq!(not_implemented.status(), 501);

    let gone = http
        .delete(format!("{base}/v1/sessions/s1"))
        .bearer_auth(&daemon.endpoint.token)
        .send()
        .await
        .expect("request");
    assert_eq!(gone.status(), 200);
    assert_eq!(daemon.broker.stats(), (0, 0));

    daemon.shutdown().await;
}

#[tokio::test]
async fn shutdown_when_idle_ends_the_daemon() {
    let temp = tempfile::tempdir().expect("tempdir");
    let daemon = start(test_config(temp.path())).await.expect("starts");
    let http = reqwest::Client::new();
    let response = http
        .post(format!(
            "{}/v1/admin/shutdown_when_idle",
            daemon.endpoint.control_url
        ))
        .bearer_auth(&daemon.endpoint.token)
        .send()
        .await
        .expect("request");
    assert_eq!(response.status(), 200);
    tokio::time::timeout(Duration::from_secs(3), daemon.wait())
        .await
        .expect("daemon stops on its own");
    daemon.shutdown().await;
}
