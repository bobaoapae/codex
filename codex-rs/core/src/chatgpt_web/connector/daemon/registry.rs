//! FORK: the connector registry — keeps exactly one ChatGPT-side connector
//! (`[chatgpt_web] connector_name`, "Codex Native" by default) pointing at the
//! daemon's current endpoint, with the fixed 6-action contract.
//!
//! Two layers, so the decision logic is testable without a browser:
//!
//! - a pure **planner** (`plan`) that turns what the account currently has
//!   (`Observed`) plus what we want (`DesiredConnector`) into a list of
//!   `RegistryOp`s;
//! - an **executor** that runs those ops through a `ConnectorApi` (the real
//!   one, `registry_api::ChromeMcpPageApi`, drives chatgpt.com's backend from
//!   a page `fetch` through the chrome-mcp daemon; tests use a fake), then
//!   persists `connector.json`.
//!
//! Endpoint facts come from the live captures in
//! `docs/plans/2026-08-26-chatgpt-web/api_shapes.md`: there is no `purpose`
//! field, connectors and links have separate `list_accessible` endpoints,
//! connector ids look like `asdk_app_<hex>`, and `create` already returns the
//! action list. Developer Mode is a user setting the daemon can switch on by
//! itself (`PATCH /backend-api/settings/account_user_setting`).

use super::control::ReconcileHook;
use super::control::ReconcileTrigger;
use super::state::ConnectorRecord;
use super::state::FailureKind;
use super::state::RegistryStatus;
use super::state::now_ms;
use super::tunnel::TunnelEndpoint;
use super::tunnel::TunnelState;
use crate::chatgpt_web::connector::contract;
use futures::future::BoxFuture;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;
use std::time::Duration;
use tokio::sync::Semaphore;
use tokio::sync::watch;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

/// Backoff between failed reconciles, then `FAILURE_BACKOFF_CAP`.
///
/// FORK: the ladder used to stop at 60s, so a cause that never resolves —
/// a tunnel the ChatGPT account cannot see — produced one reconcile a minute
/// forever, and each one opens and closes a dedicated chatgpt.com tab. 04/09
/// spent an afternoon at 60 tabs an hour.
pub const FAILURE_BACKOFF: [Duration; 8] = [
    Duration::from_secs(2),
    Duration::from_secs(5),
    Duration::from_secs(10),
    Duration::from_secs(60),
    Duration::from_secs(120),
    Duration::from_secs(300),
    Duration::from_secs(900),
    Duration::from_secs(1800),
];
pub const FAILURE_BACKOFF_CAP: Duration = Duration::from_secs(1800);
/// FORK: a failure the user has to fix gets three widely spaced attempts and
/// then stops; retrying it faster only churns tabs.
pub const TERMINAL_BACKOFF: [Duration; 3] = [
    Duration::from_secs(60),
    Duration::from_secs(300),
    Duration::from_secs(1800),
];
/// FORK: after this many identical terminal failures the watcher parks.
pub const PARK_AFTER_IDENTICAL_TERMINAL_FAILURES: usize = 3;
/// FORK: a turn asking for a refresh this soon after the last attempt is told
/// the current status instead of starting another reconcile.
pub const TURN_RECONCILE_DEBOUNCE: Duration = Duration::from_secs(10);
/// How long to wait before retrying when the chrome-mcp daemon is unreachable.
pub const BROWSER_UNAVAILABLE_RETRY: Duration = Duration::from_secs(60);
/// Retry delay while the tunnel is still coming up.
const TUNNEL_NOT_READY_RETRY: Duration = Duration::from_secs(2);
/// Name suffix the M0 spikes used; leftovers are swept.
const SPIKE_NAME_SUFFIX: &str = " Spike";

// ---------------------------------------------------------------------------
// Types shared by the planner, the executor and the API.

/// The connector we want to exist on the ChatGPT side.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DesiredConnector {
    /// Base name (`connector_name`); the display name carries the contract
    /// version from v2 on (`Codex Native 2`).
    pub name: String,
    pub description: String,
    pub endpoint: TunnelEndpoint,
    pub contract_version: u32,
    /// Action names the connector must expose (the fixed contract).
    pub expected_actions: Vec<String>,
}

impl DesiredConnector {
    /// The desired connector for the daemon's current endpoint.
    pub fn for_endpoint(name: &str, description: &str, endpoint: TunnelEndpoint) -> Self {
        Self {
            name: name.to_string(),
            description: description.to_string(),
            endpoint,
            contract_version: contract::CONTRACT_VERSION,
            expected_actions: contract::TOOL_NAMES
                .iter()
                .map(|name| (*name).to_string())
                .collect(),
        }
    }

    /// Name as ChatGPT shows it: the base name, suffixed with the contract
    /// version from v2 on so a cached contract never shadows a new one.
    pub fn display_name(&self) -> String {
        display_name_for(&self.name, self.contract_version)
    }

    /// `tunnel:<id>` or the public URL — what `connector.json` records.
    pub fn endpoint_key(&self) -> String {
        endpoint_key(&self.endpoint)
    }
}

pub fn display_name_for(base: &str, contract_version: u32) -> String {
    if contract_version <= 1 {
        base.to_string()
    } else {
        format!("{base} {contract_version}")
    }
}

pub fn endpoint_key(endpoint: &TunnelEndpoint) -> String {
    match endpoint {
        TunnelEndpoint::OpenAi { tunnel_id } => format!("tunnel:{tunnel_id}"),
        TunnelEndpoint::Public { mcp_url } => mcp_url.clone(),
    }
}

