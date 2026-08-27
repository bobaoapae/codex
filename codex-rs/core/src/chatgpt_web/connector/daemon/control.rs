//! FORK: the daemon's loopback control API for Codex sessions.
//!
//! JSON over HTTP with a bearer token from `daemon.token`; the long-poll
//! `GET /v1/sessions/{sid}/calls` is how tool calls reach a session. Only
//! `GET /healthz` is unauthenticated, so `status`/autostart can probe it.

use super::broker::CompleteError;
use super::broker::RegisterTurnError;
use super::broker::TurnBroker;
use super::broker::TurnRegistration;
use super::state::RegistryStatus;
use super::tunnel::TunnelState;
use super::wire::CallResultRequest;
use super::wire::CallsQuery;
use super::wire::CallsResponse;
use super::wire::EndTurnRequest;
use super::wire::HealthResponse;
use super::wire::OkResponse;
use super::wire::RegisterSessionRequest;
use super::wire::RegisterSessionResponse;
use super::wire::RegisterTurnRequest;
use super::wire::RegisterTurnResponse;
use axum::Json;
use axum::Router;
use axum::body::Body;
use axum::extract::Path;
use axum::extract::Query;
use axum::extract::State;
use axum::http::HeaderValue;
use axum::http::Request;
use axum::http::StatusCode;
use axum::http::header;
use axum::middleware;
use axum::middleware::Next;
use axum::response::IntoResponse;
use axum::response::Response;
use axum::routing::delete;
use axum::routing::get;
use axum::routing::post;
use futures::future::BoxFuture;
use serde_json::json;
use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use std::time::Duration;
use tokio::net::TcpListener;
use tokio::sync::watch;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

/// Longest a long-poll may block.
pub const MAX_POLL_WAIT: Duration = Duration::from_secs(30);
const DEFAULT_POLL_WAIT: Duration = Duration::from_secs(25);

/// Registry hook: C2 supplies the reconcile; until then `None` → 501.
pub type ReconcileHook =
    Arc<dyn Fn() -> BoxFuture<'static, Result<RegistryStatus, String>> + Send + Sync>;

/// Shared by every handler.
pub struct ControlState {
    pub broker: Arc<TurnBroker>,
    pub token: String,
    pub version: String,
    pub tunnel: watch::Receiver<TunnelState>,
    pub registry: Arc<Mutex<RegistryStatus>>,
    pub reconcile: Option<ReconcileHook>,
    /// Cancels the whole daemon.
    pub shutdown: CancellationToken,
    pub shutdown_when_idle: AtomicBool,
}

impl ControlState {
    pub fn registry_status(&self) -> RegistryStatus {
        self.registry
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    pub fn set_registry_status(&self, status: RegistryStatus) {
        *self
            .registry
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = status;
    }

    pub fn health(&self) -> HealthResponse {
        let (sessions, active_turns) = self.broker.stats();
        let tunnel = self.tunnel.borrow().clone();
        HealthResponse {
            ok: true,
            pid: std::process::id(),
            version: self.version.clone(),
            public_url: tunnel.endpoint().map(|endpoint| endpoint.public_label()),
            registry_status: self.registry_status().label().to_string(),
            tunnel_state: tunnel.label(),
            sessions,
            active_turns,
        }
    }
}

fn json_status(status: StatusCode, body: serde_json::Value) -> Response {
    let mut response = Response::new(Body::from(body.to_string()));
    *response.status_mut() = status;
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/json"),
    );
    response
}

fn error(status: StatusCode, message: impl Into<String>) -> Response {
    json_status(status, json!({ "error": message.into() }))
}

async fn require_bearer(
    State(state): State<Arc<ControlState>>,
    request: Request<Body>,
    next: Next,
) -> Response {
    if request.uri().path() == "/healthz" {
        return next.run(request).await;
    }
    let expected = format!("Bearer {}", state.token);
    let authorized = request
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| super::public_server::secrets_match(value, &expected));
    if authorized {
        next.run(request).await
    } else {
        error(StatusCode::UNAUTHORIZED, "missing or invalid bearer token")
    }
}

async fn healthz(State(state): State<Arc<ControlState>>) -> Json<HealthResponse> {
    Json(state.health())
}

async fn register_session(
    State(state): State<Arc<ControlState>>,
    Json(request): Json<RegisterSessionRequest>,
) -> Response {
    if request.session_id.trim().is_empty() {
        return error(StatusCode::BAD_REQUEST, "session_id is required");
    }
    let session_token = state
        .broker
        .register_session(&request.session_id, request.codex_pid);
    Json(RegisterSessionResponse {
        session_token,
        poll_url: format!("/v1/sessions/{}/calls", request.session_id),
    })
    .into_response()
}

async fn delete_session(
    State(state): State<Arc<ControlState>>,
    Path(session_id): Path<String>,
) -> Response {
    let removed = state
        .broker
        .remove_session(&session_id, "Codex session disconnected");
    if removed {
        Json(OkResponse { ok: true }).into_response()
    } else {
        error(StatusCode::NOT_FOUND, "unknown session")
    }
}

async fn heartbeat(
    State(state): State<Arc<ControlState>>,
    Path(session_id): Path<String>,
) -> Response {
    match state.broker.heartbeat(&session_id) {
        Ok(()) => Json(OkResponse { ok: true }).into_response(),
        Err(_) => error(StatusCode::NOT_FOUND, "unknown session"),
    }
}

