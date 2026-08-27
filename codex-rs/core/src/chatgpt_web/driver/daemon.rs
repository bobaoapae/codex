//! FORK: port of `chatgpt-pro-mcp/src/daemon.ts` (plus the daemon half of
//! `config.ts`) — the MCP client to the chrome-mcp daemon over Streamable HTTP.
//!
//! All browser control flows through [`DaemonClient`]: it holds exactly one
//! daemon session (`Mcp-Session-Id`), re-establishes it once when a call fails
//! with a transport-looking error, and normalizes tool results into
//! [`ToolResult`]. The daemon reports tool failures as `{isError: true}` results
//! (never as JSON-RPC errors), so `isError` is what becomes `DriverErrorKind::Tool`.
//!
//! Transport notes (verified against `rmcp-client/src/http_client_adapter.rs`):
//! no `Origin` header is ever sent (the daemon answers 403 to any Origin), the
//! bearer goes out as `Authorization: Bearer <token>`, and shutting the client
//! down cancels the rmcp transport worker, which issues `DELETE /mcp` with the
//! session id so the daemon can reap the session.

// TODO(M3): the provider does not consume the client yet; drop once `ops`/the
// provider wire it in.
#![allow(dead_code)]

use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Mutex as StdMutex;
use std::sync::OnceLock;
use std::sync::PoisonError;
use std::time::Duration;

use codex_config::types::AuthKeyringBackendKind;
use codex_config::types::OAuthCredentialsStoreMode;
use codex_exec_server::HttpClient;
use codex_exec_server::HttpRedirectPolicy;
use codex_exec_server::HttpRequestParams;
use codex_rmcp_client::ElicitationAction;
use codex_rmcp_client::ElicitationResponse;
use codex_rmcp_client::RmcpClient;
use codex_rmcp_client::SendElicitation;
use futures::FutureExt;
use futures::future::BoxFuture;
use regex_lite::Regex;
use rmcp::model::CallToolResult;
use rmcp::model::ClientCapabilities;
use rmcp::model::ContentBlock;
use rmcp::model::Implementation;
use rmcp::model::InitializeRequestParams;
use rmcp::model::ProtocolVersion;
use serde_json::Value;
use serde_json::json;
use tokio::sync::Semaphore;
use tracing::info;
use tracing::warn;

use super::DriverError;
use super::DriverResult;
use super::api::PageEval;
use super::tabs::TabId;

/// Default daemon endpoint (`config.ts:29`).
pub(crate) const DEFAULT_DAEMON_URL: &str = "http://127.0.0.1:8848/mcp";
/// The daemon's own per-call cap when `timeoutMs` is omitted.
pub(crate) const DEFAULT_TOOL_TIMEOUT_MS: u64 = 30_000;
/// Floor for the client-side cap of a tool call (`daemon.ts:20`).
const CALL_TIMEOUT: Duration = Duration::from_secs(120);
/// Slack added on top of the daemon's cap so a long browser op fails with the
/// daemon's specific error instead of a generic client timeout.
const CALL_TIMEOUT_SLACK: Duration = Duration::from_secs(30);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(30);
const HEALTH_TIMEOUT: Duration = Duration::from_secs(3);
/// MCP server name used for the rmcp-client transport (diagnostics only).
const SERVER_NAME: &str = "chatgpt_web";

/// Daemon connection settings (`Config` in `config.ts`, daemon fields only).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct DaemonConfig {
    /// Streamable HTTP endpoint of the chrome-mcp daemon.
    pub(crate) url: String,
    /// Bearer token shared with the daemon (`None` = none found).
    pub(crate) token: Option<String>,
    /// Daemon health endpoint (no auth).
    pub(crate) health_url: String,
}

impl DaemonConfig {
    /// Port of `loadConfig()`: `CHROME_MCP_URL` overrides the configured url,
    /// `CHROME_MCP_TOKEN` overrides the token, otherwise the token file
    /// (default `~/.chrome-mcp/token.txt`, trimmed) is read.
    pub(crate) fn resolve(settings_url: &str, token_file: Option<&Path>) -> Self {
        Self::resolve_from(
            settings_url,
            token_file,
            std::env::var("CHROME_MCP_URL").ok(),
            std::env::var("CHROME_MCP_TOKEN").ok(),
            dirs::home_dir(),
        )
    }