/// A connector as `connectors/list_accessible` reports it.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ObservedConnector {
    pub id: String,
    pub name: String,
    /// `base_url` in the API: the full MCP URL, secret path included.
    pub mcp_url: Option<String>,
    pub tunnel_id: Option<String>,
    pub actions: Vec<String>,
}

impl ObservedConnector {
    fn points_at(&self, endpoint: &TunnelEndpoint) -> bool {
        match endpoint {
            TunnelEndpoint::OpenAi { tunnel_id } => self.tunnel_id.as_deref() == Some(tunnel_id),
            TunnelEndpoint::Public { mcp_url } => self.mcp_url.as_deref() == Some(mcp_url),
        }
    }
}

/// A link as `links/list_accessible` reports it.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ObservedLink {
    pub link_id: String,
    pub connector_id: String,
    pub name: String,
}

/// Everything the planner looks at.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Observed {
    /// `None` = not read (the setting is absent until first toggled, which the
    /// API reports as `false`).
    pub developer_mode: Option<bool>,
    pub connectors: Vec<ObservedConnector>,
    pub links: Vec<ObservedLink>,
    /// `connector.json`, when present.
    pub persisted: Option<ConnectorRecord>,
    /// Tunnel ids the account can see (`mcp/tunnels`); only read for the
    /// `openai` transport.
    pub known_tunnels: Option<Vec<String>>,
}

/// A connector id that may only be known after `Create` ran.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConnectorRef {
    Created,
    Id(String),
}

/// A link id that may only be known after `Link` ran.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LinkRef {
    Created,
    Id(String),
}

/// One step of a reconcile. `List*`/`ReadDeveloperMode` are what `observe`
/// issues; the planner emits the rest.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RegistryOp {
    ReadDeveloperMode,
    EnableDeveloperMode,
    ListConnectors,
    ListLinks,
    ListTunnels,
    DeleteLink(String),
    DeleteConnector(String),
    Create(DesiredConnector),
    Link {
        connector: ConnectorRef,
        name: String,
    },
    RefreshActions {
        link: LinkRef,
    },
    /// `GET /connectors/<id>/actions` must list exactly `expect`.
    VerifyActions {
        connector: ConnectorRef,
        expect: Vec<String>,
    },
    /// Write `connector.json` for these ids.
    Persist {
        connector: ConnectorRef,
        link: LinkRef,
    },
}

/// What the API returns for an op.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ApiResult {
    Unit,
    DeveloperMode(bool),
    Connectors(Vec<ObservedConnector>),
    Links(Vec<ObservedLink>),
    Tunnels(Vec<String>),
    Created {
        connector_id: String,
        actions: Vec<String>,
    },
    Linked {
        link_id: String,
        actions: Vec<String>,
    },
    Actions(Vec<String>),
}

/// Why an API op failed.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ApiError {
    pub status: Option<u16>,
    pub message: String,
    /// 403 "Developer mode is required".
    pub developer_mode_required: bool,
    /// 429 that survived the backoff.
    pub rate_limited: bool,
    /// chrome-mcp daemon / extension / tab unreachable.
    pub browser_unavailable: bool,
    /// The page has no ChatGPT login.
    pub login_required: bool,
}

impl ApiError {
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            ..Self::default()
        }
    }

    pub fn browser_unavailable(message: impl Into<String>) -> Self {
        Self {
            browser_unavailable: true,
            ..Self::new(message)
        }
    }
}

impl std::fmt::Display for ApiError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.status {
            Some(status) => write!(f, "HTTP {status}: {}", self.message),
            None => f.write_str(&self.message),
        }
    }
}

/// The ChatGPT connector API, one op at a time. `open`/`close` bracket a
/// reconcile so a browser-backed implementation can hold one tab across ops.
pub trait ConnectorApi: Send + Sync {
    fn open(&self) -> BoxFuture<'_, Result<(), ApiError>> {
        Box::pin(async { Ok(()) })
    }

    fn call<'a>(&'a self, op: &'a RegistryOp) -> BoxFuture<'a, Result<ApiResult, ApiError>>;

    fn close(&self) -> BoxFuture<'_, ()> {
        Box::pin(async {})
    }
}

// ---------------------------------------------------------------------------
// Planner (pure).

/// Whether `observed_name` belongs to an older contract of `base`, or to a
/// spike leftover, and should be swept.
pub fn is_stale_name(observed_name: &str, base: &str, contract_version: u32) -> bool {
    if observed_name.starts_with(base) && observed_name[base.len()..].starts_with(SPIKE_NAME_SUFFIX)
    {
        return true;
    }
    if observed_name == base {
        return contract_version > 1;
    }
    let Some(suffix) = observed_name
        .strip_prefix(base)
        .and_then(|rest| rest.strip_prefix(' '))
    else {
        return false;
    };
    suffix
        .parse::<u32>()
        .is_ok_and(|version| version < contract_version)
}

fn delete_ops_for(observed: &Observed, connector_id: &str, ops: &mut Vec<RegistryOp>) {
    for link in observed
        .links
        .iter()
        .filter(|link| link.connector_id == connector_id)
    {
        ops.push(RegistryOp::DeleteLink(link.link_id.clone()));
    }
    ops.push(RegistryOp::DeleteConnector(connector_id.to_string()));
}

