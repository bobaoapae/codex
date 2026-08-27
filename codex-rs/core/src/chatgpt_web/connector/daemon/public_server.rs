//! FORK: the MCP server ChatGPT's connector talks to.
//!
//! Loopback only: with the OpenAI tunnel the `tunnel-client` is the sole
//! caller, and with cloudflared the quick tunnel forwards to it. Either way the
//! endpoint sits behind a 256-bit secret path regenerated on every start, and
//! every mutating tool needs a valid `turn_token` on top (see `contract`).
//!
//! The server is stateless (`legacy_session_mode: false`): ChatGPT's client
//! (`openai-mcp/1.0.0`) sends no `Mcp-Session-Id` and opens one SSE response
//! per request (spike S1).

use super::broker::BrokerResult;
use super::broker::TurnBroker;
use super::wire::ResultContent;
use crate::chatgpt_web::connector::contract;
use axum::Router;
use axum::body::Body;
use axum::extract::State;
use axum::http::HeaderValue;
use axum::http::Request;
use axum::http::StatusCode;
use axum::http::header;
use axum::middleware;
use axum::middleware::Next;
use axum::response::Response;
use axum::routing::get;
use base64::Engine;
use rmcp::ErrorData as McpError;
use rmcp::handler::server::ServerHandler;
use rmcp::model::CallToolRequestParams;
use rmcp::model::CallToolResponse;
use rmcp::model::CallToolResult;
use rmcp::model::ContentBlock;
use rmcp::model::ListToolsResult;
use rmcp::model::PaginatedRequestParams;
use rmcp::model::ServerCapabilities;
use rmcp::model::ServerInfo;
use rmcp::service::RequestContext;
use rmcp::service::RoleServer;
use rmcp::transport::StreamableHttpServerConfig;
use rmcp::transport::StreamableHttpService;
use rmcp::transport::streamable_http_server::session::local::LocalSessionManager;
use serde_json::json;
use sha2::Digest;
use std::collections::VecDeque;
use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::Mutex;
use std::time::Duration;
use std::time::Instant;
use tokio::net::TcpListener;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

/// Global call budget: ChatGPT serializes calls within a response, so anything
/// past this is not a model at work.
const CALLS_PER_WINDOW: usize = 30;
const CALLS_WINDOW: Duration = Duration::from_secs(10);
/// Failed claims are what a token-guessing client produces.
const FAILED_CLAIMS_PER_WINDOW: usize = 10;
const FAILED_CLAIMS_WINDOW: Duration = Duration::from_secs(60);

pub const SERVER_NAME: &str = "Codex Native";

#[derive(Debug, Clone)]
pub struct PublicServerConfig {
    /// `0` picks an ephemeral port.
    pub port: u16,
    pub sse_keep_alive: Duration,
    pub max_request_body_bytes: usize,
    pub cancel: CancellationToken,
}

impl Default for PublicServerConfig {
    fn default() -> Self {
        Self {
            port: 0,
            sse_keep_alive: Duration::from_secs(15),
            max_request_body_bytes: 8 * 1024 * 1024,
            cancel: CancellationToken::new(),
        }
    }
}

/// A running public server.
pub struct PublicServer {
    local_addr: SocketAddr,
    secret: String,
    task: JoinHandle<()>,
}

impl std::fmt::Debug for PublicServer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PublicServer")
            .field("local_addr", &self.local_addr)
            .finish_non_exhaustive()
    }
}

impl PublicServer {
    pub fn local_addr(&self) -> SocketAddr {
        self.local_addr
    }

    /// The secret path segment. Never logged.
    pub fn secret(&self) -> &str {
        &self.secret
    }

    /// `/mcp/<secret>`.
    pub fn mcp_path(&self) -> String {
        format!("/mcp/{}", self.secret)
    }

    /// `http://127.0.0.1:<port>/mcp/<secret>` — what the tunnel forwards to.
    pub fn local_mcp_url(&self) -> String {
        format!("http://{}{}", self.local_addr, self.mcp_path())
    }

    pub fn abort(&self) {
        self.task.abort();
    }
}

impl Drop for PublicServer {
    fn drop(&mut self) {
        self.task.abort();
    }
}

