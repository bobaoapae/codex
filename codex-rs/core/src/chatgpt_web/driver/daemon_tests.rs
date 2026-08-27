// FORK: tests for the `daemon.ts` port (`DaemonClient`, `DaemonConfig`).
use super::*;
use crate::chatgpt_web::driver::DriverErrorKind;
use pretty_assertions::assert_eq;
use std::sync::Mutex as StdMutex;
use std::sync::PoisonError;

fn text_result(text: &str) -> CallToolResult {
    CallToolResult::success(vec![ContentBlock::text(text)])
}

#[test]
fn parse_result_turns_is_error_into_a_tool_error() {
    let result = CallToolResult::error(vec![ContentBlock::text("No frame with id 7")]);
    let error = parse_result(result).expect_err("isError must fail");
    assert_eq!(error.kind, DriverErrorKind::Tool);
    assert_eq!(error.message, "No frame with id 7");
}

#[test]
fn parse_result_uses_a_default_message_for_an_empty_error() {
    let error = parse_result(CallToolResult::error(Vec::new())).expect_err("isError must fail");
    assert_eq!(error.kind, DriverErrorKind::Tool);
    assert_eq!(error.message, "chrome-mcp tool call failed");
}

#[test]
fn parse_result_exposes_json_text_as_json() {
    let parsed = parse_result(text_result("{\n  \"id\": 42,\n  \"ok\": true\n}")).expect("ok");
    assert_eq!(parsed.json(), Some(json!({"id": 42, "ok": true})));
    assert_eq!(parsed.value(), json!({"id": 42, "ok": true}));
    assert!(parsed.images.is_empty());
}

#[test]
fn parse_result_keeps_plain_text_as_a_string() {
    let parsed = parse_result(text_result("navigated")).expect("ok");
    assert_eq!(parsed.json(), None);
    assert_eq!(parsed.value(), Value::String("navigated".to_string()));
}

#[test]
fn parse_result_keeps_image_content_as_base64() {
    let result = CallToolResult::success(vec![ContentBlock::image("AAAA", "image/png")]);
    let parsed = parse_result(result).expect("ok");
    assert_eq!(
        parsed.images,
        vec![("image/png".to_string(), "AAAA".to_string())]
    );
    assert_eq!(
        parsed.image(),
        Some(&("image/png".to_string(), "AAAA".to_string()))
    );
    assert_eq!(parsed.text, "");
}

#[test]
fn parse_result_joins_text_blocks_with_newlines() {
    let result = CallToolResult::success(vec![
        ContentBlock::text("one"),
        ContentBlock::image("AAAA", "image/png"),
        ContentBlock::text("two"),
    ]);
    let parsed = parse_result(result).expect("ok");
    assert_eq!(parsed.text, "one\ntwo");
    assert_eq!(parsed.images.len(), 1);
}

#[test]
fn eval_payload_is_decoded_twice() {
    // The page script resolves `JSON.stringify({ready: true})`; the daemon then
    // `JSON.stringify`s that string into the tool text.
    let inner = serde_json::to_string(&json!({"ready": true, "url": "https://chatgpt.com/"}))
        .expect("serialize");
    let tool_text = serde_json::to_string(&inner).expect("serialize");
    let parsed = parse_result(text_result(&tool_text)).expect("ok");
    assert_eq!(parsed.value(), Value::String(inner));
    assert_eq!(
        decode_eval_payload(parsed.value()),
        json!({"ready": true, "url": "https://chatgpt.com/"})
    );
}

#[test]
fn eval_payload_tolerates_a_non_string_result() {
    assert_eq!(
        decode_eval_payload(json!({"already": "object"})),
        json!({"already": "object"})
    );
    assert_eq!(decode_eval_payload(json!(7)), json!(7));
    assert_eq!(decode_eval_payload(Value::Null), Value::Null);
}

#[test]
fn eval_payload_keeps_a_non_json_string() {
    assert_eq!(
        decode_eval_payload(Value::String("plain".to_string())),
        Value::String("plain".to_string())
    );
}