/// FORK: why the planner refuses, with the kind that decides whether a turn
/// should wait for it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlanRefusal {
    pub kind: FailureKind,
    pub reason: String,
}

impl std::fmt::Display for PlanRefusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.reason)
    }
}

/// Decides what to do. `Err` is a refusal with an actionable reason (the
/// configured tunnel is not visible to this account).
pub fn plan(
    observed: &Observed,
    desired: &DesiredConnector,
) -> Result<Vec<RegistryOp>, PlanRefusal> {
    // (4) Developer Mode gates every connector endpoint; switch it on first
    // and observe again.
    if observed.developer_mode == Some(false) {
        return Ok(vec![RegistryOp::EnableDeveloperMode]);
    }

    if let TunnelEndpoint::OpenAi { tunnel_id } = &desired.endpoint
        && let Some(known) = &observed.known_tunnels
        && !known.iter().any(|known| known == tunnel_id)
    {
        return Err(PlanRefusal {
            kind: FailureKind::TunnelNotVisible,
            reason: format!(
                "tunnel `{tunnel_id}` is not visible to the ChatGPT account logged in Chrome ({} tunnel(s) listed). On platform.openai.com > Settings > Organization > Tunnels, edit the tunnel and add this ChatGPT account under \"ChatGPT workspaces\" (for a personal account, search its account id — the `account_id` from chatgpt.com/backend-api/accounts/check); then run `codex chatgpt-web registry reconcile` (or `codex chatgpt-web setup --tunnel-id {tunnel_id} --api-key-file <path>` if the key changed)",
                known.len()
            ),
        });
    }

    let mut ops = Vec::new();
    let name = desired.display_name();

    // (1) Older contracts and spike leftovers go, links first.
    for connector in observed
        .connectors
        .iter()
        .filter(|connector| is_stale_name(&connector.name, &desired.name, desired.contract_version))
    {
        delete_ops_for(observed, &connector.id, &mut ops);
    }

    let same_name: Vec<&ObservedConnector> = observed
        .connectors
        .iter()
        .filter(|connector| connector.name == name)
        .collect();

    // (2) The recorded connector still exists and points at this endpoint:
    // only verify its actions.
    let keep = observed
        .persisted
        .as_ref()
        .filter(|record| record.mcp_url == desired.endpoint_key() && record.name == name)
        .and_then(|record| {
            let connector = same_name.iter().find(|connector| {
                connector.id == record.connector_id && connector.points_at(&desired.endpoint)
            })?;
            let link = observed
                .links
                .iter()
                .find(|link| link.link_id == record.link_id && link.connector_id == connector.id)?;
            Some((connector.id.clone(), link.link_id.clone()))
        })
        // No record (lost `connector.json`), but a same-name connector already
        // points here and has a link: adopt it instead of churning.
        .or_else(|| {
            let connector = same_name
                .iter()
                .find(|connector| connector.points_at(&desired.endpoint))?;
            let link = observed
                .links
                .iter()
                .find(|link| link.connector_id == connector.id)?;
            Some((connector.id.clone(), link.link_id.clone()))
        });

    if let Some((connector_id, link_id)) = keep {
        for duplicate in same_name
            .iter()
            .filter(|connector| connector.id != connector_id)
        {
            delete_ops_for(observed, &duplicate.id, &mut ops);
        }
        ops.push(RegistryOp::VerifyActions {
            connector: ConnectorRef::Id(connector_id.clone()),
            expect: desired.expected_actions.clone(),
        });
        ops.push(RegistryOp::Persist {
            connector: ConnectorRef::Id(connector_id),
            link: LinkRef::Id(link_id),
        });
        return Ok(ops);
    }

    // (3) Anything else with our name is stale (old URL, missing link):
    // delete, then create → link → verify → persist. The URL of a connector
    // cannot be edited, so a changed endpoint always takes this path.
    for connector in &same_name {
        delete_ops_for(observed, &connector.id, &mut ops);
    }
    ops.push(RegistryOp::Create(desired.clone()));
    ops.push(RegistryOp::Link {
        connector: ConnectorRef::Created,
        name,
    });
    ops.push(RegistryOp::VerifyActions {
        connector: ConnectorRef::Created,
        expect: desired.expected_actions.clone(),
    });
    ops.push(RegistryOp::Persist {
        connector: ConnectorRef::Created,
        link: LinkRef::Created,
    });
    Ok(ops)
}

// ---------------------------------------------------------------------------
// Executor.

/// How an execution ended short of a record.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExecError {
    Api(ApiError),
    /// The connector does not expose the contract (ChatGPT could not reach
    /// the endpoint, or cached an older contract).
    VerifyMismatch {
        connector_id: String,
        expected: Vec<String>,
        got: Vec<String>,
    },
    /// Planner/executor disagreement (a `Created` ref before `Create`).
    Unresolved(String),
}

impl std::fmt::Display for ExecError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Api(error) => write!(f, "{error}"),
            Self::VerifyMismatch {
                connector_id,
                expected,
                got,
            } => write!(
                f,
                "connector {connector_id} exposes {got:?}, expected {expected:?}"
            ),
            Self::Unresolved(what) => write!(f, "internal: {what}"),
        }
    }
}

/// What executing a plan produced.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExecOutcome {
    Record(ConnectorRecord),
    /// Developer Mode was switched on; observe and plan again.
    Reobserve,
}

