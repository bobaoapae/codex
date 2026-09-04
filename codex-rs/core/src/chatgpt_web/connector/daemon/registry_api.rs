//! FORK: `ConnectorApi` over a real chatgpt.com tab.
//!
//! Every op is one `fetch` issued from inside the page (so the browser's
//! cookies and the `/api/auth/session` bearer apply) through the chrome-mcp
//! daemon's `browser_eval`, using the driver's `api_call_with_headers` page
//! script plus `OAI-Product-Sku: CONNECTOR_SETTING`. Paths, bodies and
//! response shapes are the ones captured live in
//! `docs/plans/2026-08-26-chatgpt-web/api_shapes.md`.
//!
//! The tab: an existing chatgpt.com tab is borrowed for evals only (never
//! navigated, never typed into); when there is none, a dedicated tab is created,
//! registered in the shared `tabs.json` under this daemon's pid so a Node
//! `chatgpt-pro-mcp` or a Codex session never adopts it mid-use, and closed
//! again after the reconcile.

use super::registry::AccountInfo;
use super::registry::ApiError;
use super::registry::ApiResult;
use super::registry::ConnectorApi;
use super::registry::DesiredConnector;
use super::registry::ObservedConnector;
use super::registry::ObservedLink;
use super::registry::RegistryOp;
use super::tunnel::TunnelEndpoint;
use crate::chatgpt_web::driver::DriverError;
use crate::chatgpt_web::driver::DriverErrorKind;
use crate::chatgpt_web::driver::daemon::DEFAULT_TOOL_TIMEOUT_MS;
use crate::chatgpt_web::driver::daemon::DaemonClient;
use crate::chatgpt_web::driver::daemon::DaemonConfig;
use crate::chatgpt_web::driver::page_scripts;
use crate::chatgpt_web::driver::tabs;
use crate::chatgpt_web::driver::tabs::TabDaemon;
use crate::chatgpt_web::driver::tabs::TabId;
use crate::config::ChatGptWebSettings;
use futures::future::BoxFuture;
use serde::Deserialize;
use serde_json::Map;
use serde_json::Value;
use serde_json::json;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Mutex;
use std::time::Duration;

/// Page-side timeout of one API fetch.
const API_EVAL_TIMEOUT_MS: u64 = 30_000;
const LOGIN_PROBE_TIMEOUT_MS: u64 = 8_000;
/// How long a freshly created tab may take to finish loading.
const TAB_LOAD_TIMEOUT: Duration = Duration::from_secs(25);
/// 429 backoff, as in `api.ts`.
const RATE_LIMIT_BACKOFF: [Duration; 3] = [
    Duration::from_secs(2),
    Duration::from_secs(5),
    Duration::from_secs(10),
];
const PRODUCT_SKU_HEADER: &str = "OAI-Product-Sku";
const PRODUCT_SKU_CONNECTOR_SETTING: &str = "CONNECTOR_SETTING";

/// The tab a reconcile runs in.
#[derive(Debug, Clone)]
struct TabLease {
    tab_id: TabId,
    /// Created by us (close + unregister on `close`) vs borrowed.
    created: bool,
}

/// `ConnectorApi` over chrome-mcp.
pub struct ChromeMcpPageApi {
    daemon: Arc<dyn TabDaemon>,
    base_url: String,
    registry_path: Option<PathBuf>,
    lease: Mutex<Option<TabLease>>,
    backoff: Vec<Duration>,
}

impl std::fmt::Debug for ChromeMcpPageApi {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ChromeMcpPageApi")
            .field("base_url", &self.base_url)
            .finish_non_exhaustive()
    }
}

/// The `Arc<dyn HttpClient>` the daemon client needs for a loopback URL.
pub fn loopback_http_client() -> Arc<dyn codex_exec_server::HttpClient> {
    use codex_exec_server::RouteAwareHttpClient;
    use codex_http_client::HttpClientFactory;
    use codex_http_client::OutboundProxyPolicy;
    Arc::new(
        RouteAwareHttpClient::new(HttpClientFactory::new(OutboundProxyPolicy::ReqwestDefault))
            .with_tls_backend_fallback(),
    )
}