#[test]
fn transport_issue_regex_matches_dead_session_errors() {
    for message in [
        "streamable HTTP session expired with 404 Not Found",
        "fetch failed",
        "connect ECONNREFUSED 127.0.0.1:8848",
        "read ECONNRESET",
        "transport terminated",
        "Connection Closed",
        "socket hang up",
        "network error",
        "HTTP 400 Bad Request",
    ] {
        assert!(is_transport_issue(message), "{message} should reconnect");
    }
}

#[test]
fn transport_issue_regex_ignores_page_errors() {
    for message in [
        "Cannot find default execution context",
        "timed out awaiting tools/call after 120s",
        "No frame with id 12",
    ] {
        assert!(!is_transport_issue(message), "{message} must not reconnect");
    }
}

#[test]
fn client_timeout_is_at_least_two_minutes_and_outlives_the_daemon_cap() {
    assert_eq!(client_timeout_for(0), Duration::from_secs(120));
    assert_eq!(client_timeout_for(30_000), Duration::from_secs(120));
    assert_eq!(client_timeout_for(200_000), Duration::from_secs(230));
}

#[test]
fn health_url_replaces_the_mcp_path_and_drops_the_query() {
    assert_eq!(
        health_url_for("http://127.0.0.1:8848/mcp?x=1#frag"),
        "http://127.0.0.1:8848/healthz"
    );
    assert_eq!(
        health_url_for("http://localhost:9000/deep/mcp/"),
        "http://localhost:9000/healthz"
    );
    assert_eq!(health_url_for("not a url/mcp"), "not a url/healthz");
}

#[test]
fn daemon_config_prefers_env_over_settings_and_token_file() {
    let dir = tempfile::tempdir().expect("tempdir");
    let token_file = dir.path().join("token.txt");
    std::fs::write(&token_file, "file-token\n").expect("write");
    let config = DaemonConfig::resolve_from(
        "http://127.0.0.1:8848/mcp",
        Some(&token_file),
        Some("http://127.0.0.1:9999/mcp".to_string()),
        Some("  env-token  ".to_string()),
        None,
    );
    assert_eq!(
        config,
        DaemonConfig {
            url: "http://127.0.0.1:9999/mcp".to_string(),
            token: Some("env-token".to_string()),
            health_url: "http://127.0.0.1:9999/healthz".to_string(),
        }
    );
}

#[test]
fn daemon_config_reads_and_trims_the_token_file_when_env_is_unset() {
    let dir = tempfile::tempdir().expect("tempdir");
    let token_file = dir.path().join("token.txt");
    std::fs::write(&token_file, "\n  file-token \r\n").expect("write");
    let config = DaemonConfig::resolve_from(
        "http://127.0.0.1:8848/mcp",
        Some(&token_file),
        None,
        Some(String::new()),
        None,
    );
    assert_eq!(config.url, "http://127.0.0.1:8848/mcp");
    assert_eq!(config.token, Some("file-token".to_string()));
    assert_eq!(config.health_url, "http://127.0.0.1:8848/healthz");
}

#[test]
fn daemon_config_defaults_to_the_home_token_file() {
    let home = tempfile::tempdir().expect("tempdir");
    let chrome_dir = home.path().join(".chrome-mcp");
    std::fs::create_dir_all(&chrome_dir).expect("mkdir");
    std::fs::write(chrome_dir.join("token.txt"), "home-token").expect("write");
    let config = DaemonConfig::resolve_from(
        DEFAULT_DAEMON_URL,
        None,
        None,
        None,
        Some(home.path().to_path_buf()),
    );
    assert_eq!(config.token, Some("home-token".to_string()));
}

#[test]
fn daemon_config_has_no_token_when_nothing_is_configured() {
    let home = tempfile::tempdir().expect("tempdir");
    let missing = home.path().join("missing-token.txt");
    let config = DaemonConfig::resolve_from(
        DEFAULT_DAEMON_URL,
        Some(&missing),
        None,
        None,
        Some(home.path().to_path_buf()),
    );
    assert_eq!(config.token, None);
    let empty = home.path().join("empty.txt");
    std::fs::write(&empty, "   \n").expect("write");
    let config = DaemonConfig::resolve_from(DEFAULT_DAEMON_URL, Some(&empty), None, None, None);
    assert_eq!(config.token, None);
}