#[derive(Default)]
struct ExecContext {
    created_connector: Option<String>,
    created_link: Option<String>,
    /// Actions reported by the last create/link/refresh/verify.
    actions: Vec<String>,
}

impl ExecContext {
    fn connector(&self, reference: &ConnectorRef) -> Result<String, ExecError> {
        match reference {
            ConnectorRef::Id(id) => Ok(id.clone()),
            ConnectorRef::Created => self
                .created_connector
                .clone()
                .ok_or_else(|| ExecError::Unresolved("connector id before Create".into())),
        }
    }

    fn link(&self, reference: &LinkRef) -> Result<String, ExecError> {
        match reference {
            LinkRef::Id(id) => Ok(id.clone()),
            LinkRef::Created => self
                .created_link
                .clone()
                .ok_or_else(|| ExecError::Unresolved("link id before Link".into())),
        }
    }
}

fn same_action_set(expected: &[String], got: &[String]) -> bool {
    let mut expected: Vec<&str> = expected.iter().map(String::as_str).collect();
    let mut got: Vec<&str> = got.iter().map(String::as_str).collect();
    expected.sort_unstable();
    expected.dedup();
    got.sort_unstable();
    got.dedup();
    expected == got
}

/// Runs `ops` in order. Ids known only after `Create`/`Link` are resolved
/// from the context; `VerifyActions` reads the live action list (asking for a
/// refresh once when the create response lacked them) and fails on mismatch.
pub async fn execute(
    api: &dyn ConnectorApi,
    desired: &DesiredConnector,
    ops: &[RegistryOp],
) -> Result<ExecOutcome, ExecError> {
    let mut ctx = ExecContext::default();
    let mut record: Option<ConnectorRecord> = None;
    for op in ops {
        match op {
            RegistryOp::EnableDeveloperMode => {
                api.call(op).await.map_err(ExecError::Api)?;
                return Ok(ExecOutcome::Reobserve);
            }
            RegistryOp::Create(_) => match api.call(op).await.map_err(ExecError::Api)? {
                ApiResult::Created {
                    connector_id,
                    actions,
                } => {
                    ctx.created_connector = Some(connector_id);
                    ctx.actions = actions;
                }
                other => {
                    return Err(ExecError::Unresolved(format!("Create answered {other:?}")));
                }
            },
            RegistryOp::Link { connector, name } => {
                let connector_id = ctx.connector(connector)?;
                let resolved = RegistryOp::Link {
                    connector: ConnectorRef::Id(connector_id),
                    name: name.clone(),
                };
                match api.call(&resolved).await.map_err(ExecError::Api)? {
                    ApiResult::Linked { link_id, actions } => {
                        ctx.created_link = Some(link_id);
                        if !actions.is_empty() {
                            ctx.actions = actions;
                        }
                    }
                    other => {
                        return Err(ExecError::Unresolved(format!("Link answered {other:?}")));
                    }
                }
            }
            RegistryOp::RefreshActions { link } => {
                let link_id = ctx.link(link)?;
                let resolved = RegistryOp::RefreshActions {
                    link: LinkRef::Id(link_id),
                };
                if let ApiResult::Actions(actions) =
                    api.call(&resolved).await.map_err(ExecError::Api)?
                {
                    ctx.actions = actions;
                }
            }
            RegistryOp::VerifyActions { connector, expect } => {
                let connector_id = ctx.connector(connector)?;
                let resolved = RegistryOp::VerifyActions {
                    connector: ConnectorRef::Id(connector_id.clone()),
                    expect: expect.clone(),
                };
                let mut got = match api.call(&resolved).await.map_err(ExecError::Api)? {
                    ApiResult::Actions(actions) => actions,
                    _ => Vec::new(),
                };
                // A freshly created connector whose schema fetch lagged: ask
                // ChatGPT to re-pull the contract once before giving up.
                if !same_action_set(expect, &got)
                    && let Some(link_id) = ctx.created_link.clone()
                {
                    let refresh = RegistryOp::RefreshActions {
                        link: LinkRef::Id(link_id),
                    };
                    if let ApiResult::Actions(actions) =
                        api.call(&refresh).await.map_err(ExecError::Api)?
                    {
                        got = actions;
                    }
                }
                if !same_action_set(expect, &got) {
                    return Err(ExecError::VerifyMismatch {
                        connector_id,
                        expected: expect.clone(),
                        got,
                    });
                }
                ctx.actions = got;
            }
            RegistryOp::Persist { connector, link } => {
                record = Some(ConnectorRecord {
                    connector_id: ctx.connector(connector)?,
                    link_id: ctx.link(link)?,
                    mcp_url: desired.endpoint_key(),
                    name: desired.display_name(),
                    contract_version: desired.contract_version,
                    verified_at_ms: now_ms(),
                    actions: ctx.actions.clone(),
                });
            }
            RegistryOp::DeleteLink(_)
            | RegistryOp::DeleteConnector(_)
            | RegistryOp::ReadDeveloperMode
            | RegistryOp::ListConnectors
            | RegistryOp::ListLinks
            | RegistryOp::ListTunnels => {
                api.call(op).await.map_err(ExecError::Api)?;
            }
        }
    }
    record
        .map(ExecOutcome::Record)
        .ok_or_else(|| ExecError::Unresolved("the plan ended without Persist".into()))
}

