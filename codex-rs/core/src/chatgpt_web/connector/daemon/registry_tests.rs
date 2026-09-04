use super::*;
use crate::chatgpt_web::connector::daemon::registry_api::ChromeMcpPageApi;
use crate::chatgpt_web::driver::DriverError;
use crate::chatgpt_web::driver::DriverResult;
use crate::chatgpt_web::driver::daemon::ToolResult;
use crate::chatgpt_web::driver::tabs::TabDaemon;
use crate::chatgpt_web::driver::tabs::TabId;
use futures::FutureExt;
use pretty_assertions::assert_eq;
use serde_json::Value;
use serde_json::json;
use std::collections::VecDeque;
use std::sync::Mutex as StdMutex;

fn actions() -> Vec<String> {
    contract::TOOL_NAMES
        .iter()
        .map(|name| (*name).to_string())
        .collect()
}

fn public(url: &str) -> TunnelEndpoint {
    TunnelEndpoint::Public {
        mcp_url: url.to_string(),
    }
}

fn desired(url: &str) -> DesiredConnector {
    DesiredConnector::for_endpoint("Codex Native", "Codex tools", public(url))
}

fn connector(id: &str, name: &str, url: &str) -> ObservedConnector {
    ObservedConnector {
        id: id.into(),
        name: name.into(),
        mcp_url: Some(url.into()),
        tunnel_id: None,
        actions: actions(),
    }
}

fn link(link_id: &str, connector_id: &str, name: &str) -> ObservedLink {
    ObservedLink {
        link_id: link_id.into(),
        connector_id: connector_id.into(),
        name: name.into(),
    }
}

fn record(connector_id: &str, link_id: &str, url: &str) -> ConnectorRecord {
    ConnectorRecord {
        connector_id: connector_id.into(),
        link_id: link_id.into(),
        mcp_url: url.into(),
        name: "Codex Native".into(),
        contract_version: contract::CONTRACT_VERSION,
        verified_at_ms: 1,
        actions: actions(),
    }
}

const URL_A: &str = "https://a.trycloudflare.com/mcp/secret-a";
const URL_B: &str = "https://b.trycloudflare.com/mcp/secret-b";

// ---------------------------------------------------------------------------
// Planner.

#[test]
fn a_fresh_account_gets_create_link_verify_persist() {
    let observed = Observed {
        developer_mode: Some(true),
        ..Observed::default()
    };
    let ops = plan(&observed, &desired(URL_A)).expect("plan");
    assert_eq!(
        ops,
        vec![
            RegistryOp::Create(desired(URL_A)),
            RegistryOp::Link {
                connector: ConnectorRef::Created,
                name: "Codex Native".into(),
            },
            RegistryOp::VerifyActions {
                connector: ConnectorRef::Created,
                expect: actions(),
            },
            RegistryOp::Persist {
                connector: ConnectorRef::Created,
                link: LinkRef::Created,
            },
        ]
    );
}

#[test]
fn a_changed_url_deletes_the_old_connector_and_recreates() {
    let observed = Observed {
        developer_mode: Some(true),
        connectors: vec![connector("asdk_app_old", "Codex Native", URL_A)],
        links: vec![link("link_old", "asdk_app_old", "Codex Native")],
        persisted: Some(record("asdk_app_old", "link_old", URL_A)),
        known_tunnels: None,
        account: None,
    };
    let ops = plan(&observed, &desired(URL_B)).expect("plan");
    assert_eq!(
        &ops[..2],
        &[
            RegistryOp::DeleteLink("link_old".into()),
            RegistryOp::DeleteConnector("asdk_app_old".into()),
        ]
    );
    assert!(matches!(ops[2], RegistryOp::Create(_)));
    assert!(matches!(ops.last(), Some(RegistryOp::Persist { .. })));
}

#[test]
fn the_same_endpoint_only_verifies_and_refreshes_the_record() {
    let observed = Observed {
        developer_mode: Some(true),
        connectors: vec![connector("asdk_app_1", "Codex Native", URL_A)],
        links: vec![link("link_1", "asdk_app_1", "Codex Native")],
        persisted: Some(record("asdk_app_1", "link_1", URL_A)),
        known_tunnels: None,
        account: None,
    };
    let ops = plan(&observed, &desired(URL_A)).expect("plan");
    assert_eq!(
        ops,
        vec![
            RegistryOp::VerifyActions {
                connector: ConnectorRef::Id("asdk_app_1".into()),
                expect: actions(),
            },
            RegistryOp::Persist {
                connector: ConnectorRef::Id("asdk_app_1".into()),
                link: LinkRef::Id("link_1".into()),
            },
        ]
    );
}

#[test]
fn a_lost_record_adopts_the_connector_that_already_points_here() {
    let observed = Observed {
        developer_mode: Some(true),
        connectors: vec![
            connector("asdk_app_dup", "Codex Native", URL_B),
            connector("asdk_app_1", "Codex Native", URL_A),
        ],
        links: vec![
            link("link_dup", "asdk_app_dup", "Codex Native"),
            link("link_1", "asdk_app_1", "Codex Native"),
        ],
        persisted: None,
        known_tunnels: None,
        account: None,
    };
    let ops = plan(&observed, &desired(URL_A)).expect("plan");
    assert_eq!(
        ops,
        vec![
            RegistryOp::DeleteLink("link_dup".into()),
            RegistryOp::DeleteConnector("asdk_app_dup".into()),
            RegistryOp::VerifyActions {
                connector: ConnectorRef::Id("asdk_app_1".into()),
                expect: actions(),
            },
            RegistryOp::Persist {
                connector: ConnectorRef::Id("asdk_app_1".into()),
                link: LinkRef::Id("link_1".into()),
            },
        ]
    );
}

#[test]
fn developer_mode_off_is_switched_on_before_anything_else() {
    let observed = Observed {
        developer_mode: Some(false),
        connectors: vec![connector("asdk_app_1", "Codex Native", URL_A)],
        ..Observed::default()
    };
    assert_eq!(
        plan(&observed, &desired(URL_A)).expect("plan"),
        vec![RegistryOp::EnableDeveloperMode]
    );
}

#[test]
fn old_contract_names_and_spike_leftovers_are_swept() {
    let mut wanted = desired(URL_A);
    wanted.contract_version = 2;
    let observed = Observed {
        developer_mode: Some(true),
        connectors: vec![
            connector("asdk_app_v1", "Codex Native", URL_A),
            connector("asdk_app_spike", "Codex Native Spike API", URL_B),
            connector("asdk_app_other", "Something Else", URL_B),
        ],
        links: vec![
            link("link_v1", "asdk_app_v1", "Codex Native"),
            link("link_other", "asdk_app_other", "Something Else"),
        ],
        persisted: None,
        known_tunnels: None,
        account: None,
    };
    let ops = plan(&observed, &wanted).expect("plan");
    assert_eq!(
        &ops[..3],
        &[
            RegistryOp::DeleteLink("link_v1".into()),
            RegistryOp::DeleteConnector("asdk_app_v1".into()),
            RegistryOp::DeleteConnector("asdk_app_spike".into()),
        ]
    );
    assert!(matches!(&ops[3], RegistryOp::Create(d) if d.display_name() == "Codex Native 2"));
    assert!(
        !ops.iter()
            .any(|op| *op == RegistryOp::DeleteConnector("asdk_app_other".into()))
    );
}