impl ChromeMcpPageApi {
    pub(crate) fn new(daemon: Arc<DaemonClient>, base_url: &str) -> Self {
        Self::with_daemon(daemon, base_url)
    }

    /// Same over any [`TabDaemon`] (tests use a fake).
    pub(crate) fn with_daemon(daemon: Arc<dyn TabDaemon>, base_url: &str) -> Self {
        Self {
            daemon,
            base_url: base_url.trim_end_matches('/').to_string(),
            registry_path: tabs::default_registry_path(),
            lease: Mutex::new(None),
            backoff: RATE_LIMIT_BACKOFF.to_vec(),
        }
    }

    /// Builds the daemon client from `[chatgpt_web]` settings.
    pub fn from_settings(settings: &ChatGptWebSettings) -> Self {
        let config = DaemonConfig::resolve(&settings.daemon_url, settings.token_file.as_deref());
        let daemon = Arc::new(DaemonClient::new(config, loopback_http_client()));
        Self::new(daemon, &settings.base_url)
    }

    #[cfg(test)]
    pub(crate) fn with_backoff(mut self, backoff: Vec<Duration>) -> Self {
        self.backoff = backoff;
        self
    }

    #[cfg(test)]
    pub(crate) fn with_registry_path(mut self, path: Option<PathBuf>) -> Self {
        self.registry_path = path;
        self
    }