/// Reads the account's current state. A Developer-Mode 403 on the lists is
/// folded into `developer_mode = Some(false)` so the planner switches it on.
pub async fn observe(
    api: &dyn ConnectorApi,
    desired: &DesiredConnector,
    persisted: Option<ConnectorRecord>,
) -> Result<Observed, ApiError> {
    let mut observed = Observed {
        persisted,
        ..Observed::default()
    };
    // A failure here is not fatal on its own: the lists below tell us for sure.
    if let Ok(ApiResult::DeveloperMode(enabled)) = api.call(&RegistryOp::ReadDeveloperMode).await {
        observed.developer_mode = Some(enabled);
    }
    if observed.developer_mode == Some(false) {
        return Ok(observed);
    }
    match api.call(&RegistryOp::ListConnectors).await {
        Ok(ApiResult::Connectors(connectors)) => observed.connectors = connectors,
        Ok(_) => {}
        Err(error) if error.developer_mode_required => {
            observed.developer_mode = Some(false);
            return Ok(observed);
        }
        Err(error) => return Err(error),
    }
    match api.call(&RegistryOp::ListLinks).await {
        Ok(ApiResult::Links(links)) => observed.links = links,
        Ok(_) => {}
        Err(error) if error.developer_mode_required => {
            observed.developer_mode = Some(false);
            return Ok(observed);
        }
        Err(error) => return Err(error),
    }
    if matches!(desired.endpoint, TunnelEndpoint::OpenAi { .. }) {
        match api.call(&RegistryOp::ListTunnels).await {
            Ok(ApiResult::Tunnels(tunnels)) => observed.known_tunnels = Some(tunnels),
            Ok(_) => {}
            Err(error) if error.developer_mode_required => {
                observed.developer_mode = Some(false);
                return Ok(observed);
            }
            // The tunnel list is a pre-flight check, not a requirement.
            Err(error) => {
                tracing::warn!("chatgpt_web registry: could not list tunnels: {error}");
            }
        }
    }
    Ok(observed)
}

/// How a reconcile failed, already mapped onto a status.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RegistryFailure {
    pub status: RegistryStatus,
    pub message: String,
}

/// One full reconcile: observe → plan → execute, re-observing after the
/// Developer Mode toggle and once after a verify mismatch (recreate). Writes
/// `connector.json` on success.
pub async fn reconcile(
    api: &dyn ConnectorApi,
    desired: &DesiredConnector,
    connector_path: &Path,
) -> Result<ConnectorRecord, RegistryFailure> {
    api.open().await.map_err(map_api)?;
    let result = reconcile_open(api, desired, connector_path).await;
    api.close().await;
    result
}

fn fail(reason: String, retry_after: Duration) -> RegistryFailure {
    fail_kind(reason, retry_after, FailureKind::Transient)
}

/// FORK: the same, with the kind that decides whether a turn waits for it.
fn fail_kind(reason: String, retry_after: Duration, kind: FailureKind) -> RegistryFailure {
    RegistryFailure {
        message: reason.clone(),
        status: RegistryStatus::Failed {
            reason,
            retry_at_ms: now_ms() + retry_after.as_millis() as u64,
            kind,
            parked: false,
        },
    }
}

fn map_api(error: ApiError) -> RegistryFailure {
    if error.browser_unavailable {
        RegistryFailure {
            message: error.to_string(),
            status: RegistryStatus::BrowserUnavailable,
        }
    } else if error.developer_mode_required {
        RegistryFailure {
            message: error.to_string(),
            status: RegistryStatus::DeveloperModeOff,
        }
    } else if error.login_required {
        // FORK: no login, no connector API; polling cannot log anyone in.
        fail_kind(
            error.to_string(),
            TERMINAL_BACKOFF[0],
            FailureKind::LoginRequired,
        )
    } else if error.rate_limited {
        // FORK: worth retrying, but not at the transient cadence.
        fail_kind(
            error.to_string(),
            FAILURE_BACKOFF[3],
            FailureKind::RateLimited,
        )
    } else {
        fail(error.to_string(), FAILURE_BACKOFF[0])
    }
}