#[test]
fn an_unknown_tunnel_id_is_refused_with_a_setup_hint() {
    let wanted = DesiredConnector::for_endpoint(
        "Codex Native",
        "Codex tools",
        TunnelEndpoint::OpenAi {
            tunnel_id: "tunnel_0123456789abcdef0123456789abcdef".into(),
        },
    );
    let observed = Observed {
        developer_mode: Some(true),
        known_tunnels: Some(vec!["tunnel_ffffffffffffffffffffffffffffffff".into()]),
        ..Observed::default()
    };
    let refusal = plan(&observed, &wanted).expect_err("refused");
    assert_eq!(refusal.kind, FailureKind::TunnelNotVisible);
    assert!(refusal.kind.is_terminal());
    let reason = &refusal.reason;
    assert!(reason.contains("codex chatgpt-web setup"), "{reason}");
    assert!(reason.contains("not visible"), "{reason}");

    // Listed → normal create path with `tunnel_id`.
    let observed = Observed {
        developer_mode: Some(true),
        known_tunnels: Some(vec!["tunnel_0123456789abcdef0123456789abcdef".into()]),
        ..Observed::default()
    };
    let ops = plan(&observed, &wanted).expect("plan");
    assert!(matches!(ops[0], RegistryOp::Create(_)));
    assert_eq!(
        wanted.endpoint_key(),
        "tunnel:tunnel_0123456789abcdef0123456789abcdef"
    );
}

#[test]
fn stale_name_detection_covers_versions_and_spikes() {
    assert!(is_stale_name("Codex Native", "Codex Native", 2));
    assert!(!is_stale_name("Codex Native", "Codex Native", 1));
    assert!(is_stale_name("Codex Native 1", "Codex Native", 2));
    assert!(!is_stale_name("Codex Native 2", "Codex Native", 2));
    assert!(!is_stale_name("Codex Native 3", "Codex Native", 2));
    assert!(is_stale_name("Codex Native Spike", "Codex Native", 1));
    assert!(is_stale_name("Codex Native Spike API", "Codex Native", 1));
    assert!(!is_stale_name("Codex Nativeness", "Codex Native", 1));
    assert!(!is_stale_name("Other", "Codex Native", 5));
    assert_eq!(display_name_for("Codex Native", 1), "Codex Native");
    assert_eq!(display_name_for("Codex Native", 3), "Codex Native 3");
}

// ---------------------------------------------------------------------------
// Executor / reconcile with a fake API.

/// A fake ChatGPT account: connectors and links live in memory, ids are
/// minted on create, and errors can be queued per op kind.
#[derive(Default)]
struct FakeApi {
    state: StdMutex<FakeState>,
    log: StdMutex<Vec<RegistryOp>>,
}

#[derive(Default)]
struct FakeState {
    developer_mode: bool,
    connectors: Vec<ObservedConnector>,
    links: Vec<ObservedLink>,
    tunnels: Vec<String>,
    next_id: usize,
    /// Actions the next creates report (`None` = the contract).
    create_actions: VecDeque<Vec<String>>,
    /// Actions `VerifyActions` reports for a connector id (default: the
    /// contract, or what create said).
    verify_actions: Vec<(String, Vec<String>)>,
    /// Errors to return, matched by op kind name, consumed in order.
    failures: VecDeque<(&'static str, ApiError)>,
    open_calls: usize,
    close_calls: usize,
    /// FORK: who Chrome is logged in as, for the tunnel-not-visible message.
    account: AccountInfo,
}

fn op_kind(op: &RegistryOp) -> &'static str {
    match op {
        RegistryOp::ReadDeveloperMode => "ReadDeveloperMode",
        RegistryOp::ReadAccount => "ReadAccount",
        RegistryOp::EnableDeveloperMode => "EnableDeveloperMode",
        RegistryOp::ListConnectors => "ListConnectors",
        RegistryOp::ListLinks => "ListLinks",
        RegistryOp::ListTunnels => "ListTunnels",
        RegistryOp::DeleteLink(_) => "DeleteLink",
        RegistryOp::DeleteConnector(_) => "DeleteConnector",
        RegistryOp::Create(_) => "Create",
        RegistryOp::Link { .. } => "Link",
        RegistryOp::RefreshActions { .. } => "RefreshActions",
        RegistryOp::VerifyActions { .. } => "VerifyActions",
        RegistryOp::Persist { .. } => "Persist",
    }
}

impl FakeApi {
    fn new(developer_mode: bool) -> Arc<Self> {
        let api = Arc::new(Self::default());
        api.state.lock().expect("state").developer_mode = developer_mode;
        api
    }

    fn with(self: Arc<Self>, f: impl FnOnce(&mut FakeState)) -> Arc<Self> {
        f(&mut self.state.lock().expect("state"));
        self
    }

    fn fail(self: Arc<Self>, kind: &'static str, error: ApiError) -> Arc<Self> {
        self.state
            .lock()
            .expect("state")
            .failures
            .push_back((kind, error));
        self
    }