    fn lease(&self) -> Option<TabLease> {
        self.lease
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    fn set_lease(&self, lease: Option<TabLease>) {
        *self
            .lease
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = lease;
    }

    fn is_our_origin(&self, url: &str) -> bool {
        url.starts_with(&self.base_url)
            && url[self.base_url.len()..]
                .chars()
                .next()
                .is_none_or(|next| next == '/' || next == '?' || next == '#')
    }

    /// Borrows an existing chatgpt.com tab, or creates a dedicated one.
    async fn acquire_tab(&self) -> Result<TabLease, ApiError> {
        if let Some(lease) = self.lease() {
            return Ok(lease);
        }
        let listed = self
            .daemon
            .call(
                "browser_tabs",
                json!({"action": "list"}),
                DEFAULT_TOOL_TIMEOUT_MS,
            )
            .await
            .map_err(map_driver_error)?;
        let tabs: Vec<tabs::TabInfo> = listed
            .json()
            .and_then(|value| serde_json::from_value(value).ok())
            .unwrap_or_default();
        // FORK (verified live): a chatgpt.com tab is not necessarily logged in
        // — another Chrome window/profile in the same browser answers
        // `/api/auth/session` without a token — and borrowing it made every
        // reconcile fail with "not logged in" while the driver's own tab was
        // fine. Probe each candidate first; fall back to a dedicated tab.
        for tab_id in tabs
            .iter()
            .filter(|tab| {
                tab.url
                    .as_deref()
                    .is_some_and(|url| self.is_our_origin(url))
            })
            .filter_map(|tab| tab.id)
        {
            if !self.tab_logged_in(tab_id).await {
                tracing::debug!(
                    "chatgpt_web registry: skipping chatgpt.com tab {tab_id} (no session token)"
                );
                continue;
            }
            let lease = TabLease {
                tab_id,
                created: false,
            };
            self.set_lease(Some(lease.clone()));
            return Ok(lease);
        }

        let created = self
            .daemon
            .call(
                "browser_tabs",
                json!({
                    "action": "create",
                    "url": format!("{}/", self.base_url),
                    "dedicated": true,
                }),
                DEFAULT_TOOL_TIMEOUT_MS,
            )
            .await
            .map_err(map_driver_error)?;
        let tab_id = created
            .json()
            .and_then(|value| value.get("id").and_then(Value::as_i64))
            .ok_or_else(|| {
                ApiError::browser_unavailable(format!(
                    "browser_tabs create returned no tab id: {}",
                    created.text
                ))
            })?;
        self.register_tab(tab_id).await;
        let lease = TabLease {
            tab_id,
            created: true,
        };
        self.set_lease(Some(lease.clone()));
        self.wait_loaded(tab_id).await?;
        Ok(lease)
    }

    /// Whether `tab_id` holds a logged-in chatgpt.com session (a token from
    /// `/api/auth/session`). Read-only; never navigates the tab.
    async fn tab_logged_in(&self, tab_id: TabId) -> bool {
        const PROBE: &str = r#"/* codex-login-probe */ () => fetch('/api/auth/session', { credentials: 'include' })
  .then((r) => r.json())
  .then((s) => JSON.stringify({ ok: !!(s && s.accessToken) }))
  .catch(() => JSON.stringify({ ok: false }))"#;
        match self
            .daemon
            .eval_in(tab_id, PROBE.to_string(), LOGIN_PROBE_TIMEOUT_MS)
            .await
        {
            Ok(value) => decode_page_json(value)
                .get("ok")
                .and_then(Value::as_bool)
                .unwrap_or(false),
            Err(error) => {
                tracing::debug!(
                    "chatgpt_web registry: login probe on tab {tab_id} failed: {error}"
                );
                false
            }
        }
    }

    /// Records the dedicated tab in the shared registry under our pid.
    async fn register_tab(&self, tab_id: TabId) {
        let Some(path) = self.registry_path.clone() else {
            return;
        };
        let pid = std::process::id();
        let outcome = tabs::with_registry_lock(&path, tabs::RegistryLockOptions::default(), || {
            let mut registry = tabs::load_registry(&path);
            registry.owners.retain(|owner| owner.tab_id != tab_id);
            registry.owners.push(tabs::OwnerEntry {
                tab_id,
                pid: Some(pid),
                since: super::state::now_ms(),
            });
            tabs::save_registry(&path, &registry)
        })
        .await;
        if let Err(error) =
            outcome.and_then(|saved| saved.map_err(|e| DriverError::other(e.to_string())))
        {
            tracing::warn!("chatgpt_web registry: could not record the tab in tabs.json: {error}");
        }
    }

    async fn unregister_tab(&self, tab_id: TabId) {
        let Some(path) = self.registry_path.clone() else {
            return;
        };
        let _ = tabs::with_registry_lock(&path, tabs::RegistryLockOptions::default(), || {
            let mut registry = tabs::load_registry(&path);
            registry.owners.retain(|owner| owner.tab_id != tab_id);
            let _ = tabs::save_registry(&path, &registry);
        })
        .await;
    }

    /// Waits for `document.readyState === "complete"` (retrying while the
    /// page has no execution context yet).
    async fn wait_loaded(&self, tab_id: TabId) -> Result<(), ApiError> {
        let deadline = tokio::time::Instant::now() + TAB_LOAD_TIMEOUT;
        loop {
            let probe = self
                .daemon
                .eval_in(
                    tab_id,
                    "() => JSON.stringify({ ready: document.readyState === 'complete', path: location.pathname, href: location.href })".to_string(),
                    5_000,
                )
                .await;
            match probe {
                Ok(value) => {
                    let value = decode_page_json(value);
                    // FORK (verified live): a freshly created tab reports
                    // `readyState === "complete"` while it is still
                    // `about:blank`, and a relative `fetch` there fails with
                    // "Failed to parse URL". Wait until it is actually on our
                    // origin.
                    let on_origin = value
                        .get("href")
                        .and_then(Value::as_str)
                        .is_some_and(|href| self.is_our_origin(href));
                    if value.get("ready").and_then(Value::as_bool) == Some(true) && on_origin {
                        return Ok(());
                    }
                }
                Err(error) if error.kind == DriverErrorKind::DaemonDown => {
                    return Err(map_driver_error(error));
                }
                Err(_) => {}
            }
            if tokio::time::Instant::now() >= deadline {
                return Err(ApiError::browser_unavailable(format!(
                    "the chatgpt.com tab {tab_id} did not finish loading within {}s",
                    TAB_LOAD_TIMEOUT.as_secs()
                )));
            }
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
    }

    /// One page fetch with the connector-settings headers; 429s are retried
    /// with the backoff, everything else is returned as-is.
    async fn fetch(
        &self,
        method: &str,
        path: &str,
        body: Option<&Value>,
    ) -> Result<PageResponse, ApiError> {
        let lease = self.acquire_tab().await?;
        let mut headers = Map::new();
        headers.insert(
            PRODUCT_SKU_HEADER.to_string(),
            Value::String(PRODUCT_SKU_CONNECTOR_SETTING.to_string()),
        );
        let url = format!("{}{path}", self.base_url);
        let mut attempt = 0usize;
        loop {
            let script = page_scripts::api_call_with_headers(&url, method, body, &headers);
            let raw = self
                .daemon
                .eval_in(lease.tab_id, script, API_EVAL_TIMEOUT_MS)
                .await
                .map_err(map_driver_error)?;
            let response: PageResponse =
                serde_json::from_value(decode_page_json(raw)).map_err(|error| {
                    ApiError::new(format!(
                        "ChatGPT API {method} {path}: unexpected page result ({error})"
                    ))
                })?;
            if let Some(error) = response.error.as_deref().filter(|e| !e.is_empty()) {
                let login_required = error.contains("not logged in");
                if login_required && !lease.created {
                    // The borrowed tab lost its session; pick again next time.
                    self.set_lease(None);
                }
                return Err(ApiError {
                    status: None,
                    message: format!("ChatGPT API {method} {path} failed in page: {error}"),
                    login_required,
                    ..ApiError::default()
                });
            }
            if response.status == 429 && attempt < self.backoff.len() {
                let delay = self.backoff[attempt];
                tracing::info!(
                    "chatgpt_web registry: {method} {path} → 429, retrying in {}ms",
                    delay.as_millis()
                );
                tokio::time::sleep(delay).await;
                attempt += 1;
                continue;
            }
            return Ok(response);
        }
    }

    /// `fetch` + status check; non-2xx becomes an `ApiError` classified by
    /// status and detail.
    async fn fetch_ok(
        &self,
        method: &str,
        path: &str,
        body: Option<&Value>,
    ) -> Result<Value, ApiError> {
        let response = self.fetch(method, path, body).await?;
        if (200..300).contains(&response.status) {
            return Ok(response.json.unwrap_or(Value::Null));
        }
        Err(http_error(method, path, &response))
    }
}

/// What the page script resolves.
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct PageResponse {
    status: u16,
    json: Option<Value>,
    text: Option<String>,
    error: Option<String>,
}

fn decode_page_json(value: Value) -> Value {
    match value {
        Value::String(text) => serde_json::from_str(&text).unwrap_or(Value::String(text)),
        other => other,
    }
}

fn truncate(text: &str, max: usize) -> String {
    text.chars().take(max).collect()
}

fn http_error(method: &str, path: &str, response: &PageResponse) -> ApiError {
    let detail = response
        .json
        .as_ref()
        .and_then(|json| json.get("detail"))
        .map(|detail| match detail {
            Value::String(text) => text.clone(),
            other => other.to_string(),
        })
        .or_else(|| response.text.clone())
        .unwrap_or_default();
    let developer_mode_required =
        response.status == 403 && detail.to_ascii_lowercase().contains("developer mode");
    ApiError {
        status: Some(response.status),
        message: format!(
            "ChatGPT API {method} {path} → HTTP {}: {}",
            response.status,
            truncate(&detail, 300)
        ),
        developer_mode_required,
        rate_limited: response.status == 429,
        login_required: response.status == 401,
        browser_unavailable: false,
    }
}

fn map_driver_error(error: DriverError) -> ApiError {
    let browser_unavailable = matches!(
        error.kind,
        DriverErrorKind::DaemonDown | DriverErrorKind::Timeout | DriverErrorKind::Tool
    );
    ApiError {
        status: None,
        message: format!("chrome-mcp: {error}"),
        developer_mode_required: false,
        rate_limited: false,
        browser_unavailable,
        login_required: error.kind == DriverErrorKind::LoginRequired,
    }
}

fn string_field(value: &Value, keys: &[&str]) -> Option<String> {
    keys.iter()
        .find_map(|key| value.get(*key).and_then(Value::as_str))
        .map(str::to_string)
}

fn action_names(value: &Value) -> Vec<String> {
    value
        .get("actions")
        .and_then(Value::as_array)
        .map(|actions| {
            actions
                .iter()
                .filter_map(|action| match action {
                    Value::String(name) => Some(name.clone()),
                    other => string_field(other, &["name"]),
                })
                .collect()
        })
        .unwrap_or_default()
}

fn parse_connectors(value: &Value) -> Vec<ObservedConnector> {
    value
        .get("connectors")
        .and_then(Value::as_array)
        .map(|connectors| {
            connectors
                .iter()
                .filter_map(|connector| {
                    Some(ObservedConnector {
                        id: string_field(connector, &["id", "connector_id"])?,
                        name: string_field(connector, &["name"]).unwrap_or_default(),
                        mcp_url: string_field(connector, &["base_url", "mcp_url"]),
                        tunnel_id: string_field(connector, &["tunnel_id"]),
                        actions: action_names(connector),
                    })
                })
                .collect()
        })
        .unwrap_or_default()
}

fn parse_links(value: &Value) -> Vec<ObservedLink> {
    value
        .get("links")
        .and_then(Value::as_array)
        .map(|links| {
            links
                .iter()
                .filter_map(|link| {
                    Some(ObservedLink {
                        link_id: string_field(link, &["id", "link_id"])?,
                        connector_id: string_field(link, &["connector_id"])?,
                        name: string_field(link, &["name"]).unwrap_or_default(),
                    })
                })
                .collect()
        })
        .unwrap_or_default()
}

fn parse_tunnels(value: &Value) -> Vec<String> {
    value
        .get("tunnels")
        .and_then(Value::as_array)
        .map(|tunnels| {
            tunnels
                .iter()
                .filter_map(|tunnel| match tunnel {
                    Value::String(id) => Some(id.clone()),
                    other => string_field(other, &["id", "tunnel_id"]),
                })
                .collect()
        })
        .unwrap_or_default()
}

/// FORK: `accounts/check` names the account and its plan; `auth/session` adds
/// the email. Both shapes have moved before, so every field is optional.
fn parse_account(check: &Value, session: Option<&Value>) -> AccountInfo {
    let account_id = check
        .get("account_ordering")
        .and_then(Value::as_array)
        .and_then(|ordering| ordering.first())
        .and_then(Value::as_str)
        .map(str::to_string)
        .unwrap_or_default();
    let plan_type = check
        .get("accounts")
        .and_then(Value::as_object)
        .and_then(|accounts| {
            accounts
                .get(&account_id)
                .or_else(|| accounts.values().next())
        })
        .and_then(|account| account.get("account"))
        .and_then(|account| string_field(account, &["plan_type", "structure"]));
    let email = session
        .and_then(|session| session.get("user"))
        .and_then(|user| string_field(user, &["email"]));
    AccountInfo {
        account_id,
        email,
        plan_type,
    }
}

fn create_body(desired: &DesiredConnector) -> Value {
    let mut body = json!({
        "name": desired.display_name(),
        "description": desired.description,
        "logo_url": Value::Null,
        // "Sem autenticação": the UI sends an empty list and the server
        // normalizes it to `[{"type":"NONE"}]`.
        "auth_request": { "supported_auth": [], "oauth_client_params": Value::Null },
    });
    match &desired.endpoint {
        TunnelEndpoint::OpenAi { tunnel_id } => {
            body["tunnel_id"] = Value::String(tunnel_id.clone());
        }
        TunnelEndpoint::Public { mcp_url } => {
            body["mcp_url"] = Value::String(mcp_url.clone());
        }
    }
    body
}

/// Deletes are idempotent: a 404 means it is already gone.
fn ok_or_gone(result: Result<Value, ApiError>) -> Result<ApiResult, ApiError> {
    match result {
        Ok(_) => Ok(ApiResult::Unit),
        Err(error) if error.status == Some(404) => Ok(ApiResult::Unit),
        Err(error) => Err(error),
    }
}

impl ConnectorApi for ChromeMcpPageApi {
    fn open(&self) -> BoxFuture<'_, Result<(), ApiError>> {
        Box::pin(async move { self.acquire_tab().await.map(|_| ()) })
    }

    fn call<'a>(&'a self, op: &'a RegistryOp) -> BoxFuture<'a, Result<ApiResult, ApiError>> {
        Box::pin(async move {
            match op {
                RegistryOp::ReadDeveloperMode => {
                    let settings = self
                        .fetch_ok("GET", "/backend-api/settings/user", None)
                        .await?;
                    // Absent until toggled once, which the UI treats as off.
                    let enabled = settings
                        .get("settings")
                        .and_then(|settings| settings.get("developer_mode"))
                        .and_then(Value::as_bool)
                        .unwrap_or(false);
                    Ok(ApiResult::DeveloperMode(enabled))
                }
                RegistryOp::EnableDeveloperMode => {
                    let answer = self
                        .fetch_ok(
                            "PATCH",
                            "/backend-api/settings/account_user_setting?feature=developer_mode&value=true",
                            None,
                        )
                        .await?;
                    let enabled = answer
                        .get("developer_mode")
                        .and_then(Value::as_bool)
                        .unwrap_or(true);
                    Ok(ApiResult::DeveloperMode(enabled))
                }
                RegistryOp::ListConnectors => {
                    let answer = self
                        .fetch_ok(
                            "POST",
                            "/backend-api/aip/connectors/list_accessible?include_actions=false&external_logos=true&skip_directory=true",
                            Some(&json!({ "principals": [] })),
                        )
                        .await?;
                    Ok(ApiResult::Connectors(parse_connectors(&answer)))
                }
                RegistryOp::ListLinks => {
                    let answer = self
                        .fetch_ok(
                            "POST",
                            "/backend-api/aip/connectors/links/list_accessible",
                            Some(&json!({ "principals": [], "link_refresh_strategy": "NONE" })),
                        )
                        .await?;
                    Ok(ApiResult::Links(parse_links(&answer)))
                }
                RegistryOp::ListTunnels => {
                    let answer = self
                        .fetch_ok("GET", "/backend-api/aip/connectors/mcp/tunnels", None)
                        .await?;
                    Ok(ApiResult::Tunnels(parse_tunnels(&answer)))
                }
                // FORK: which account this Chrome session is. The email comes
                // from a second endpoint and is best-effort — the account id is
                // the part that matches what the tunnel's audience lists.
                RegistryOp::ReadAccount => {
                    let account = self
                        .fetch_ok(
                            "GET",
                            "/backend-api/accounts/check/v4-2023-04-27",
                            None,
                        )
                        .await?;
                    let session = self.fetch_ok("GET", "/api/auth/session", None).await.ok();
                    Ok(ApiResult::Account(parse_account(&account, session.as_ref())))
                }
                RegistryOp::DeleteLink(link_id) => ok_or_gone(
                    self.fetch_ok(
                        "DELETE",
                        &format!("/backend-api/aip/connectors/links/{link_id}"),
                        None,
                    )
                    .await,
                ),
                RegistryOp::DeleteConnector(connector_id) => ok_or_gone(
                    self.fetch_ok(
                        "DELETE",
                        &format!("/backend-api/aip/connectors/{connector_id}"),
                        None,
                    )
                    .await,
                ),
                RegistryOp::Create(desired) => {
                    let answer = self
                        .fetch_ok(
                            "POST",
                            "/backend-api/aip/connectors/mcp",
                            Some(&create_body(desired)),
                        )
                        .await?;
                    let connector = answer.get("connector").unwrap_or(&answer);
                    let connector_id = string_field(connector, &["id", "connector_id"])
                        .ok_or_else(|| {
                            ApiError::new(format!(
                                "connector create returned no id: {}",
                                truncate(&answer.to_string(), 300)
                            ))
                        })?;
                    Ok(ApiResult::Created {
                        connector_id,
                        actions: action_names(connector),
                    })
                }
                RegistryOp::Link { connector, name } => {
                    let connector_id = match connector {
                        super::registry::ConnectorRef::Id(id) => id.clone(),
                        super::registry::ConnectorRef::Created => {
                            return Err(ApiError::new("Link needs a resolved connector id"));
                        }
                    };
                    let answer = self
                        .fetch_ok(
                            "POST",
                            "/backend-api/aip/connectors/links/noauth",
                            Some(&json!({
                                "connector_id": connector_id,
                                "name": name,
                                "action_names": [],
                            })),
                        )
                        .await?;
                    let link_id = string_field(&answer, &["id", "link_id"]).ok_or_else(|| {
                        ApiError::new(format!(
                            "link create returned no id: {}",
                            truncate(&answer.to_string(), 300)
                        ))
                    })?;
                    Ok(ApiResult::Linked {
                        link_id,
                        actions: action_names(&answer),
                    })
                }
                RegistryOp::RefreshActions { link } => {
                    let link_id = match link {
                        super::registry::LinkRef::Id(id) => id.clone(),
                        super::registry::LinkRef::Created => {
                            return Err(ApiError::new("RefreshActions needs a resolved link id"));
                        }
                    };
                    let answer = self
                        .fetch_ok(
                            "POST",
                            "/backend-api/aip/connectors/mcp/refresh_actions",
                            Some(&json!({ "link_id": link_id })),
                        )
                        .await?;
                    Ok(ApiResult::Actions(action_names(&answer)))
                }
                RegistryOp::VerifyActions { connector, .. } => {
                    let connector_id = match connector {
                        super::registry::ConnectorRef::Id(id) => id.clone(),
                        super::registry::ConnectorRef::Created => {
                            return Err(ApiError::new(
                                "VerifyActions needs a resolved connector id",
                            ));
                        }
                    };
                    let answer = self
                        .fetch_ok(
                            "GET",
                            &format!("/backend-api/aip/connectors/{connector_id}/actions"),
                            None,
                        )
                        .await?;
                    Ok(ApiResult::Actions(action_names(&answer)))
                }
                RegistryOp::Persist { .. } => Ok(ApiResult::Unit),
            }
        })
    }

    fn close(&self) -> BoxFuture<'_, ()> {
        Box::pin(async move {
            let Some(lease) = self.lease() else {
                return;
            };
            self.set_lease(None);
            if !lease.created {
                return;
            }
            if let Err(error) = self
                .daemon
                .call(
                    "browser_tabs",
                    json!({"action": "close", "tabId": lease.tab_id}),
                    DEFAULT_TOOL_TIMEOUT_MS,
                )
                .await
            {
                tracing::warn!(
                    "chatgpt_web registry: could not close tab {}: {error}",
                    lease.tab_id
                );
            }
            self.unregister_tab(lease.tab_id).await;
        })
    }
}