async fn reconcile_open(
    api: &dyn ConnectorApi,
    desired: &DesiredConnector,
    connector_path: &Path,
) -> Result<ConnectorRecord, RegistryFailure> {
    let mut persisted: Option<ConnectorRecord> = super::state::read_json_opt(connector_path);
    let mut enabled_developer_mode = false;
    let mut recreated = false;
    for _round in 0..4 {
        let observed = observe(api, desired, persisted.clone())
            .await
            .map_err(map_api)?;
        let ops = plan(&observed, desired).map_err(|refusal| {
            fail_kind(refusal.reason, TERMINAL_BACKOFF[0], refusal.kind)
        })?;
        if ops.first() == Some(&RegistryOp::EnableDeveloperMode) {
            if enabled_developer_mode {
                return Err(RegistryFailure {
                    message: "Developer Mode is still off after enabling it".to_string(),
                    status: RegistryStatus::DeveloperModeOff,
                });
            }
            enabled_developer_mode = true;
        }
        match execute(api, desired, &ops).await {
            Ok(ExecOutcome::Record(record)) => {
                persist_record(connector_path, &record).map_err(|error| {
                    fail(
                        format!("writing connector.json: {error}"),
                        FAILURE_BACKOFF[0],
                    )
                })?;
                return Ok(record);
            }
            Ok(ExecOutcome::Reobserve) => continue,
            Err(ExecError::Api(error))
                if error.developer_mode_required && !enabled_developer_mode =>
            {
                // A 403 mid-plan: switch Developer Mode on once and start over.
                enabled_developer_mode = true;
                api.call(&RegistryOp::EnableDeveloperMode)
                    .await
                    .map_err(map_api)?;
                continue;
            }
            Err(ExecError::VerifyMismatch {
                connector_id,
                expected,
                got,
            }) if !recreated => {
                // ChatGPT cached another contract under this identity (or
                // could not reach the endpoint): drop it and create afresh.
                recreated = true;
                tracing::warn!(
                    "chatgpt_web registry: connector {connector_id} exposes {got:?} instead of {expected:?}; recreating"
                );
                let links: Vec<String> = observe(api, desired, None)
                    .await
                    .map_err(map_api)?
                    .links
                    .into_iter()
                    .filter(|link| link.connector_id == connector_id)
                    .map(|link| link.link_id)
                    .collect();
                for link_id in links {
                    api.call(&RegistryOp::DeleteLink(link_id))
                        .await
                        .map_err(map_api)?;
                }
                api.call(&RegistryOp::DeleteConnector(connector_id))
                    .await
                    .map_err(map_api)?;
                persisted = None;
                let _ = std::fs::remove_file(connector_path);
                continue;
            }
            Err(error @ ExecError::VerifyMismatch { .. }) => {
                return Err(fail_kind(
                    error.to_string(),
                    TERMINAL_BACKOFF[0],
                    FailureKind::SetupRequired,
                ));
            }
            Err(ExecError::Api(error)) => return Err(map_api(error)),
            Err(error @ ExecError::Unresolved(_)) => {
                return Err(fail_kind(
                    error.to_string(),
                    TERMINAL_BACKOFF[0],
                    FailureKind::SetupRequired,
                ));
            }
        }
    }
    Err(fail_kind(
        "reconcile did not converge after 4 rounds".to_string(),
        TERMINAL_BACKOFF[0],
        FailureKind::SetupRequired,
    ))
}

/// `connector.json` holds the secret MCP path: owner-only where the OS allows.
pub fn persist_record(path: &Path, record: &ConnectorRecord) -> std::io::Result<()> {
    let body = serde_json::to_string_pretty(record).map_err(std::io::Error::other)?;
    super::state::write_secret(path, &body)
}

/// Removes the recorded connector (and any other connector carrying our
/// name) from the ChatGPT side, then forgets `connector.json`.
pub async fn delete_recorded(
    api: &dyn ConnectorApi,
    base_name: &str,
    connector_path: &Path,
) -> Result<Vec<String>, ApiError> {
    api.open().await?;
    let result = delete_recorded_open(api, base_name, connector_path).await;
    api.close().await;
    result
}

async fn delete_recorded_open(
    api: &dyn ConnectorApi,
    base_name: &str,
    connector_path: &Path,
) -> Result<Vec<String>, ApiError> {
    let persisted: Option<ConnectorRecord> = super::state::read_json_opt(connector_path);
    let connectors = match api.call(&RegistryOp::ListConnectors).await? {
        ApiResult::Connectors(connectors) => connectors,
        _ => Vec::new(),
    };
    let links = match api.call(&RegistryOp::ListLinks).await? {
        ApiResult::Links(links) => links,
        _ => Vec::new(),
    };
    let ours = |connector: &ObservedConnector| {
        persisted
            .as_ref()
            .is_some_and(|record| record.connector_id == connector.id)
            || connector.name == base_name
            || is_stale_name(&connector.name, base_name, u32::MAX)
    };
    let mut deleted = Vec::new();
    for connector in connectors.iter().filter(|connector| ours(connector)) {
        for link in links
            .iter()
            .filter(|link| link.connector_id == connector.id)
        {
            api.call(&RegistryOp::DeleteLink(link.link_id.clone()))
                .await?;
        }
        api.call(&RegistryOp::DeleteConnector(connector.id.clone()))
            .await?;
        deleted.push(format!("{} ({})", connector.name, connector.id));
    }
    let _ = std::fs::remove_file(connector_path);
    Ok(deleted)
}

// ---------------------------------------------------------------------------
// Service: the daemon-side wrapper with status, backoff and the tunnel watch.

/// Owns the reconcile inside the daemon: serializes runs, applies the failure
/// backoff, keeps `ControlState.registry` current and re-runs whenever the
/// tunnel endpoint changes.
pub struct RegistryService {
    api: Arc<dyn ConnectorApi>,
    connector_name: String,
    connector_description: String,
    connector_path: PathBuf,
    tunnel: watch::Receiver<TunnelState>,
    status: Arc<Mutex<RegistryStatus>>,
    gate: Semaphore,
    failures: AtomicUsize,
    /// FORK: the last failure message, so an identical repeat logs at `debug`.
    last_failure: Mutex<Option<String>>,
    /// FORK: how many times in a row the same terminal failure has come back.
    identical_terminal_failures: AtomicUsize,
    /// FORK: when the last attempt finished, for the turn debounce.
    last_attempt_ms: AtomicU64,
}

impl std::fmt::Debug for RegistryService {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RegistryService")
            .field("connector_name", &self.connector_name)
            .field("connector_path", &self.connector_path)
            .finish_non_exhaustive()
    }
}