/// Serializes the tests that mutate `CHROME_MCP_*` in the process environment.
static ENV_LOCK: StdMutex<()> = StdMutex::new(());

struct EnvGuard {
    saved: Vec<(&'static str, Option<String>)>,
}

impl EnvGuard {
    fn set(vars: &[(&'static str, Option<&str>)]) -> Self {
        let saved = vars
            .iter()
            .map(|(name, value)| {
                let previous = std::env::var(name).ok();
                // SAFETY: `ENV_LOCK` serializes every test that touches these
                // variables, and nothing else in the crate reads them.
                unsafe {
                    match value {
                        Some(value) => std::env::set_var(name, value),
                        None => std::env::remove_var(name),
                    }
                }
                (*name, previous)
            })
            .collect();
        Self { saved }
    }
}

impl Drop for EnvGuard {
    fn drop(&mut self) {
        for (name, previous) in self.saved.drain(..) {
            // SAFETY: see `EnvGuard::set`.
            unsafe {
                match previous {
                    Some(value) => std::env::set_var(name, value),
                    None => std::env::remove_var(name),
                }
            }
        }
    }
}

#[test]
fn daemon_config_resolve_honors_the_process_environment() {
    let _lock = ENV_LOCK.lock().unwrap_or_else(PoisonError::into_inner);
    let dir = tempfile::tempdir().expect("tempdir");
    let token_file = dir.path().join("token.txt");
    std::fs::write(&token_file, "file-token").expect("write");

    {
        let _env = EnvGuard::set(&[
            ("CHROME_MCP_URL", Some("http://127.0.0.1:7777/mcp")),
            ("CHROME_MCP_TOKEN", Some("env-token")),
        ]);
        let config = DaemonConfig::resolve(DEFAULT_DAEMON_URL, Some(&token_file));
        assert_eq!(config.url, "http://127.0.0.1:7777/mcp");
        assert_eq!(config.health_url, "http://127.0.0.1:7777/healthz");
        assert_eq!(config.token, Some("env-token".to_string()));
    }

    {
        let _env = EnvGuard::set(&[("CHROME_MCP_URL", None), ("CHROME_MCP_TOKEN", None)]);
        let config = DaemonConfig::resolve(DEFAULT_DAEMON_URL, Some(&token_file));
        assert_eq!(config.url, DEFAULT_DAEMON_URL);
        assert_eq!(config.token, Some("file-token".to_string()));
    }
}

fn live_client() -> DaemonClient {
    use codex_exec_server::RouteAwareHttpClient;
    use codex_http_client::HttpClientFactory;
    use codex_http_client::OutboundProxyPolicy;

    let http_client: Arc<dyn HttpClient> = Arc::new(
        RouteAwareHttpClient::new(HttpClientFactory::new(OutboundProxyPolicy::ReqwestDefault))
            .with_tls_backend_fallback(),
    );
    DaemonClient::new(DaemonConfig::resolve(DEFAULT_DAEMON_URL, None), http_client)
}

/// Needs a running chrome-mcp daemon on 127.0.0.1:8848. Read-only.
#[tokio::test]
#[ignore]
async fn live_daemon_health() {
    let client = live_client();
    let health = client.health().await.expect("healthz must answer");
    assert!(health.ok, "daemon reports ok=false");
    assert!(
        health.extension_connected,
        "the Chrome extension is not connected to the daemon"
    );
}

/// Needs a running chrome-mcp daemon on 127.0.0.1:8848. Read-only: opens an MCP
/// session (bearer, no Origin), lists tabs, and closes the session.
#[tokio::test]
#[ignore]
async fn live_daemon_lists_tabs_over_mcp() {
    let client = live_client();
    let result = client
        .call(
            "browser_tabs",
            json!({"action": "list"}),
            DEFAULT_TOOL_TIMEOUT_MS,
        )
        .await
        .expect("browser_tabs list must succeed");
    let tabs = result.json().expect("tab list is JSON");
    assert!(tabs.is_array(), "expected an array, got {tabs}");
    client.shutdown().await;
}