    /// Environment-free variant of [`Self::resolve`] so the precedence rules
    /// can be unit-tested without touching the process environment.
    pub(crate) fn resolve_from(
        settings_url: &str,
        token_file: Option<&Path>,
        env_url: Option<String>,
        env_token: Option<String>,
        home: Option<PathBuf>,
    ) -> Self {
        let url = env_url
            .map(|value| value.trim().to_string())
            .filter(|value| !value.is_empty())
            .unwrap_or_else(|| settings_url.trim().to_string());
        let token = env_token
            .map(|value| value.trim().to_string())
            .filter(|value| !value.is_empty())
            .or_else(|| {
                let path = match token_file {
                    Some(path) => path.to_path_buf(),
                    None => home?.join(".chrome-mcp").join("token.txt"),
                };
                read_token_file(&path)
            });
        Self {
            health_url: health_url_for(&url),
            url,
            token,
        }
    }
}

fn read_token_file(path: &Path) -> Option<String> {
    let raw = std::fs::read_to_string(path).ok()?;
    let trimmed = raw.trim();
    (!trimmed.is_empty()).then(|| trimmed.to_string())
}

/// `config.ts:30-32`: same origin as the MCP url, path `/healthz`, no query.
pub(crate) fn health_url_for(mcp_url: &str) -> String {
    match url::Url::parse(mcp_url) {
        Ok(mut parsed) => {
            parsed.set_path("/healthz");
            parsed.set_query(None);
            parsed.set_fragment(None);
            parsed.to_string()
        }
        Err(_) => {
            let base = mcp_url
                .trim_end_matches('/')
                .strip_suffix("/mcp")
                .unwrap_or(mcp_url.trim_end_matches('/'));
            format!("{base}/healthz")
        }
    }
}

/// `GET /healthz` payload.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub(crate) struct DaemonHealth {
    pub(crate) ok: bool,
    pub(crate) extension_connected: bool,
}

/// Normalized chrome-mcp tool result (`parseResult` in `daemon.ts`).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct ToolResult {
    /// All `text` content blocks joined with `\n`.
    pub(crate) text: String,
    /// All `image` content blocks as `(mime_type, base64)`.
    pub(crate) images: Vec<(String, String)>,
}

impl ToolResult {
    /// The text parsed as JSON, when it is JSON.
    pub(crate) fn json(&self) -> Option<Value> {
        serde_json::from_str(&self.text).ok()
    }

    /// Port of `parseResult`'s return value for non-image results: the parsed
    /// JSON when the text parses, else the raw text as a JSON string.
    pub(crate) fn value(&self) -> Value {
        self.json()
            .unwrap_or_else(|| Value::String(self.text.clone()))
    }

    /// First image block, as `(mime_type, base64)`.
    pub(crate) fn image(&self) -> Option<&(String, String)> {
        self.images.first()
    }
}

/// Port of `parseResult` (`daemon.ts:121-139`): joins text blocks, keeps image
/// blocks, and turns `isError` into `DriverErrorKind::Tool`.
pub(crate) fn parse_result(result: CallToolResult) -> DriverResult<ToolResult> {
    let mut texts = Vec::new();
    let mut images = Vec::new();
    for block in result.content {
        match block {
            ContentBlock::Text(text) => texts.push(text.text),
            ContentBlock::Image(image) => images.push((image.mime_type, image.data)),
            _ => {}
        }
    }
    let text = texts.join("\n");
    if result.is_error == Some(true) {
        let message = if text.is_empty() {
            "chrome-mcp tool call failed".to_string()
        } else {
            text
        };
        return Err(DriverError::tool(message));
    }
    Ok(ToolResult { text, images })
}

/// Second decoding layer of `evalIn` (`daemon.ts:159-166`): page scripts
/// always resolve `JSON.stringify(...)`, so the tool's JSON text decodes to a
/// string that is itself JSON. A non-string (or non-JSON string) is returned
/// as is.
pub(crate) fn decode_eval_payload(raw: Value) -> Value {
    match raw {
        Value::String(text) => serde_json::from_str(&text).unwrap_or(Value::String(text)),
        other => other,
    }
}

/// Port of the transport-error regex in `daemon.ts:107-109`: errors that look
/// like a dead daemon session get exactly one reconnect + retry.
pub(crate) fn is_transport_issue(message: &str) -> bool {
    static PATTERN: OnceLock<Regex> = OnceLock::new();
    let regex = PATTERN.get_or_init(|| {
        #[expect(clippy::expect_used, reason = "the pattern is a compile-time literal")]
        Regex::new(
            "(?i)session|fetch failed|ECONNREFUSED|ECONNRESET|terminated|connection closed|socket|network|404|Bad Request",
        )
        .expect("transport-issue regex must compile")
    });
    regex.is_match(message)
}