/// Starts the server on `127.0.0.1:{port}`.
pub async fn start(
    broker: Arc<TurnBroker>,
    config: PublicServerConfig,
) -> std::io::Result<PublicServer> {
    let listener = TcpListener::bind(("127.0.0.1", config.port)).await?;
    let local_addr = listener.local_addr()?;
    let secret = new_secret();

    let handler = PublicMcpHandler {
        broker,
        limiter: Arc::new(RateLimiter::default()),
    };
    let server_config = StreamableHttpServerConfig::default()
        .with_legacy_session_mode(false)
        .with_json_response(false)
        .with_sse_keep_alive(Some(config.sse_keep_alive))
        .with_max_request_body_bytes(config.max_request_body_bytes)
        .with_cancellation_token(config.cancel.child_token())
        .with_allowed_hosts([
            "127.0.0.1".to_string(),
            format!("127.0.0.1:{}", local_addr.port()),
            "localhost".to_string(),
            format!("localhost:{}", local_addr.port()),
        ]);
    let service = StreamableHttpService::new(
        move || Ok(handler.clone()),
        Arc::new(LocalSessionManager::default()),
        server_config,
    );

    let gate = Arc::new(SecretGate {
        secret: secret.clone(),
    });
    let router = Router::new()
        .route("/mcp/{secret}/healthz", get(healthz))
        .route(
            "/.well-known/oauth-protected-resource/mcp/{secret}",
            get(protected_resource_metadata),
        )
        .route_service("/mcp/{secret}", service)
        .fallback(not_found)
        .layer(middleware::from_fn_with_state(gate, require_secret));

    let cancel = config.cancel.clone();
    let task = tokio::spawn(async move {
        let serve = axum::serve(listener, router).with_graceful_shutdown(async move {
            cancel.cancelled().await;
        });
        if let Err(error) = serve.await {
            tracing::warn!("chatgpt_web public MCP server stopped: {error}");
        }
    });
    Ok(PublicServer {
        local_addr,
        secret,
        task,
    })
}

fn new_secret() -> String {
    use rand::RngCore;
    let mut bytes = [0u8; 32];
    rand::rng().fill_bytes(&mut bytes);
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
}

/// Constant-time byte comparison; the secret is the only thing between the
/// public internet and this server in cloudflared mode.
pub fn secrets_match(candidate: &str, expected: &str) -> bool {
    let (a, b) = (candidate.as_bytes(), expected.as_bytes());
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b) {
        diff |= x ^ y;
    }
    diff == 0
}

struct SecretGate {
    secret: String,
}

/// Extracts the secret segment of a request path, if it is one of ours.
fn secret_segment(path: &str) -> Option<&str> {
    let rest = path
        .strip_prefix("/.well-known/oauth-protected-resource/mcp/")
        .or_else(|| path.strip_prefix("/mcp/"))?;
    let segment = rest.split('/').next().unwrap_or_default();
    (!segment.is_empty()).then_some(segment)
}

async fn require_secret(
    State(gate): State<Arc<SecretGate>>,
    request: Request<Body>,
    next: Next,
) -> Response {
    let ok = secret_segment(request.uri().path())
        .is_some_and(|segment| secrets_match(segment, &gate.secret));
    if ok {
        next.run(request).await
    } else {
        json_error(StatusCode::NOT_FOUND, "not_found")
    }
}

fn json_error(status: StatusCode, error: &str) -> Response {
    let body = json!({ "error": error }).to_string();
    let mut response = Response::new(Body::from(body));
    *response.status_mut() = status;
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/json"),
    );
    response
}

async fn not_found() -> Response {
    json_error(StatusCode::NOT_FOUND, "not_found")
}

async fn healthz() -> Response {
    let mut response = Response::new(Body::from(json!({ "ok": true }).to_string()));
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/json"),
    );
    response
}

/// RFC 9728 protected-resource metadata: an explicit "no authorization server"
/// answer, as JSON, so ChatGPT's discovery treats the connector as auth-less
/// instead of stalling on a text 404.
async fn protected_resource_metadata(request: Request<Body>) -> Response {
    let headers = request.headers();
    let header_str = |name: &str| {
        headers
            .get(name)
            .and_then(|value| value.to_str().ok())
            .map(str::to_string)
    };
    let scheme = header_str("x-forwarded-proto").unwrap_or_else(|| "http".to_string());
    let host = header_str("x-forwarded-host")
        .or_else(|| header_str("host"))
        .unwrap_or_else(|| "127.0.0.1".to_string());
    let path = request
        .uri()
        .path()
        .strip_prefix("/.well-known/oauth-protected-resource")
        .unwrap_or_default();
    let body = json!({
        "resource": format!("{scheme}://{host}{path}"),
        "resource_name": SERVER_NAME,
        "authorization_servers": [],
        "scopes_supported": [],
    });
    let mut response = Response::new(Body::from(body.to_string()));
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/json"),
    );
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    response
}

/// Sliding-window counters for the two abuse shapes the endpoint can see.
#[derive(Default)]
struct RateLimiter {
    calls: Mutex<VecDeque<Instant>>,
    failed_claims: Mutex<VecDeque<Instant>>,
}

impl RateLimiter {
    fn admit(queue: &Mutex<VecDeque<Instant>>, cap: usize, window: Duration) -> bool {
        let now = Instant::now();
        let mut queue = queue
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        while queue
            .front()
            .is_some_and(|at| now.duration_since(*at) > window)
        {
            queue.pop_front();
        }
        if queue.len() >= cap {
            return false;
        }
        queue.push_back(now);
        true
    }