async fn register_turn(
    State(state): State<Arc<ControlState>>,
    Json(request): Json<RegisterTurnRequest>,
) -> Response {
    let registration = TurnRegistration {
        session_id: request.session_id,
        turn_token: request.turn_token,
        trace: format!("{}/{}", request.thread_id, request.turn_id),
        ttl: Duration::from_millis(request.ttl_ms.max(1_000)),
        tools: request.tools.into(),
        exec_tool: request.exec_tool,
        apply_patch: request.apply_patch,
    };
    match state.broker.register_turn(registration) {
        Ok(()) => Json(RegisterTurnResponse {
            registry_status: state.registry_status().label().to_string(),
            tunnel_state: state.tunnel.borrow().label(),
        })
        .into_response(),
        Err(RegisterTurnError::Duplicate) => {
            error(StatusCode::CONFLICT, "turn_token already registered")
        }
        Err(RegisterTurnError::UnknownSession) => {
            error(StatusCode::NOT_FOUND, "unknown session; register it first")
        }
    }
}

async fn end_turn(
    State(state): State<Arc<ControlState>>,
    Path(turn_token): Path<String>,
    body: Option<Json<EndTurnRequest>>,
) -> Response {
    let reason = body
        .and_then(|Json(body)| body.reason)
        .unwrap_or_else(|| "the Codex turn finished".to_string());
    if state.broker.revoke(&turn_token, &reason) {
        Json(OkResponse { ok: true }).into_response()
    } else {
        error(StatusCode::NOT_FOUND, "unknown turn_token")
    }
}

async fn poll_calls(
    State(state): State<Arc<ControlState>>,
    Path(session_id): Path<String>,
    Query(query): Query<CallsQuery>,
) -> Response {
    let wait = query
        .wait_ms
        .map(Duration::from_millis)
        .unwrap_or(DEFAULT_POLL_WAIT)
        .min(MAX_POLL_WAIT);
    match state
        .broker
        .next_batches(&session_id, query.after, wait)
        .await
    {
        Ok(response) => Json::<CallsResponse>(response).into_response(),
        Err(_) => error(StatusCode::NOT_FOUND, "unknown session"),
    }
}

async fn post_result(
    State(state): State<Arc<ControlState>>,
    Path(call_id): Path<String>,
    Json(request): Json<CallResultRequest>,
) -> Response {
    let result = super::broker::BrokerResult {
        content: request.content,
        is_error: request.is_error,
        structured: request.structured,
    };
    match state.broker.complete(&request.session_id, &call_id, result) {
        Ok(()) => Json(OkResponse { ok: true }).into_response(),
        Err(CompleteError::UnknownCall) => error(StatusCode::NOT_FOUND, "unknown call_id"),
        Err(CompleteError::WrongSession) => {
            error(StatusCode::FORBIDDEN, "call_id belongs to another session")
        }
        Err(CompleteError::NotInFlight) => error(
            StatusCode::CONFLICT,
            "call_id was not delivered to the session",
        ),
    }
}

async fn reconcile(State(state): State<Arc<ControlState>>) -> Response {
    let Some(hook) = state.reconcile.clone() else {
        return error(
            StatusCode::NOT_IMPLEMENTED,
            "connector registry is not implemented in this build",
        );
    };
    match hook().await {
        Ok(status) => {
            let label = status.label().to_string();
            let body = serde_json::to_value(&status).unwrap_or_else(|_| json!({}));
            state.set_registry_status(status);
            json_status(
                StatusCode::OK,
                json!({ "registry_status": label, "detail": body }),
            )
        }
        Err(reason) => error(StatusCode::BAD_GATEWAY, reason),
    }
}

async fn shutdown_when_idle(State(state): State<Arc<ControlState>>) -> Response {
    state.shutdown_when_idle.store(true, Ordering::SeqCst);
    let (sessions, turns) = state.broker.stats();
    if sessions == 0 && turns == 0 {
        state.shutdown.cancel();
    }
    Json(OkResponse { ok: true }).into_response()
}

pub fn router(state: Arc<ControlState>) -> Router {
    Router::new()
        .route("/healthz", get(healthz))
        .route("/v1/sessions", post(register_session))
        .route("/v1/sessions/{sid}", delete(delete_session))
        .route("/v1/sessions/{sid}/heartbeat", post(heartbeat))
        .route("/v1/sessions/{sid}/calls", get(poll_calls))
        .route("/v1/turns", post(register_turn))
        .route("/v1/turns/{turn_token}", delete(end_turn))
        .route("/v1/calls/{call_id}/result", post(post_result))
        .route("/v1/registry/reconcile", post(reconcile))
        .route("/v1/admin/shutdown_when_idle", post(shutdown_when_idle))
        .layer(middleware::from_fn_with_state(
            Arc::clone(&state),
            require_bearer,
        ))
        .with_state(state)
}

/// Binds `127.0.0.1:{port}` and serves until `cancel` fires.
pub async fn start(
    state: Arc<ControlState>,
    port: u16,
    cancel: CancellationToken,
) -> std::io::Result<(SocketAddr, JoinHandle<()>)> {
    let listener = TcpListener::bind(("127.0.0.1", port)).await?;
    let addr = listener.local_addr()?;
    let app = router(state);
    let task = tokio::spawn(async move {
        let serve = axum::serve(listener, app).with_graceful_shutdown(async move {
            cancel.cancelled().await;
        });
        if let Err(error) = serve.await {
            tracing::warn!("chatgpt_web control API stopped: {error}");
        }
    });
    Ok((addr, task))
}