/// Client-side cap for a tool call: `max(120s, daemon cap + 30s)`.
pub(crate) fn client_timeout_for(daemon_timeout_ms: u64) -> Duration {
    CALL_TIMEOUT.max(Duration::from_millis(daemon_timeout_ms) + CALL_TIMEOUT_SLACK)
}

fn initialize_params() -> InitializeRequestParams {
    InitializeRequestParams::new(
        ClientCapabilities::default(),
        Implementation::new("codex-chatgpt-web", env!("CARGO_PKG_VERSION")).with_title("Codex"),
    )
    .with_protocol_version(ProtocolVersion::V_2025_06_18)
}

/// The daemon never elicits; decline anything it might send anyway.
fn decline_elicitations() -> SendElicitation {
    Box::new(|_, _| {
        async {
            Ok(ElicitationResponse {
                action: ElicitationAction::Decline,
                content: None,
                meta: None,
            })
        }
        .boxed()
    })
}

/// MCP client to the chrome-mcp daemon (port of `DaemonClient` in `daemon.ts`).
///
/// Construction does no I/O; the session is opened lazily by the first call
/// (or [`Self::ensure_connected`]).
pub(crate) struct DaemonClient {
    config: DaemonConfig,
    http_client: Arc<dyn HttpClient>,
    /// The live session, if any.
    connection: StdMutex<Option<Arc<RmcpClient>>>,
    /// Held across connect so concurrent callers share one handshake (the
    /// `connecting` promise in the TS).
    connect_gate: Semaphore,
}

impl DaemonClient {
    pub(crate) fn new(config: DaemonConfig, http_client: Arc<dyn HttpClient>) -> Self {
        Self {
            config,
            http_client,
            connection: StdMutex::new(None),
            connect_gate: Semaphore::new(/*permits*/ 1),
        }
    }

    pub(crate) fn config(&self) -> &DaemonConfig {
        &self.config
    }

    fn daemon_down(&self, error: impl std::fmt::Display) -> DriverError {
        DriverError::daemon_down(format!(
            "Cannot reach the chrome-mcp daemon at {}: {error}. Is the daemon running? (it powers all browser control for this provider)",
            self.config.url
        ))
    }

