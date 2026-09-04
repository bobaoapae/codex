use super::*;
use pretty_assertions::assert_eq;

#[test]
fn the_trycloudflare_url_is_parsed_out_of_the_log_box() {
    let line = "2026-08-27T00:00:00Z INF |  https://quiet-otter-pine.trycloudflare.com  |";
    assert_eq!(
        parse_trycloudflare_url(line).as_deref(),
        Some("https://quiet-otter-pine.trycloudflare.com")
    );
    assert_eq!(
        parse_trycloudflare_url("INF Registered tunnel connection"),
        None
    );
}

#[test]
fn tunnel_ids_are_validated() {
    assert!(is_valid_tunnel_id(
        "tunnel_0123456789abcdef0123456789abcdef"
    ));
    assert!(!is_valid_tunnel_id("tunnel_0123"));
    assert!(!is_valid_tunnel_id("0123456789abcdef0123456789abcdef"));
    assert!(!is_valid_tunnel_id(
        "tunnel_0123456789abcdef0123456789abcdeg"
    ));
}

#[test]
fn auth_failures_and_outages_are_told_apart() {
    assert!(is_auth_failure(
        r#"{"level":"error","msg":"poll failed: 401 Unauthorized"}"#
    ));
    assert!(is_auth_failure("invalid_api_key"));
    assert!(!is_auth_failure(r#"{"level":"info","msg":"ready"}"#));

    assert!(is_unreachable(
        "dial tcp: lookup api.openai.com: no such host"
    ));
    assert!(is_unreachable("poll failed: context deadline exceeded"));
    assert!(!is_unreachable("channel main: probe ok"));
}

#[test]
fn backoff_grows_and_caps() {
    assert_eq!(backoff(0), Duration::from_secs(2));
    assert_eq!(backoff(1), Duration::from_secs(4));
    assert_eq!(backoff(3), Duration::from_secs(16));
    assert_eq!(backoff(10), MAX_BACKOFF);
}

#[test]
fn endpoints_describe_themselves_without_secrets() {
    let public = TunnelEndpoint::Public {
        mcp_url: "https://a-b.trycloudflare.com/mcp/SECRET".into(),
    };
    assert_eq!(public.public_label(), "https://a-b.trycloudflare.com");
    let openai = TunnelEndpoint::OpenAi {
        tunnel_id: "tunnel_x".into(),
    };
    assert_eq!(openai.public_label(), "tunnel:tunnel_x");
    assert_eq!(TunnelState::Down { reason: "x".into() }.label(), "down: x");
}

#[test]
fn the_pinned_release_has_a_checksum_for_this_platform() {
    assert!(pinned_archive_sha256(PINNED_TUNNEL_CLIENT_VERSION).is_some());
    assert_eq!(pinned_archive_sha256("9.9.9"), None);
    let asset = release_asset_name(PINNED_TUNNEL_CLIENT_VERSION);
    assert!(asset.starts_with("tunnel-client-v0.0.12-"));
    assert!(asset.ends_with(".zip"));
}

#[test]
fn managed_binary_path_is_versioned() {
    let path = managed_tunnel_client_path(Path::new("/bin"), "0.0.12");
    let name = path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or_default();
    assert!(name.starts_with("tunnel-client-v0.0.12"));
}

#[tokio::test]
async fn the_noop_tunnel_is_ready_at_once_and_stops_on_cancel() {
    let handle = start(
        Arc::new(NoopTunnel {
            endpoint: TunnelEndpoint::Public {
                mcp_url: "http://127.0.0.1:1/mcp/x".into(),
            },
        }),
        "http://127.0.0.1:1/mcp/x".into(),
    );
    let endpoint = handle
        .wait_ready(Duration::from_secs(1))
        .await
        .expect("ready");
    assert!(matches!(endpoint, TunnelEndpoint::Public { .. }));
    handle.shutdown().await;
}

#[tokio::test]
async fn a_fatal_tunnel_fails_wait_ready_with_its_reason() {
    let handle = start(
        Arc::new(FatalTunnel {
            reason: "no key".into(),
        }),
        "http://127.0.0.1:1/mcp/x".into(),
    );
    let err = handle
        .wait_ready(Duration::from_secs(1))
        .await
        .expect_err("fatal");
    assert_eq!(err, "no key");
    handle.shutdown().await;
}

#[test]
fn a_missing_cloudflared_path_is_none_and_the_local_install_is_found_when_present() {
    assert_eq!(
        resolve_cloudflared(Some(Path::new("/definitely/missing"))),
        None
    );
    let resolved = resolve_cloudflared(None);
    if let Some(path) = resolved {
        assert!(path.is_file());
    }
}

/// FORK: `tunnel-client admin tunnels get --json` says who a tunnel is shared
/// with. That is the fact that turns "not visible to the account logged in
/// Chrome" into a two-minute fix, so the parser tolerates the shapes this API
/// has used and simply says nothing when it recognises none of them.
#[test]
fn tunnel_audience_is_parsed_from_the_admin_json() {
    let flat = parse_tunnel_audience(&serde_json::json!({
        "chatgpt_accounts": ["fbf63138-0000-4000-8000-000000000000"],
        "chatgpt_workspaces": [],
    }));
    assert_eq!(
        flat.chatgpt_accounts,
        vec!["fbf63138-0000-4000-8000-000000000000".to_string()]
    );
    assert!(flat.workspaces.is_empty());
    assert!(flat.describe().contains("fbf63138"));

    let nested = parse_tunnel_audience(&serde_json::json!({
        "tunnel": {
            "accounts": [{ "account_id": "acc_1" }],
            "workspaces": [{ "id": "ws_1" }],
        }
    }));
    assert_eq!(nested.chatgpt_accounts, vec!["acc_1".to_string()]);
    assert_eq!(nested.workspaces, vec!["ws_1".to_string()]);
    assert_eq!(nested.describe(), "account(s) acc_1 and workspace(s) ws_1");

    let unknown = parse_tunnel_audience(&serde_json::json!({ "something": "else" }));
    assert!(unknown.is_empty());
    assert_eq!(unknown.describe(), "nobody");
}