    fn kinds(&self) -> Vec<&'static str> {
        self.log.lock().expect("log").iter().map(op_kind).collect()
    }

    fn count(&self, kind: &str) -> usize {
        self.kinds().iter().filter(|k| **k == kind).count()
    }

    fn connectors(&self) -> Vec<ObservedConnector> {
        self.state.lock().expect("state").connectors.clone()
    }

    fn links(&self) -> Vec<ObservedLink> {
        self.state.lock().expect("state").links.clone()
    }

    fn handle(&self, op: &RegistryOp) -> Result<ApiResult, ApiError> {
        self.log.lock().expect("log").push(op.clone());
        let mut state = self.state.lock().expect("state");
        let kind = op_kind(op);
        if let Some(position) = state.failures.iter().position(|(k, _)| *k == kind) {
            let (_, error) = state.failures.remove(position).expect("failure");
            return Err(error);
        }
        let dev_gate = |state: &FakeState| -> Result<(), ApiError> {
            if state.developer_mode {
                Ok(())
            } else {
                Err(ApiError {
                    status: Some(403),
                    message: "Developer mode is required".into(),
                    developer_mode_required: true,
                    ..ApiError::default()
                })
            }
        };
        match op {
            RegistryOp::ReadDeveloperMode => Ok(ApiResult::DeveloperMode(state.developer_mode)),
            RegistryOp::ReadAccount => Ok(ApiResult::Account(state.account.clone())),
            RegistryOp::EnableDeveloperMode => {
                state.developer_mode = true;
                Ok(ApiResult::DeveloperMode(true))
            }
            RegistryOp::ListConnectors => {
                dev_gate(&state)?;
                Ok(ApiResult::Connectors(state.connectors.clone()))
            }
            RegistryOp::ListLinks => {
                dev_gate(&state)?;
                Ok(ApiResult::Links(state.links.clone()))
            }
            RegistryOp::ListTunnels => {
                dev_gate(&state)?;
                Ok(ApiResult::Tunnels(state.tunnels.clone()))
            }
            RegistryOp::DeleteLink(id) => {
                state.links.retain(|link| link.link_id != *id);
                Ok(ApiResult::Unit)
            }
            RegistryOp::DeleteConnector(id) => {
                state.connectors.retain(|connector| connector.id != *id);
                Ok(ApiResult::Unit)
            }
            RegistryOp::Create(desired) => {
                dev_gate(&state)?;
                state.next_id += 1;
                let id = format!("asdk_app_{:032x}", state.next_id);
                let actions = state.create_actions.pop_front().unwrap_or_else(actions);
                state.connectors.push(ObservedConnector {
                    id: id.clone(),
                    name: desired.display_name(),
                    mcp_url: match &desired.endpoint {
                        TunnelEndpoint::Public { mcp_url } => Some(mcp_url.clone()),
                        TunnelEndpoint::OpenAi { .. } => None,
                    },
                    tunnel_id: match &desired.endpoint {
                        TunnelEndpoint::OpenAi { tunnel_id } => Some(tunnel_id.clone()),
                        TunnelEndpoint::Public { .. } => None,
                    },
                    actions: actions.clone(),
                });
                Ok(ApiResult::Created {
                    connector_id: id,
                    actions,
                })
            }
            RegistryOp::Link { connector, name } => {
                let ConnectorRef::Id(connector_id) = connector else {
                    panic!("Link reached the API unresolved");
                };
                state.next_id += 1;
                let link_id = format!("link_{:032x}", state.next_id);
                state.links.push(ObservedLink {
                    link_id: link_id.clone(),
                    connector_id: connector_id.clone(),
                    name: name.clone(),
                });
                let actions = state
                    .connectors
                    .iter()
                    .find(|c| c.id == *connector_id)
                    .map(|c| c.actions.clone())
                    .unwrap_or_default();
                Ok(ApiResult::Linked { link_id, actions })
            }
            RegistryOp::RefreshActions { link } => {
                let LinkRef::Id(link_id) = link else {
                    panic!("RefreshActions reached the API unresolved");
                };
                let connector_id = state
                    .links
                    .iter()
                    .find(|l| l.link_id == *link_id)
                    .map(|l| l.connector_id.clone())
                    .unwrap_or_default();
                let refreshed = state
                    .verify_actions
                    .iter()
                    .find(|(id, _)| *id == connector_id)
                    .map(|(_, a)| a.clone())
                    .unwrap_or_else(actions);
                if let Some(connector) = state.connectors.iter_mut().find(|c| c.id == connector_id)
                {
                    connector.actions = refreshed.clone();
                }
                Ok(ApiResult::Actions(refreshed))
            }
            RegistryOp::VerifyActions { connector, .. } => {
                let ConnectorRef::Id(connector_id) = connector else {
                    panic!("VerifyActions reached the API unresolved");
                };
                let Some(found) = state.connectors.iter().find(|c| c.id == *connector_id) else {
                    return Err(ApiError {
                        status: Some(404),
                        message: "Connector not found".into(),
                        ..ApiError::default()
                    });
                };
                let actions = state
                    .verify_actions
                    .iter()
                    .find(|(id, _)| id == connector_id)
                    .map(|(_, a)| a.clone())
                    .unwrap_or_else(|| found.actions.clone());
                Ok(ApiResult::Actions(actions))
            }
            RegistryOp::Persist { .. } => Ok(ApiResult::Unit),
        }
    }
}

impl ConnectorApi for FakeApi {
    fn open(&self) -> BoxFuture<'_, Result<(), ApiError>> {
        self.state.lock().expect("state").open_calls += 1;
        async { Ok(()) }.boxed()
    }

    fn call<'a>(&'a self, op: &'a RegistryOp) -> BoxFuture<'a, Result<ApiResult, ApiError>> {
        let result = self.handle(op);
        async move { result }.boxed()
    }

    fn close(&self) -> BoxFuture<'_, ()> {
        self.state.lock().expect("state").close_calls += 1;
        async {}.boxed()
    }
}

fn temp_connector_path() -> (tempfile::TempDir, PathBuf) {
    let temp = tempfile::tempdir().expect("tempdir");
    let path = temp.path().join("connector.json");
    (temp, path)
}

#[tokio::test]
async fn a_fresh_reconcile_creates_links_verifies_and_writes_connector_json() {
    let api = FakeApi::new(true);
    let (_temp, path) = temp_connector_path();

    let record = reconcile(api.as_ref(), &desired(URL_A), &path)
        .await
        .expect("record");

    assert!(record.connector_id.starts_with("asdk_app_"));
    assert!(record.link_id.starts_with("link_"));
    assert_eq!(record.mcp_url, URL_A);
    assert_eq!(record.name, "Codex Native");
    assert_eq!(record.actions, actions());
    let on_disk: ConnectorRecord =
        super::super::state::read_json_opt(&path).expect("connector.json");
    assert_eq!(on_disk, record);
    assert_eq!(
        api.kinds(),
        vec![
            "ReadDeveloperMode",
            "ListConnectors",
            "ListLinks",
            "Create",
            "Link",
            "VerifyActions",
        ]
    );
    let state = api.state.lock().expect("state");
    assert_eq!((state.open_calls, state.close_calls), (1, 1));
}

#[tokio::test]
async fn the_second_reconcile_only_verifies() {
    let api = FakeApi::new(true);
    let (_temp, path) = temp_connector_path();
    let first = reconcile(api.as_ref(), &desired(URL_A), &path)
        .await
        .expect("record");
    api.log.lock().expect("log").clear();

    let second = reconcile(api.as_ref(), &desired(URL_A), &path)
        .await
        .expect("record");

    assert_eq!(second.connector_id, first.connector_id);
    assert_eq!(second.link_id, first.link_id);
    assert_eq!(
        api.kinds(),
        vec![
            "ReadDeveloperMode",
            "ListConnectors",
            "ListLinks",
            "VerifyActions"
        ]
    );
    assert_eq!(api.connectors().len(), 1);
}

#[tokio::test]
async fn a_new_url_replaces_the_connector() {
    let api = FakeApi::new(true);
    let (_temp, path) = temp_connector_path();
    let first = reconcile(api.as_ref(), &desired(URL_A), &path)
        .await
        .expect("record");

    let second = reconcile(api.as_ref(), &desired(URL_B), &path)
        .await
        .expect("record");

    assert_ne!(second.connector_id, first.connector_id);
    assert_eq!(second.mcp_url, URL_B);
    assert_eq!(api.connectors().len(), 1);
    assert_eq!(api.links().len(), 1);
    assert_eq!(api.count("DeleteLink"), 1);
    assert_eq!(api.count("DeleteConnector"), 1);
}

#[tokio::test]
async fn verify_asks_for_a_refresh_once_when_create_lacked_actions() {
    let api = FakeApi::new(true).with(|state| {
        state.create_actions.push_back(Vec::new());
    });
    let (_temp, path) = temp_connector_path();

    let record = reconcile(api.as_ref(), &desired(URL_A), &path)
        .await
        .expect("record");

    assert_eq!(record.actions, actions());
    assert_eq!(api.count("RefreshActions"), 1);
    assert_eq!(api.count("Create"), 1);
}

#[tokio::test]
async fn a_persistent_verify_mismatch_recreates_once_then_fails() {
    // Every connector this account creates reports a stale contract.
    let api = FakeApi::new(true).with(|state| {
        state.create_actions.push_back(vec!["codex_exec".into()]);
        state.create_actions.push_back(vec!["codex_exec".into()]);
        state.verify_actions = vec![
            (format!("asdk_app_{:032x}", 1), vec!["codex_exec".into()]),
            (format!("asdk_app_{:032x}", 3), vec!["codex_exec".into()]),
        ];
    });
    let (_temp, path) = temp_connector_path();

    let failure = reconcile(api.as_ref(), &desired(URL_A), &path)
        .await
        .expect_err("fails");

    assert!(
        matches!(failure.status, RegistryStatus::Failed { .. }),
        "{failure:?}"
    );
    assert!(failure.message.contains("expected"), "{}", failure.message);
    assert_eq!(api.count("Create"), 2, "{:?}", api.kinds());
    assert!(api.count("DeleteConnector") >= 1);
    assert!(!path.exists());
}