    fn current(&self) -> Option<Arc<RmcpClient>> {
        self.connection
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// Open the daemon session if there is none yet (`connect()` in the TS).
    pub(crate) async fn ensure_connected(&self) -> DriverResult<Arc<RmcpClient>> {
        if let Some(client) = self.current() {
            return Ok(client);
        }
        let _gate = self
            .connect_gate
            .acquire()
            .await
            .map_err(|_| self.daemon_down("connect gate closed"))?;
        if let Some(client) = self.current() {
            return Ok(client);
        }
        let client = RmcpClient::new_streamable_http_client(
            SERVER_NAME,
            &self.config.url,
            self.config.token.clone(),
            /*http_headers*/ None,
            /*env_http_headers*/ None,
            // A bearer token is configured; the OAuth store is never consulted
            // when it is. `File` keeps the fallback path away from the keyring.
            OAuthCredentialsStoreMode::File,
            AuthKeyringBackendKind::default(),
            Arc::clone(&self.http_client),
            /*auth_provider*/ None,
        )
        .await
        .map_err(|error| self.daemon_down(format!("{error:#}")))?;
        if let Err(error) = client
            .initialize(
                initialize_params(),
                Some(CONNECT_TIMEOUT),
                decline_elicitations(),
            )
            .await
        {
            client.shutdown().await;
            return Err(self.daemon_down(format!("{error:#}")));
        }
        info!("[chatgpt_web daemon] connected to {}", self.config.url);
        let client = Arc::new(client);
        *self
            .connection
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = Some(Arc::clone(&client));
        Ok(client)
    }

    /// Drop the current session and tell the daemon it is over (`reset()`).
    /// The rmcp transport worker sends `DELETE /mcp` with the session id as it
    /// winds down, so the daemon can reap the session instead of leaking it.
    async fn reset(&self) {
        let previous = self
            .connection
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take();
        if let Some(client) = previous {
            client.shutdown().await;
        }
    }

    /// Daemon liveness + whether the Chrome extension is connected
    /// (`GET /healthz`, 3s, no auth).
    pub(crate) async fn health(&self) -> DriverResult<DaemonHealth> {
        let request = self.http_client.http_request(HttpRequestParams {
            method: "GET".to_string(),
            url: self.config.health_url.clone(),
            headers: Vec::new(),
            body: None,
            timeout_ms: Some(HEALTH_TIMEOUT.as_millis() as u64),
            redirect_policy: HttpRedirectPolicy::Follow,
            request_id: "chatgpt-web-healthz".to_string(),
            stream_response: false,
        });
        let response = tokio::time::timeout(HEALTH_TIMEOUT, request)
            .await
            .map_err(|_| self.daemon_down("health check timed out"))?
            .map_err(|error| self.daemon_down(format!("health check failed: {error}")))?;
        if !(200..300).contains(&response.status) {
            return Err(self.daemon_down(format!("health check returned HTTP {}", response.status)));
        }
        serde_json::from_slice::<DaemonHealth>(&response.body.into_inner()).map_err(|error| {
            self.daemon_down(format!("health check returned invalid JSON: {error}"))
        })
    }

    /// Call one chrome-mcp tool and return its normalized result.
    ///
    /// `timeout_ms` is the daemon-side cap for this call (`params.timeoutMs`,
    /// default 30s); it is inserted into `args` as `timeoutMs` when absent, and
    /// the client-side cap is `max(120s, timeout_ms + 30s)` so a slow browser
    /// op fails with the daemon's specific error rather than a client timeout.
    ///
    /// Transport failures matching [`is_transport_issue`] trigger ONE
    /// reconnect and retry. `isError` results are never retried (the browser
    /// action may have run), which is the one deliberate divergence from
    /// `daemon.ts`, where `parseResult` throws inside the retried block.
    pub(crate) async fn call(
        &self,
        tool: &str,
        args: Value,
        timeout_ms: u64,
    ) -> DriverResult<ToolResult> {
        let Value::Object(mut args) = args else {
            return Err(DriverError::other(format!(
                "chrome-mcp {tool}: tool arguments must be a JSON object"
            )));
        };
        args.entry("timeoutMs")
            .or_insert_with(|| Value::from(timeout_ms));
        let args = Value::Object(args);
        let client_timeout = client_timeout_for(timeout_ms);

        let mut attempt = 0;
        loop {
            let client = self.ensure_connected().await?;
            match client
                .call_tool(
                    tool.to_string(),
                    Some(args.clone()),
                    /*meta*/ None,
                    Some(client_timeout),
                )
                .await
            {
                Ok(result) => return parse_result(result),
                Err(error) => {
                    let message = format!("{error:#}");
                    if attempt == 0 && is_transport_issue(&message) {
                        let preview: String = message.chars().take(120).collect();
                        warn!("[chatgpt_web daemon] {tool} failed ({preview}); reconnecting once");
                        self.reset().await;
                        attempt += 1;
                        continue;
                    }
                    return Err(classify_call_error(tool, &message, client_timeout));
                }
            }
        }
    }

    /// Evaluate a page-side function (source string) in a tab's MAIN world.
    ///
    /// Page functions always resolve to a JSON string (never `async` — the
    /// injected runner returns `{}` for those), so the result is decoded twice:
    /// once as the tool's JSON text, once more for the script's `JSON.stringify`.
    ///
    /// `timeout_ms` raises the daemon's per-call cap: hidden pool tabs throttle
    /// timers down to one tick per minute after ~5min occluded, so even a
    /// 300ms page-side sleep can stretch past 30s of wall clock.
    pub(crate) async fn eval_in(
        &self,
        tab_id: TabId,
        expression: String,
        timeout_ms: u64,
    ) -> DriverResult<Value> {
        let args = json!({
            "tabId": tab_id,
            "expression": expression,
            "world": "MAIN",
            "timeoutMs": timeout_ms,
        });
        let result = self.call("browser_eval", args, timeout_ms).await?;
        Ok(decode_eval_payload(result.value()))
    }

    /// Close the daemon session (issues the `DELETE` so the daemon reaps it).
    pub(crate) async fn shutdown(&self) {
        self.reset().await;
    }
}

impl PageEval for DaemonClient {
    fn eval<'a>(
        &'a self,
        tab_id: TabId,
        expression: String,
        timeout_ms: u64,
    ) -> BoxFuture<'a, DriverResult<Value>> {
        self.eval_in(tab_id, expression, timeout_ms).boxed()
    }
}

fn classify_call_error(tool: &str, message: &str, client_timeout: Duration) -> DriverError {
    if message.contains("timed out") {
        DriverError::timeout(format!(
            "chrome-mcp {tool} did not answer within {}s: {message}",
            client_timeout.as_secs()
        ))
    } else {
        DriverError::other(format!("chrome-mcp {tool} failed: {message}"))
    }
}

#[cfg(test)]
#[path = "daemon_tests.rs"]
mod tests;