impl RegistryService {
    pub fn new(
        api: Arc<dyn ConnectorApi>,
        connector_name: &str,
        connector_description: &str,
        connector_path: PathBuf,
        tunnel: watch::Receiver<TunnelState>,
        status: Arc<Mutex<RegistryStatus>>,
    ) -> Arc<Self> {
        Arc::new(Self {
            api,
            connector_name: connector_name.to_string(),
            connector_description: connector_description.to_string(),
            connector_path,
            tunnel,
            status,
            gate: Semaphore::new(1),
            failures: AtomicUsize::new(0),
            last_failure: Mutex::new(None),
            identical_terminal_failures: AtomicUsize::new(0),
            last_attempt_ms: AtomicU64::new(0),
        })
    }

    pub fn status(&self) -> RegistryStatus {
        self.status
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    /// FORK: every status change is one `info!` line, so `daemon.log` shows the
    /// registry's whole trajectory rather than only its failures.
    fn set_status(&self, status: RegistryStatus) {
        let mut current = self
            .status
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if *current != status {
            tracing::info!(
                "chatgpt_web registry: {} -> {}",
                current.label(),
                status.label()
            );
        }
        *current = status;
    }

    /// The connector for the tunnel's current endpoint, if it is ready.
    pub fn desired(&self) -> Result<DesiredConnector, String> {
        match &*self.tunnel.borrow() {
            TunnelState::Ready { endpoint } => Ok(DesiredConnector::for_endpoint(
                &self.connector_name,
                &self.connector_description,
                endpoint.clone(),
            )),
            other => Err(format!("tunnel not ready ({})", other.label())),
        }
    }

    /// Whether the persisted record already covers the tunnel's endpoint.
    pub fn record_matches_endpoint(&self) -> bool {
        let Ok(desired) = self.desired() else {
            return false;
        };
        super::state::read_json_opt::<ConnectorRecord>(&self.connector_path).is_some_and(|record| {
            record.mcp_url == desired.endpoint_key()
                && record.name == desired.display_name()
                && record.contract_version == desired.contract_version
        })
    }

    /// Runs a reconcile now unless one is in progress (then waits for it) or
    /// the caller is not entitled to one yet (then reports the current status).
    ///
    /// FORK: `trigger` decides what "entitled" means. The watcher respects the
    /// backoff and the parked flag; a `Turn` may override a park once (the user
    /// has probably just fixed whatever it was), subject to a short debounce;
    /// `Manual` and `TunnelChange` start the ladder over.
    pub async fn reconcile_now(&self, trigger: ReconcileTrigger) -> RegistryStatus {
        let _permit = match self.gate.acquire().await {
            Ok(permit) => permit,
            Err(_) => return self.status(),
        };
        if matches!(
            trigger,
            ReconcileTrigger::Manual | ReconcileTrigger::TunnelChange
        ) {
            self.reset_backoff();
        }
        if !self.may_reconcile(trigger) {
            return self.status();
        }
        let desired = match self.desired() {
            Ok(desired) => desired,
            Err(reason) => {
                let status = RegistryStatus::Failed {
                    reason,
                    retry_at_ms: now_ms() + TUNNEL_NOT_READY_RETRY.as_millis() as u64,
                    kind: FailureKind::Transient,
                    parked: false,
                };
                self.set_status(status.clone());
                return status;
            }
        };
        self.last_attempt_ms.store(now_ms(), Ordering::SeqCst);
        self.set_status(RegistryStatus::Reconciling);
        let status = match reconcile(self.api.as_ref(), &desired, &self.connector_path).await {
            Ok(record) => {
                self.reset_backoff();
                tracing::info!(
                    "chatgpt_web registry: connector `{}` verified ({} actions)",
                    record.name,
                    record.actions.len()
                );
                RegistryStatus::Verified {
                    connector_id: record.connector_id,
                    link_id: record.link_id,
                    mcp_url: record.mcp_url,
                }
            }
            Err(failure) => {
                let attempt = self.failures.fetch_add(1, Ordering::SeqCst);
                // FORK: a stuck cause repeats every backoff tick; say it once
                // at `warn` and keep the repeats at `debug` so a real new
                // failure still stands out. (04/09 produced 379 identical
                // lines in one afternoon.)
                let repeated = self
                    .last_failure
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .replace(failure.message.clone())
                    .is_some_and(|previous| previous == failure.message);
                if repeated {
                    tracing::debug!(
                        "chatgpt_web registry: reconcile failed again: {}",
                        failure.message
                    );
                } else {
                    tracing::warn!(
                        "chatgpt_web registry: reconcile failed: {}",
                        failure.message
                    );
                }
                match failure.status {
                    RegistryStatus::Failed { reason, kind, .. } => {
                        // FORK: a terminal failure that keeps coming back is
                        // not going to fix itself; after a few widely spaced
                        // attempts the watcher parks and stops churning tabs.
                        let identical_terminal = if kind.is_terminal() && repeated {
                            self.identical_terminal_failures
                                .fetch_add(1, Ordering::SeqCst)
                                + 1
                        } else {
                            self.identical_terminal_failures.store(
                                usize::from(kind.is_terminal()),
                                Ordering::SeqCst,
                            );
                            usize::from(kind.is_terminal())
                        };
                        let parked =
                            identical_terminal >= PARK_AFTER_IDENTICAL_TERMINAL_FAILURES;
                        if parked {
                            tracing::warn!(
                                "chatgpt_web registry: parking automatic retries after {identical_terminal} identical {} failures; run `codex chatgpt-web registry reconcile` after fixing it",
                                kind.label()
                            );
                        }
                        RegistryStatus::Failed {
                            reason,
                            retry_at_ms: now_ms()
                                + backoff_for(kind, attempt).as_millis() as u64,
                            kind,
                            parked,
                        }
                    }
                    other => other,
                }
            }
        };
        self.set_status(status.clone());
        status
    }

    /// FORK: whether this trigger gets to run a reconcile right now.
    fn may_reconcile(&self, trigger: ReconcileTrigger) -> bool {
        let RegistryStatus::Failed {
            retry_at_ms,
            parked,
            ..
        } = self.status()
        else {
            return true;
        };
        match trigger {
            // The watcher honours both the backoff and the park.
            ReconcileTrigger::Watcher => !parked && now_ms() >= retry_at_ms,
            // A turn is the user in front of us: it overrides the park, but
            // not two turns in the same breath.
            ReconcileTrigger::Turn => {
                now_ms().saturating_sub(self.last_attempt_ms.load(Ordering::SeqCst))
                    >= TURN_RECONCILE_DEBOUNCE.as_millis() as u64
            }
            // Explicitly asked for, or the world changed underneath us.
            ReconcileTrigger::Manual | ReconcileTrigger::TunnelChange => true,
        }
    }

    /// FORK: pretends every wait has elapsed — the current failure's retry
    /// deadline and the turn debounce alike — so a test can drive the ladder
    /// without sleeping through 60s, 5min and 30min. The failure history itself
    /// is untouched: that is what the ladder counts.
    #[cfg(test)]
    pub(crate) fn advance_clock_for_tests(&self) {
        self.last_attempt_ms.store(0, Ordering::SeqCst);
        let mut status = self
            .status
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let RegistryStatus::Failed { retry_at_ms, .. } = &mut *status {
            *retry_at_ms = now_ms();
        }
    }

    /// FORK: forget the failure history so the ladder starts over.
    fn reset_backoff(&self) {
        self.failures.store(0, Ordering::SeqCst);
        self.identical_terminal_failures.store(0, Ordering::SeqCst);
        *self
            .last_failure
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = None;
    }

    /// The hook the control API calls on `POST /v1/registry/reconcile` and
    /// `POST /v1/registry/refresh`.
    pub fn hook(self: &Arc<Self>) -> ReconcileHook {
        let service = Arc::clone(self);
        Arc::new(move |trigger| {
            let service = Arc::clone(&service);
            Box::pin(async move { Ok(service.reconcile_now(trigger).await) })
        })
    }

    /// Reconciles at start and whenever the tunnel reports a `Ready` endpoint
    /// the record does not cover; a failed attempt is retried after its
    /// backoff while the tunnel stays ready.
    pub fn spawn_watcher(self: &Arc<Self>, cancel: CancellationToken) -> JoinHandle<()> {
        let service = Arc::clone(self);
        tokio::spawn(async move {
            let mut tunnel = service.tunnel.clone();
            loop {
                let ready = matches!(&*tunnel.borrow(), TunnelState::Ready { .. });
                if ready && !matches!(service.status(), RegistryStatus::Verified { .. }) {
                    service.reconcile_now(ReconcileTrigger::Watcher).await;
                }
                let wait = match service.status() {
                    // FORK: a parked registry is not on a timer any more; only
                    // a turn, a manual reconcile or a new tunnel wakes it.
                    RegistryStatus::Failed { parked: true, .. } => Duration::from_secs(3600),
                    RegistryStatus::Failed { retry_at_ms, .. } => {
                        Duration::from_millis(retry_at_ms.saturating_sub(now_ms()).max(500))
                    }
                    RegistryStatus::BrowserUnavailable => BROWSER_UNAVAILABLE_RETRY,
                    RegistryStatus::DeveloperModeOff => FAILURE_BACKOFF_CAP,
                    _ => Duration::from_secs(3600),
                };
                tracing::debug!(
                    "chatgpt_web registry: next reconcile in {}s",
                    wait.as_secs()
                );
                tokio::select! {
                    _ = cancel.cancelled() => break,
                    changed = tunnel.changed() => {
                        if changed.is_err() { break; }
                        // A new endpoint invalidates a `Verified` status.
                        if !service.record_matches_endpoint()
                            && matches!(service.status(), RegistryStatus::Verified { .. })
                        {
                            service.set_status(RegistryStatus::Unknown);
                        }
                        // FORK: a new tunnel is exactly the kind of change that
                        // can fix a parked failure; un-park and try again.
                        if matches!(service.status(), RegistryStatus::Failed { parked: true, .. }) {
                            service.reset_backoff();
                            service.set_status(RegistryStatus::Unknown);
                        }
                    }
                    _ = tokio::time::sleep(wait) => {}
                }
            }
        })
    }
}

/// FORK: terminal failures climb a much slower ladder — each attempt costs a
/// dedicated chatgpt.com tab, and no number of them fixes a tunnel the account
/// cannot see.
fn backoff_for(kind: FailureKind, attempt: usize) -> Duration {
    if kind.is_terminal() {
        return TERMINAL_BACKOFF
            .get(attempt)
            .copied()
            .unwrap_or(*TERMINAL_BACKOFF.last().unwrap_or(&FAILURE_BACKOFF_CAP));
    }
    FAILURE_BACKOFF
        .get(attempt)
        .copied()
        .unwrap_or(FAILURE_BACKOFF_CAP)
}

#[cfg(test)]
#[path = "registry_tests.rs"]
mod tests;