#[tokio::test]
async fn a_verify_mismatch_is_fixed_by_recreating() {
    let api = FakeApi::new(true).with(|state| {
        // The first connector is stuck on an old contract; the recreated one
        // is fine.
        state.create_actions.push_back(vec!["codex_exec".into()]);
        state.verify_actions = vec![(format!("asdk_app_{:032x}", 1), vec!["codex_exec".into()])];
    });
    let (_temp, path) = temp_connector_path();

    let record = reconcile(api.as_ref(), &desired(URL_A), &path)
        .await
        .expect("record");

    assert_eq!(record.actions, actions());
    assert_eq!(api.count("Create"), 2);
    assert_eq!(api.connectors().len(), 1);
}

#[tokio::test]
async fn developer_mode_off_is_enabled_then_the_plan_proceeds() {
    let api = FakeApi::new(false);
    let (_temp, path) = temp_connector_path();

    let record = reconcile(api.as_ref(), &desired(URL_A), &path)
        .await
        .expect("record");

    assert_eq!(record.actions, actions());
    let kinds = api.kinds();
    assert_eq!(kinds[0], "ReadDeveloperMode");
    assert_eq!(kinds[1], "EnableDeveloperMode");
    assert_eq!(api.count("EnableDeveloperMode"), 1);
    assert!(api.state.lock().expect("state").developer_mode);
}

#[tokio::test]
async fn a_403_mid_plan_enables_developer_mode_and_retries() {
    // The setting reads as on, but the connector endpoints still refuse.
    let api = FakeApi::new(true).fail(
        "ListConnectors",
        ApiError {
            status: Some(403),
            message: "Developer mode is required".into(),
            developer_mode_required: true,
            ..ApiError::default()
        },
    );
    let (_temp, path) = temp_connector_path();

    let record = reconcile(api.as_ref(), &desired(URL_A), &path)
        .await
        .expect("record");

    assert_eq!(record.actions, actions());
    assert_eq!(api.count("EnableDeveloperMode"), 1);
    assert_eq!(api.count("Create"), 1);
}

#[tokio::test]
async fn developer_mode_that_stays_off_is_reported() {
    let api = FakeApi::new(false).with(|state| {
        // Enabling "succeeds" but the account keeps refusing.
        state.failures.push_back((
            "EnableDeveloperMode",
            ApiError {
                status: Some(403),
                message: "Developer mode is required".into(),
                developer_mode_required: true,
                ..ApiError::default()
            },
        ));
    });
    let (_temp, path) = temp_connector_path();

    let failure = reconcile(api.as_ref(), &desired(URL_A), &path)
        .await
        .expect_err("fails");

    assert_eq!(failure.status, RegistryStatus::DeveloperModeOff);
}

#[tokio::test]
async fn a_rate_limited_api_becomes_failed_with_a_retry_time() {
    let api = FakeApi::new(true).fail(
        "Create",
        ApiError {
            status: Some(429),
            message: "Too many requests".into(),
            rate_limited: true,
            ..ApiError::default()
        },
    );
    let (_temp, path) = temp_connector_path();
    let before = now_ms();

    let failure = reconcile(api.as_ref(), &desired(URL_A), &path)
        .await
        .expect_err("fails");

    match failure.status {
        RegistryStatus::Failed {
            reason,
            retry_at_ms,
            kind,
            parked,
        } => {
            assert!(reason.contains("429"), "{reason}");
            // FORK: a 429 is worth retrying, but not at the transient cadence.
            assert_eq!(kind, FailureKind::RateLimited);
            assert!(!kind.is_terminal());
            assert!(!parked);
            assert!(retry_at_ms >= before + FAILURE_BACKOFF[0].as_millis() as u64);
        }
        other => panic!("unexpected status {other:?}"),
    }
    assert!(!path.exists());
}

#[tokio::test]
async fn an_unreachable_browser_is_reported_as_such() {
    let api = FakeApi::new(true).fail(
        "ReadDeveloperMode",
        ApiError::browser_unavailable("chrome-mcp daemon unreachable"),
    );
    // ReadDeveloperMode failures are tolerated; make the lists fail too.
    let api = api.fail(
        "ListConnectors",
        ApiError::browser_unavailable("chrome-mcp daemon unreachable"),
    );
    let (_temp, path) = temp_connector_path();

    let failure = reconcile(api.as_ref(), &desired(URL_A), &path)
        .await
        .expect_err("fails");

    assert_eq!(failure.status, RegistryStatus::BrowserUnavailable);
}

#[tokio::test]
async fn delete_recorded_removes_our_connectors_and_forgets_the_record() {
    let api = FakeApi::new(true).with(|state| {
        state.connectors = vec![
            connector("asdk_app_mine", "Codex Native", URL_A),
            connector("asdk_app_old", "Codex Native 1", URL_B),
            connector("asdk_app_theirs", "Gmail", "https://gmail"),
        ];
        state.links = vec![
            link("link_mine", "asdk_app_mine", "Codex Native"),
            link("link_theirs", "asdk_app_theirs", "Gmail"),
        ];
    });
    let (_temp, path) = temp_connector_path();
    persist_record(&path, &record("asdk_app_mine", "link_mine", URL_A)).expect("persist");

    let deleted = delete_recorded(api.as_ref(), "Codex Native", &path)
        .await
        .expect("deleted");

    assert_eq!(deleted.len(), 2, "{deleted:?}");
    let remaining: Vec<String> = api.connectors().into_iter().map(|c| c.id).collect();
    assert_eq!(remaining, vec!["asdk_app_theirs".to_string()]);
    assert_eq!(api.links().len(), 1);
    assert!(!path.exists());
}

// ---------------------------------------------------------------------------
// Service: status, backoff gating, tunnel watch.

fn service_for(
    api: Arc<FakeApi>,
    path: PathBuf,
    state: TunnelState,
) -> (Arc<RegistryService>, watch::Sender<TunnelState>) {
    let (tx, rx) = watch::channel(state);
    let status = Arc::new(Mutex::new(RegistryStatus::Unknown));
    let service = RegistryService::new(api, "Codex Native", "Codex tools", path, rx, status);
    (service, tx)
}

#[tokio::test]
async fn the_service_verifies_when_the_tunnel_is_ready_and_waits_otherwise() {
    let api = FakeApi::new(true);
    let (_temp, path) = temp_connector_path();
    let (service, tx) = service_for(Arc::clone(&api), path, TunnelState::Connecting);

    let status = service.reconcile_now(ReconcileTrigger::Watcher).await;
    assert!(
        matches!(status, RegistryStatus::Failed { ref reason, .. } if reason.contains("tunnel not ready")),
        "{status:?}"
    );
    assert_eq!(api.count("Create"), 0);

    // The not-ready failure has a short retry; wait it out.
    tokio::time::sleep(TUNNEL_NOT_READY_RETRY + Duration::from_millis(50)).await;
    tx.send(TunnelState::Ready {
        endpoint: public(URL_A),
    })
    .expect("send");
    let status = service.reconcile_now(ReconcileTrigger::Watcher).await;
    assert!(
        matches!(status, RegistryStatus::Verified { ref mcp_url, .. } if mcp_url == URL_A),
        "{status:?}"
    );
    assert!(service.record_matches_endpoint());
}