    fn admit_call(&self) -> bool {
        Self::admit(&self.calls, CALLS_PER_WINDOW, CALLS_WINDOW)
    }

    /// Records a failed claim; `false` when the budget is exhausted.
    fn record_failed_claim(&self) -> bool {
        Self::admit(
            &self.failed_claims,
            FAILED_CLAIMS_PER_WINDOW,
            FAILED_CLAIMS_WINDOW,
        )
    }

    fn failed_claims_exhausted(&self) -> bool {
        let now = Instant::now();
        let queue = self
            .failed_claims
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        queue
            .iter()
            .filter(|at| now.duration_since(**at) <= FAILED_CLAIMS_WINDOW)
            .count()
            >= FAILED_CLAIMS_PER_WINDOW
    }
}

/// Short hash for logs: never the value itself.
fn log_hash(value: &str) -> String {
    let digest = sha2::Sha256::digest(value.as_bytes());
    digest[..6]
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

#[derive(Clone)]
struct PublicMcpHandler {
    broker: Arc<TurnBroker>,
    limiter: Arc<RateLimiter>,
}

fn text_result(text: String, is_error: bool) -> CallToolResult {
    let content = vec![ContentBlock::text(text)];
    if is_error {
        CallToolResult::error(content)
    } else {
        CallToolResult::success(content)
    }
}

/// Converts a session's result into what the MCP client receives.
pub fn to_call_tool_result(result: BrokerResult) -> CallToolResult {
    let content: Vec<ContentBlock> = result
        .content
        .into_iter()
        .map(|item| match item {
            ResultContent::Text { text } => ContentBlock::text(text),
            ResultContent::Image { data, mime_type } => ContentBlock::image(data, mime_type),
        })
        .collect();
    let mut out = if result.is_error {
        CallToolResult::error(content)
    } else {
        CallToolResult::success(content)
    };
    if let Some(structured) = result.structured
        && let serde_json::Value::Object(object) = structured
    {
        out.structured_content = Some(serde_json::Value::Object(object));
    }
    out
}

impl ServerHandler for PublicMcpHandler {
    fn get_info(&self) -> ServerInfo {
        let mut info = ServerInfo::new(ServerCapabilities::builder().enable_tools().build());
        info.server_info.name = SERVER_NAME.to_string();
        info.server_info.version = env!("CARGO_PKG_VERSION").to_string();
        info.instructions = Some(
            "Tools of the Codex session that is talking to you. Every call needs the \
turn_token from the current Codex prompt, passed unchanged. Commands run on the user's \
machine under Codex's sandbox and approvals; keep yield_time_ms at or below 30000 and \
poll long commands with codex_write_stdin."
                .to_string(),
        );
        info
    }

    async fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, McpError> {
        Ok(ListToolsResult::with_all_items(contract::tools()))
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResponse, McpError> {
        let request_tag = context
            .extensions
            .get::<http::request::Parts>()
            .and_then(|parts| {
                parts
                    .headers
                    .get("x-request-id")
                    .or_else(|| parts.headers.get("x-openai-session"))
                    .and_then(|value| value.to_str().ok())
                    .map(log_hash)
            })
            .unwrap_or_default();

        if !self.limiter.admit_call() {
            tracing::warn!(request = %request_tag, "chatgpt_web connector: call rate limit hit");
            return Ok(text_result(
                "Too many connector calls in a short time; wait a few seconds and retry."
                    .to_string(),
                true,
            )
            .into());
        }
        if self.limiter.failed_claims_exhausted() {
            return Ok(text_result(
                "Too many invalid turn_tokens recently; wait a minute before retrying.".to_string(),
                true,
            )
            .into());
        }

        let parsed = contract::parse(&request.name, request.arguments.as_ref())?;
        let claim = match self.broker.claim(&parsed.turn_token) {
            Ok(claim) => claim,
            Err(error) => {
                self.limiter.record_failed_claim();
                tracing::info!(
                    request = %request_tag,
                    token = %log_hash(&parsed.turn_token),
                    "chatgpt_web connector: claim refused: {error:?}"
                );
                return Ok(text_result(error.to_string(), true).into());
            }
        };
        tracing::debug!(
            request = %request_tag,
            tool = %request.name,
            binding = %claim.binding,
            "chatgpt_web connector: call"
        );
        match contract::to_call(&parsed, &claim.tools)? {
            contract::Resolved::Local(result) => Ok(result.into()),
            contract::Resolved::Forward(target) => {
                let result = self.broker.invoke(&claim.binding, target).await;
                Ok(to_call_tool_result(result).into())
            }
        }
    }
}

#[cfg(test)]
#[path = "public_server_tests.rs"]
mod tests;