#[tokio::test]
async fn a_failed_reconcile_is_not_retried_before_its_backoff() {
    let api = FakeApi::new(true).fail(
        "Create",
        ApiError {
            status: Some(500),
            message: "boom".into(),
            ..ApiError::default()
        },
    );
    let (_temp, path) = temp_connector_path();
    let (service, _tx) = service_for(
        Arc::clone(&api),
        path,
        TunnelState::Ready {
            endpoint: public(URL_A),
        },
    );

    let first = service.reconcile_now(ReconcileTrigger::Watcher).await;
    assert!(matches!(first, RegistryStatus::Failed { .. }), "{first:?}");
    let creates = api.count("Create");

    let second = service.reconcile_now(ReconcileTrigger::Watcher).await;
    assert_eq!(second, first);
    assert_eq!(
        api.count("Create"),
        creates,
        "no API call during the backoff"
    );
}

#[tokio::test]
async fn the_watcher_reconciles_on_start_and_again_on_a_new_endpoint() {
    let api = FakeApi::new(true);
    let (_temp, path) = temp_connector_path();
    let (service, tx) = service_for(
        Arc::clone(&api),
        path,
        TunnelState::Ready {
            endpoint: public(URL_A),
        },
    );
    let cancel = CancellationToken::new();
    let watcher = service.spawn_watcher(cancel.clone());

    let mut verified = false;
    for _ in 0..100 {
        if matches!(service.status(), RegistryStatus::Verified { .. }) {
            verified = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(verified, "{:?}", service.status());
    assert_eq!(api.count("Create"), 1);

    tx.send(TunnelState::Ready {
        endpoint: public(URL_B),
    })
    .expect("send");
    let mut moved = false;
    for _ in 0..100 {
        if matches!(service.status(), RegistryStatus::Verified { ref mcp_url, .. } if mcp_url == URL_B)
        {
            moved = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(moved, "{:?}", service.status());
    assert_eq!(api.count("Create"), 2);

    cancel.cancel();
    let _ = tokio::time::timeout(Duration::from_secs(2), watcher).await;
}

// ---------------------------------------------------------------------------
// The page API over a fake chrome-mcp daemon.

/// Records tool calls and answers `browser_eval` from a queue of page
/// responses (what the `api_call` script would `JSON.stringify`).
struct FakeTabDaemon {
    tabs: Vec<Value>,
    calls: StdMutex<Vec<(String, Value)>>,
    evals: StdMutex<Vec<String>>,
    responses: StdMutex<VecDeque<Value>>,
}

impl FakeTabDaemon {
    fn new(tabs: Vec<Value>, responses: Vec<Value>) -> Arc<Self> {
        Arc::new(Self {
            tabs,
            calls: StdMutex::new(Vec::new()),
            evals: StdMutex::new(Vec::new()),
            responses: StdMutex::new(responses.into()),
        })
    }

    fn calls(&self) -> Vec<(String, Value)> {
        self.calls.lock().expect("calls").clone()
    }

    fn evals(&self) -> Vec<String> {
        self.evals.lock().expect("evals").clone()
    }
}

impl TabDaemon for FakeTabDaemon {
    fn call<'a>(
        &'a self,
        tool: &'a str,
        args: Value,
        _timeout_ms: u64,
    ) -> BoxFuture<'a, DriverResult<ToolResult>> {
        self.calls
            .lock()
            .expect("calls")
            .push((tool.to_string(), args.clone()));
        let text = match args.get("action").and_then(Value::as_str) {
            Some("list") => Value::Array(self.tabs.clone()).to_string(),
            Some("create") => json!({"id": 777}).to_string(),
            _ => json!({"ok": true}).to_string(),
        };
        async move {
            Ok(ToolResult {
                text,
                images: Vec::new(),
            })
        }
        .boxed()
    }

    fn eval_in<'a>(
        &'a self,
        _tab_id: TabId,
        expression: String,
        _timeout_ms: u64,
    ) -> BoxFuture<'a, DriverResult<Value>> {
        let is_probe = expression.contains("readyState");
        let is_login_probe = expression.contains("codex-login-probe");
        // The login probe is a read-only pre-check, not an API call the
        // tests count.
        if !is_login_probe {
            self.evals.lock().expect("evals").push(expression);
        }
        let next = if is_probe {
            Some(json!({"ready": true, "path": "/", "href": "https://chatgpt.com/"}))
        } else if is_login_probe {
            Some(json!({"ok": true}))
        } else {
            self.responses.lock().expect("responses").pop_front()
        };
        async move { next.ok_or_else(|| DriverError::other("no scripted page response left")) }
            .boxed()
    }
}

fn page(status: u16, json: Value) -> Value {
    // The page script resolves a JSON *string*; the daemon client hands it
    // over decoded or not — both must work.
    Value::String(json!({"status": status, "json": json, "text": Value::Null}).to_string())
}

fn chatgpt_tab(id: i64) -> Value {
    json!({"id": id, "url": "https://chatgpt.com/c/abc", "title": "ChatGPT", "active": false})
}

#[tokio::test]
async fn the_page_api_retries_429_with_backoff_then_succeeds() {
    let daemon = FakeTabDaemon::new(
        vec![chatgpt_tab(5)],
        vec![
            page(429, json!({"detail": "Too many requests"})),
            page(
                200,
                json!({"connectors": [{"id": "asdk_app_1", "name": "Codex Native", "base_url": URL_A}]}),
            ),
        ],
    );
    let api = ChromeMcpPageApi::with_daemon(daemon.clone(), "https://chatgpt.com")
        .with_backoff(vec![Duration::from_millis(1)])
        .with_registry_path(None);

    let result = api.call(&RegistryOp::ListConnectors).await.expect("ok");

    assert_eq!(
        result,
        ApiResult::Connectors(vec![ObservedConnector {
            id: "asdk_app_1".into(),
            name: "Codex Native".into(),
            mcp_url: Some(URL_A.into()),
            tunnel_id: None,
            actions: Vec::new(),
        }])
    );
    assert_eq!(daemon.evals().len(), 2);
    // Borrowed the existing tab: no create, and close does not touch it.
    api.close().await;
    let tools: Vec<String> = daemon
        .calls()
        .into_iter()
        .map(|(tool, args)| format!("{tool}:{}", args["action"].as_str().unwrap_or("")))
        .collect();
    assert_eq!(tools, vec!["browser_tabs:list".to_string()]);
}

#[tokio::test]
async fn the_page_api_creates_a_dedicated_tab_when_none_exists_and_closes_it() {
    let temp = tempfile::tempdir().expect("tempdir");
    let registry = temp.path().join("tabs.json");
    let daemon = FakeTabDaemon::new(
        vec![json!({"id": 1, "url": "https://example.com/"})],
        vec![page(200, json!({"settings": {"developer_mode": true}}))],
    );
    let api = ChromeMcpPageApi::with_daemon(daemon.clone(), "https://chatgpt.com/")
        .with_registry_path(Some(registry.clone()));

    api.open().await.expect("open");
    let owners = crate::chatgpt_web::driver::tabs::load_registry(&registry).owners;
    assert_eq!(owners.len(), 1);
    assert_eq!(owners[0].tab_id, 777);
    assert_eq!(owners[0].pid, Some(std::process::id()));

    let result = api.call(&RegistryOp::ReadDeveloperMode).await.expect("ok");
    assert_eq!(result, ApiResult::DeveloperMode(true));
    api.close().await;

    let tools: Vec<String> = daemon
        .calls()
        .into_iter()
        .map(|(tool, args)| format!("{tool}:{}", args["action"].as_str().unwrap_or("")))
        .collect();
    assert_eq!(
        tools,
        vec![
            "browser_tabs:list".to_string(),
            "browser_tabs:create".to_string(),
            "browser_tabs:close".to_string(),
        ]
    );
    let create = &daemon.calls()[1].1;
    assert_eq!(create["url"], "https://chatgpt.com/");
    assert_eq!(create["dedicated"], true);
    assert!(
        crate::chatgpt_web::driver::tabs::load_registry(&registry)
            .owners
            .is_empty()
    );
}

#[tokio::test]
async fn the_page_api_sends_the_captured_bodies_and_headers() {
    let daemon = FakeTabDaemon::new(
        vec![chatgpt_tab(5)],
        vec![
            page(
                200,
                json!({"connector": {"id": "asdk_app_new", "actions": [{"name": "codex_exec"}, {"name": "codex_write_stdin"}]}}),
            ),
            page(
                200,
                json!({"id": "link_new", "connector_id": "asdk_app_new", "actions": ["codex_exec"]}),
            ),
            page(200, json!({"actions": [{"name": "codex_exec"}]})),
            page(200, json!({"developer_mode": true})),
            page(200, json!({"tunnels": [{"id": "tunnel_a"}, "tunnel_b"]})),
            page(404, json!({"detail": "Connector not found"})),
        ],
    );
    let api = ChromeMcpPageApi::with_daemon(daemon.clone(), "https://chatgpt.com")
        .with_registry_path(None);

    let created = api
        .call(&RegistryOp::Create(desired(URL_A)))
        .await
        .expect("created");
    assert_eq!(
        created,
        ApiResult::Created {
            connector_id: "asdk_app_new".into(),
            actions: vec!["codex_exec".into(), "codex_write_stdin".into()],
        }
    );
    let linked = api
        .call(&RegistryOp::Link {
            connector: ConnectorRef::Id("asdk_app_new".into()),
            name: "Codex Native".into(),
        })
        .await
        .expect("linked");
    assert_eq!(
        linked,
        ApiResult::Linked {
            link_id: "link_new".into(),
            actions: vec!["codex_exec".into()],
        }
    );
    let verified = api
        .call(&RegistryOp::VerifyActions {
            connector: ConnectorRef::Id("asdk_app_new".into()),
            expect: actions(),
        })
        .await
        .expect("actions");
    assert_eq!(verified, ApiResult::Actions(vec!["codex_exec".into()]));
    let enabled = api
        .call(&RegistryOp::EnableDeveloperMode)
        .await
        .expect("enabled");
    assert_eq!(enabled, ApiResult::DeveloperMode(true));
    let tunnels = api.call(&RegistryOp::ListTunnels).await.expect("tunnels");
    assert_eq!(
        tunnels,
        ApiResult::Tunnels(vec!["tunnel_a".into(), "tunnel_b".into()])
    );
    // A 404 on delete is "already gone".
    let gone = api
        .call(&RegistryOp::DeleteConnector("asdk_app_new".into()))
        .await
        .expect("gone");
    assert_eq!(gone, ApiResult::Unit);

    let evals = daemon.evals();
    assert_eq!(evals.len(), 6);
    let create = &evals[0];
    assert!(
        create.contains(r#""https://chatgpt.com/backend-api/aip/connectors/mcp""#),
        "{create}"
    );
    assert!(create.contains(r#""POST""#));
    assert!(
        create.contains(r#""mcp_url":"https://a.trycloudflare.com/mcp/secret-a""#),
        "{create}"
    );
    assert!(create.contains(r#""supported_auth":[]"#), "{create}");
    assert!(create.contains(r#""name":"Codex Native""#), "{create}");
    assert!(
        create.contains(r#""OAI-Product-Sku":"CONNECTOR_SETTING""#),
        "{create}"
    );
    let link = &evals[1];
    assert!(
        link.contains("/backend-api/aip/connectors/links/noauth"),
        "{link}"
    );
    assert!(link.contains(r#""action_names":[]"#), "{link}");
    assert!(link.contains(r#""connector_id":"asdk_app_new""#), "{link}");
    assert!(evals[2].contains("/backend-api/aip/connectors/asdk_app_new/actions"));
    assert!(evals[3].contains("feature=developer_mode&value=true"));
    assert!(evals[3].contains(r#""PATCH""#));
    assert!(evals[4].contains("/backend-api/aip/connectors/mcp/tunnels"));
    assert!(evals[5].contains(r#""DELETE""#));
}

#[tokio::test]
async fn the_page_api_classifies_developer_mode_and_login_errors() {
    let daemon = FakeTabDaemon::new(
        vec![chatgpt_tab(5)],
        vec![
            page(403, json!({"detail": "Developer mode is required"})),
            Value::String(json!({"status": 0, "error": "Error: not logged in: /api/auth/session returned no accessToken"}).to_string()),
        ],
    );
    let api = ChromeMcpPageApi::with_daemon(daemon, "https://chatgpt.com").with_registry_path(None);

    let forbidden = api
        .call(&RegistryOp::ListConnectors)
        .await
        .expect_err("403");
    assert!(forbidden.developer_mode_required, "{forbidden:?}");
    assert_eq!(forbidden.status, Some(403));

    let logged_out = api
        .call(&RegistryOp::ListLinks)
        .await
        .expect_err("logged out");
    assert!(logged_out.login_required, "{logged_out:?}");
    assert!(logged_out.message.contains("not logged in"));
}

// ---------------------------------------------------------------------------
// Daemon wiring: a registered turn kicks a reconcile off in the background.

#[tokio::test]
async fn a_registered_turn_triggers_a_background_reconcile() {
    use super::super::DaemonRunConfig;
    use super::super::tunnel::NoopTunnel;
    use super::super::wire;
    use crate::config::ChatGptWebSettings;

    let temp = tempfile::tempdir().expect("tempdir");
    let calls = Arc::new(AtomicUsize::new(0));
    let hook_calls = Arc::clone(&calls);
    let hook: ReconcileHook = Arc::new(move |trigger| {
        assert_eq!(trigger, ReconcileTrigger::Turn);
        let hook_calls = Arc::clone(&hook_calls);
        Box::pin(async move {
            hook_calls.fetch_add(1, Ordering::SeqCst);
            Ok(RegistryStatus::Verified {
                connector_id: "asdk_app_1".into(),
                link_id: "link_1".into(),
                mcp_url: URL_A.into(),
            })
        })
    });
    let mut config = DaemonRunConfig::new(ChatGptWebSettings::default(), temp.path().to_path_buf());
    config.tunnel_override = Some(Arc::new(NoopTunnel {
        endpoint: public(URL_A),
    }));
    config.reconcile = Some(hook);
    let daemon = super::super::start(config).await.expect("starts");
    let http = reqwest::Client::new();
    let base = daemon.endpoint.control_url.clone();
    let token = daemon.endpoint.token.clone();

    let session = http
        .post(format!("{base}/v1/sessions"))
        .bearer_auth(&token)
        .json(&wire::RegisterSessionRequest {
            codex_pid: std::process::id(),
            session_id: "sess-1".into(),
            codex_version: "test".into(),
        })
        .send()
        .await
        .expect("session");
    assert!(session.status().is_success());
    let turn = http
        .post(format!("{base}/v1/turns"))
        .bearer_auth(&token)
        .json(&wire::RegisterTurnRequest {
            session_id: "sess-1".into(),
            turn_token: "turn-token-0123456789abcdef".into(),
            thread_id: "thread-1".into(),
            turn_id: "turn-1".into(),
            ttl_ms: 60_000,
            tools: Vec::new(),
            exec_tool: contract::ExecTool::ExecCommand,
            apply_patch: false,
        })
        .send()
        .await
        .expect("turn");
    assert!(turn.status().is_success(), "{}", turn.status());

    let mut verified = false;
    for _ in 0..100 {
        if matches!(
            daemon.control.registry_status(),
            RegistryStatus::Verified { .. }
        ) {
            verified = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(verified);
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    daemon.shutdown().await;
}

// ---------------------------------------------------------------------------
// Live: needs the chrome-mcp daemon and a logged-in chatgpt.com.
//
//   RUST_MIN_STACK=8388608 cargo test -p codex-core --lib \
//     chatgpt_web::connector::daemon::registry -- --ignored --nocapture
//
// `CHATGPT_WEB_LIVE_MCP_URL` (optional) points the connector at a reachable
// MCP server (e.g. the spike server behind cloudflared) so the 6 actions can
// be verified; without it the URL is unreachable and the reconcile is
// expected to fail at `VerifyActions` — either way nothing is left behind.

#[tokio::test]
#[ignore]
async fn live_registry_reconciles_a_manual_url() {
    let settings = crate::config::ChatGptWebSettings::default();
    let api = ChromeMcpPageApi::from_settings(&settings);
    let temp = tempfile::tempdir().expect("tempdir");
    let path = temp.path().join("connector.json");
    let name = "Codex Native Live Test";
    let url = std::env::var("CHATGPT_WEB_LIVE_MCP_URL")
        .unwrap_or_else(|_| "https://example.invalid/mcp/test".to_string());
    let reachable = std::env::var("CHATGPT_WEB_LIVE_MCP_URL").is_ok();
    let wanted = DesiredConnector::for_endpoint(name, "Codex live registry test", public(&url));

    let outcome = reconcile(&api, &wanted, &path).await;

    let deleted = delete_recorded(&api, name, &path)
        .await
        .expect("cleanup must work");

    // Whatever happened, nothing carrying our name may remain.
    api.open().await.expect("open");
    let remaining = match api.call(&RegistryOp::ListConnectors).await.expect("list") {
        ApiResult::Connectors(connectors) => connectors
            .into_iter()
            .filter(|c| c.name.starts_with(name))
            .count(),
        _ => 0,
    };
    api.close().await;
    assert_eq!(remaining, 0, "cleanup deleted {deleted:?}");

    match outcome {
        Ok(record) => {
            assert_eq!(record.actions.len(), 6);
            assert!(reachable, "unreachable URL should not verify: {record:?}");
        }
        Err(failure) => {
            assert!(!reachable, "reachable URL should verify: {failure:?}");
            assert!(
                matches!(failure.status, RegistryStatus::Failed { .. }),
                "{failure:?}"
            );
        }
    }
}

// ---------------------------------------------------------------------------
// FORK: failure classification, the slow ladder, and parking.

#[test]
fn backoff_is_slow_for_terminal_failures_and_capped_for_transient_ones() {
    // A transient failure retries quickly at first and then settles at the cap.
    assert_eq!(backoff_for(FailureKind::Transient, 0), FAILURE_BACKOFF[0]);
    assert_eq!(backoff_for(FailureKind::Transient, 3), FAILURE_BACKOFF[3]);
    assert_eq!(backoff_for(FailureKind::Transient, 99), FAILURE_BACKOFF_CAP);

    // A failure the user has to fix gets three widely spaced attempts; each one
    // costs a dedicated chatgpt.com tab, so the first retry is a full minute.
    assert_eq!(
        backoff_for(FailureKind::TunnelNotVisible, 0),
        TERMINAL_BACKOFF[0]
    );
    assert_eq!(
        backoff_for(FailureKind::LoginRequired, 1),
        TERMINAL_BACKOFF[1]
    );
    assert_eq!(
        backoff_for(FailureKind::SetupRequired, 99),
        TERMINAL_BACKOFF[TERMINAL_BACKOFF.len() - 1]
    );
    assert!(backoff_for(FailureKind::TunnelNotVisible, 0) >= Duration::from_secs(60));
}

#[test]
fn map_api_classifies_failure_kinds() {
    let kind_of = |error: ApiError| match map_api(error).status {
        RegistryStatus::Failed { kind, .. } => Some(kind),
        _ => None,
    };

    assert_eq!(
        kind_of(ApiError {
            status: Some(401),
            message: "not logged in".into(),
            login_required: true,
            ..ApiError::default()
        }),
        Some(FailureKind::LoginRequired)
    );
    assert_eq!(
        kind_of(ApiError {
            status: Some(429),
            message: "slow down".into(),
            rate_limited: true,
            ..ApiError::default()
        }),
        Some(FailureKind::RateLimited)
    );
    assert_eq!(
        kind_of(ApiError {
            status: Some(500),
            message: "boom".into(),
            ..ApiError::default()
        }),
        Some(FailureKind::Transient)
    );
    // These two keep their own statuses rather than becoming `Failed`.
    assert!(matches!(
        map_api(ApiError::browser_unavailable("no extension")).status,
        RegistryStatus::BrowserUnavailable
    ));
    assert!(matches!(
        map_api(ApiError {
            developer_mode_required: true,
            ..ApiError::new("developer mode")
        })
        .status,
        RegistryStatus::DeveloperModeOff
    ));
}

/// A tunnel the ChatGPT account cannot see is the case that produced 60 tabs an
/// hour: the same terminal failure, forever, one dedicated tab per attempt.
fn service_with_invisible_tunnel(
    path: PathBuf,
) -> (Arc<RegistryService>, Arc<FakeApi>, watch::Sender<TunnelState>) {
    let api = FakeApi::new(true).with(|state| {
        state.tunnels = vec!["tunnel_ffffffffffffffffffffffffffffffff".into()];
    });
    let (service, tx) = service_for(
        Arc::clone(&api),
        path,
        TunnelState::Ready {
            endpoint: TunnelEndpoint::OpenAi {
                tunnel_id: "tunnel_0123456789abcdef0123456789abcdef".into(),
            },
        },
    );
    (service, api, tx)
}

#[tokio::test]
async fn identical_terminal_failures_climb_the_ladder_and_park() {
    let (_temp, path) = temp_connector_path();
    let (service, api, _tx) = service_with_invisible_tunnel(path);

    let mut last = RegistryStatus::Unknown;
    for attempt in 1..=PARK_AFTER_IDENTICAL_TERMINAL_FAILURES {
        // Drive the watcher's own ladder without sleeping through 60s, 5min
        // and 30min. `Manual` would not do: it deliberately forgets the failure
        // history, which is exactly what is being counted here.
        service.advance_clock_for_tests();
        last = service.reconcile_now(ReconcileTrigger::Watcher).await;
        let RegistryStatus::Failed { kind, parked, .. } = &last else {
            panic!("expected a failure, got {last:?}");
        };
        assert_eq!(*kind, FailureKind::TunnelNotVisible);
        assert_eq!(
            *parked,
            attempt >= PARK_AFTER_IDENTICAL_TERMINAL_FAILURES,
            "parked after {attempt} attempt(s): {last:?}"
        );
    }
    let attempts = api.count("ListTunnels");

    // Parked: the watcher stops asking, so no more tabs.
    let watched = service.reconcile_now(ReconcileTrigger::Watcher).await;
    assert_eq!(watched, last);
    assert_eq!(api.count("ListTunnels"), attempts);
}

#[tokio::test]
async fn a_parked_registry_reconciles_once_when_a_turn_asks() {
    let (_temp, path) = temp_connector_path();
    let (service, api, _tx) = service_with_invisible_tunnel(path);
    for _ in 0..PARK_AFTER_IDENTICAL_TERMINAL_FAILURES {
        service.advance_clock_for_tests();
        service.reconcile_now(ReconcileTrigger::Watcher).await;
    }
    let attempts = api.count("ListTunnels");

    // The user in front of us may well have just fixed it.
    service.advance_clock_for_tests();
    service.reconcile_now(ReconcileTrigger::Turn).await;
    assert_eq!(api.count("ListTunnels"), attempts + 1);

    // But two turns in the same breath are one attempt.
    service.reconcile_now(ReconcileTrigger::Turn).await;
    assert_eq!(api.count("ListTunnels"), attempts + 1);
}

#[tokio::test]
async fn a_parked_registry_resumes_on_a_tunnel_change() {
    let (_temp, path) = temp_connector_path();
    let (service, api, _tx) = service_with_invisible_tunnel(path);
    for _ in 0..PARK_AFTER_IDENTICAL_TERMINAL_FAILURES {
        service.advance_clock_for_tests();
        service.reconcile_now(ReconcileTrigger::Watcher).await;
    }
    assert!(matches!(
        service.status(),
        RegistryStatus::Failed { parked: true, .. }
    ));
    let attempts = api.count("ListTunnels");

    // A new endpoint is exactly the kind of change that can fix this.
    let status = service.reconcile_now(ReconcileTrigger::TunnelChange).await;
    assert_eq!(api.count("ListTunnels"), attempts + 1);
    // The ladder starts over: this failure is the first one again, not the
    // fourth, so it is not parked.
    assert!(
        matches!(status, RegistryStatus::Failed { parked: false, .. }),
        "{status:?}"
    );
}

#[tokio::test]
async fn a_manual_reconcile_resets_the_backoff() {
    let api = FakeApi::new(true).fail(
        "Create",
        ApiError {
            status: Some(500),
            message: "boom".into(),
            ..ApiError::default()
        },
    );
    let (_temp, path) = temp_connector_path();
    let (service, _tx) = service_for(
        Arc::clone(&api),
        path,
        TunnelState::Ready {
            endpoint: public(URL_A),
        },
    );

    let failed = service.reconcile_now(ReconcileTrigger::Watcher).await;
    assert!(matches!(failed, RegistryStatus::Failed { .. }), "{failed:?}");
    let creates = api.count("Create");

    // The watcher waits out the backoff; a manual reconcile does not.
    assert_eq!(service.reconcile_now(ReconcileTrigger::Watcher).await, failed);
    assert_eq!(api.count("Create"), creates);

    let status = service.reconcile_now(ReconcileTrigger::Manual).await;
    assert!(api.count("Create") > creates);
    assert!(
        matches!(status, RegistryStatus::Verified { .. }),
        "the injected failure was consumed: {status:?}"
    );
}

// ---------------------------------------------------------------------------
// FORK: naming the account behind a tunnel-visibility refusal.

#[test]
fn a_tunnel_refusal_names_the_account_when_it_is_known() {
    let wanted = DesiredConnector::for_endpoint(
        "Codex Native",
        "Codex tools",
        TunnelEndpoint::OpenAi {
            tunnel_id: "tunnel_0123456789abcdef0123456789abcdef".into(),
        },
    );
    let observed = Observed {
        developer_mode: Some(true),
        known_tunnels: Some(vec!["tunnel_ffffffffffffffffffffffffffffffff".into()]),
        account: Some(AccountInfo {
            account_id: "b7000e3e-0000-4000-8000-000000000000".into(),
            email: Some("someone@example.com".into()),
            plan_type: Some("plus".into()),
        }),
        ..Observed::default()
    };

    let refusal = plan(&observed, &wanted).expect_err("refused");

    assert!(
        refusal
            .reason
            .contains("b7000e3e-0000-4000-8000-000000000000"),
        "{}",
        refusal.reason
    );
    assert!(refusal.reason.contains("someone@example.com"), "{}", refusal.reason);
    assert!(refusal.reason.contains("plus"), "{}", refusal.reason);
}

#[tokio::test]
async fn observe_reads_the_account_only_when_the_tunnel_is_missing() {
    let tunnel_id = "tunnel_0123456789abcdef0123456789abcdef";
    let wanted = DesiredConnector::for_endpoint(
        "Codex Native",
        "Codex tools",
        TunnelEndpoint::OpenAi {
            tunnel_id: tunnel_id.into(),
        },
    );

    // Listed: no reason to spend two page fetches on the account.
    let listed = FakeApi::new(true).with(|state| state.tunnels = vec![tunnel_id.into()]);
    let observed = observe(listed.as_ref(), &wanted, None)
        .await
        .expect("observe");
    assert_eq!(listed.count("ReadAccount"), 0);
    assert_eq!(observed.account, None);

    // Missing: the account is exactly what the refusal needs to name.
    let missing = FakeApi::new(true).with(|state| {
        state.tunnels = vec!["tunnel_ffffffffffffffffffffffffffffffff".into()];
        state.account = AccountInfo {
            account_id: "b7000e3e".into(),
            email: Some("someone@example.com".into()),
            plan_type: None,
        };
    });
    let observed = observe(missing.as_ref(), &wanted, None)
        .await
        .expect("observe");
    assert_eq!(missing.count("ReadAccount"), 1);
    assert_eq!(
        observed.account.as_ref().map(AccountInfo::describe),
        Some("b7000e3e (someone@example.com)".to_string())
    );
}

#[test]
fn a_public_endpoint_never_reads_the_account() {
    // Cloudflared has no tunnel audience at all, so nothing to look up.
    let wanted = desired(URL_A);
    let observed = Observed {
        developer_mode: Some(true),
        ..Observed::default()
    };
    assert!(plan(&observed, &wanted).is_ok());
}

/// FORK: `ReadAccount` is two page fetches — `accounts/check` for the id and
/// plan, `auth/session` for the email — and every field is optional because
/// both shapes have moved before.
#[tokio::test]
async fn the_page_api_reads_the_account_and_email() {
    let daemon = FakeTabDaemon::new(
        vec![chatgpt_tab(5)],
        vec![
            page(
                200,
                json!({
                    "account_ordering": ["b7000e3e-0000-4000-8000-000000000000"],
                    "accounts": {
                        "b7000e3e-0000-4000-8000-000000000000": {
                            "account": { "plan_type": "plus" }
                        }
                    }
                }),
            ),
            page(200, json!({ "user": { "email": "someone@example.com" } })),
        ],
    );
    let api = ChromeMcpPageApi::with_daemon(daemon.clone(), "https://chatgpt.com")
        .with_registry_path(None);

    let result = api.call(&RegistryOp::ReadAccount).await.expect("ok");

    assert_eq!(
        result,
        ApiResult::Account(AccountInfo {
            account_id: "b7000e3e-0000-4000-8000-000000000000".into(),
            email: Some("someone@example.com".into()),
            plan_type: Some("plus".into()),
        })
    );
    api.close().await;
}
