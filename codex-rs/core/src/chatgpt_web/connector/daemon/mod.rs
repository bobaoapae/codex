//! FORK: the shared `codex chatgpt-web daemon`.
//!
//! One instance per `CODEX_HOME` owns the tunnel to ChatGPT, the public MCP
//! server with the fixed contract, the turn broker and (C2) the connector
//! registry. Codex sessions reach it over the loopback control API; ChatGPT
//! reaches it through the tunnel. See PLANO.md, "Modo conector".

pub mod broker;
pub mod control;
pub mod public_server;
pub mod registry;
pub mod registry_api;
pub mod state;
pub mod tunnel;
pub mod wire;

use crate::config::ChatGptWebSettings;
use anyhow::Context;
use anyhow::anyhow;
use codex_config::config_toml::ChatGptWebTunnel;
use state::ConnectorRecord;
use state::DaemonPaths;
use state::DaemonState;
use state::InstanceLock;
use state::LockError;
use state::RegistryStatus;
use std::net::SocketAddr;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use std::time::Duration;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use tunnel::TunnelAdapter;
use tunnel::TunnelEndpoint;
use tunnel::TunnelHandle;
use tunnel::TunnelState;

pub const DAEMON_VERSION: &str = env!("CARGO_PKG_VERSION");

/// How long autostart waits for a freshly spawned daemon to answer.
const AUTOSTART_TIMEOUT: Duration = Duration::from_secs(15);
const STOP_TIMEOUT: Duration = Duration::from_secs(10);

/// What `run`/`start` need.
pub struct DaemonRunConfig {
    pub settings: ChatGptWebSettings,
    pub codex_home: PathBuf,
    /// Stay attached to the terminal (logs to stderr).
    pub foreground: bool,
    /// Exit after this long with no sessions; `None` = never.
    pub idle_shutdown: Option<Duration>,
    /// Replaces the configured tunnel (tests, `manual`).
    pub tunnel_override: Option<Arc<dyn TunnelAdapter>>,
    /// Registry reconcile hook. `None` + `live_registry = false` → the
    /// registry reports `NotImplemented` (tests); `None` + `live_registry =
    /// true` → the real registry over chrome-mcp (`registry_api`).
    pub reconcile: Option<control::ReconcileHook>,
    /// FORK (C2): run the connector registry against the real chatgpt.com
    /// account when no explicit hook is given.
    pub live_registry: bool,
}

impl DaemonRunConfig {
    pub fn new(settings: ChatGptWebSettings, codex_home: PathBuf) -> Self {
        Self {
            settings,
            codex_home,
            foreground: false,
            idle_shutdown: None,
            tunnel_override: None,
            reconcile: None,
            live_registry: false,
        }
    }

    /// The daemon as the CLI runs it: registry included.
    pub fn with_live_registry(mut self) -> Self {
        self.live_registry = true;
        self
    }
}

/// Where a running daemon can be reached.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DaemonEndpoint {
    pub control_url: String,
    pub token: String,
    pub pid: u32,
}

/// A daemon started in this process.
pub struct RunningDaemon {
    pub endpoint: DaemonEndpoint,
    pub control_addr: SocketAddr,
    pub public: public_server::PublicServer,
    pub tunnel: TunnelHandle,
    pub control: Arc<control::ControlState>,
    pub broker: Arc<broker::TurnBroker>,
    pub paths: DaemonPaths,
    cancel: CancellationToken,
    tasks: Vec<JoinHandle<()>>,
    _lock: InstanceLock,
}

impl std::fmt::Debug for RunningDaemon {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RunningDaemon")
            .field("control_addr", &self.control_addr)
            .field("public", &self.public)
            .finish_non_exhaustive()
    }
}

impl RunningDaemon {
    /// The loopback MCP URL the tunnel forwards to (contains the secret).
    pub fn local_mcp_url(&self) -> String {
        self.public.local_mcp_url()
    }

    pub fn cancel_token(&self) -> CancellationToken {
        self.cancel.clone()
    }

    /// Blocks until the daemon is asked to stop.
    pub async fn wait(&self) {
        self.cancel.cancelled().await;
    }

    pub async fn shutdown(mut self) {
        self.cancel.cancel();
        for task in self.tasks.drain(..) {
            let _ = tokio::time::timeout(Duration::from_secs(5), task).await;
        }
        let tunnel = std::mem::replace(
            &mut self.tunnel,
            tunnel::start(
                Arc::new(tunnel::NoopTunnel {
                    endpoint: TunnelEndpoint::Public {
                        mcp_url: String::new(),
                    },
                }),
                String::new(),
            ),
        );
        tunnel.shutdown().await;
        self.public.abort();
        let _ = std::fs::remove_file(&self.paths.state);
    }
}

fn http_client() -> reqwest::Client {
    reqwest::Client::builder()
        .no_proxy()
        .build()
        .unwrap_or_default()
}

/// Chooses the tunnel adapter from settings.
pub async fn build_tunnel_adapter(
    settings: &ChatGptWebSettings,
    paths: &DaemonPaths,
) -> Arc<dyn TunnelAdapter> {
    let http = http_client();
    match settings.tunnel {
        ChatGptWebTunnel::Manual => match settings.manual_mcp_url.clone() {
            Some(mcp_url) => Arc::new(tunnel::NoopTunnel {
                endpoint: TunnelEndpoint::Public { mcp_url },
            }),
            None => Arc::new(tunnel::FatalTunnel {
                reason: "`[chatgpt_web] tunnel = \"manual\"` needs `manual_mcp_url`".to_string(),
            }),
        },
        ChatGptWebTunnel::Cloudflared => {
            match tunnel::resolve_cloudflared(settings.cloudflared_path.as_deref()) {
                Some(binary) => Arc::new(tunnel::CloudflaredTunnel {
                    binary,
                    extra_args: settings.cloudflared_extra_args.clone(),
                    http,
                }),
                None => Arc::new(tunnel::FatalTunnel {
                    reason:
                        "cloudflared not found; install it or set `[chatgpt_web] cloudflared_path`"
                            .to_string(),
                }),
            }
        }
        ChatGptWebTunnel::Openai => {
            let Some(tunnel_id) = settings.tunnel_id.clone() else {
                return Arc::new(tunnel::FatalTunnel {
                    reason: "no `[chatgpt_web] tunnel_id`; run `codex chatgpt-web setup --tunnel-id <id> --api-key-file <path>` (or set `tunnel = \"cloudflared\"`)".to_string(),
                });
            };
            let key_path = settings
                .tunnel_key_file
                .clone()
                .unwrap_or_else(|| paths.tunnel_key.clone());
            let api_key = std::env::var("CODEX_CHATGPT_WEB_TUNNEL_KEY")
                .ok()
                .filter(|key| !key.trim().is_empty())
                .or_else(|| state::read_secret(&key_path));
            let Some(api_key) = api_key else {
                return Arc::new(tunnel::FatalTunnel {
                    reason: format!(
                        "no tunnel API key at {}; run `codex chatgpt-web setup` (or set `tunnel = \"cloudflared\"`)",
                        key_path.display()
                    ),
                });
            };
            let version = settings.tunnel_client_version.clone();
            let binary = match tunnel::resolve_tunnel_client(
                settings.tunnel_client_path.as_deref(),
                &paths.bin_dir,
                &version,
            ) {
                Some(binary) => binary,
                None => match tunnel::ensure_tunnel_client(&http, &paths.bin_dir, &version).await {
                    Ok(binary) => binary,
                    Err(error) => {
                        return Arc::new(tunnel::FatalTunnel {
                            reason: format!(
                                "tunnel-client unavailable: {error:#}; install it (brew install openai/tools/tunnel-client) or set `[chatgpt_web] tunnel_client_path`"
                            ),
                        });
                    }
                },
            };
            Arc::new(tunnel::OpenAiTunnel {
                binary,
                tunnel_id,
                api_key,
                http,
            })
        }
    }
}

/// Starts every component in this process. Fails when another daemon holds
/// the lock.
pub async fn start(config: DaemonRunConfig) -> anyhow::Result<RunningDaemon> {
    let paths = DaemonPaths::new(&config.codex_home);
    paths.ensure_dir()?;
    let lock = match InstanceLock::try_acquire(&paths.lock) {
        Ok(lock) => lock,
        Err(LockError::Held) => {
            return Err(anyhow!(
                "another chatgpt-web daemon is already running for {}",
                config.codex_home.display()
            ));
        }
        Err(LockError::Io(error)) => {
            return Err(error).context("acquiring the daemon lock");
        }
    };

    let cancel = CancellationToken::new();
    let token = state::new_token();
    state::write_secret(&paths.token, &token).context("writing daemon.token")?;

    let settings = &config.settings;
    let broker = broker::TurnBroker::new(broker::BrokerConfig {
        call_timeout: settings.connector_call_timeout,
        exec_default_yield_ms: settings.connector_exec_default_yield.as_millis() as u64,
        ..broker::BrokerConfig::default()
    });
    let public = public_server::start(
        Arc::clone(&broker),
        public_server::PublicServerConfig {
            port: settings.tunnel_port,
            cancel: cancel.clone(),
            ..public_server::PublicServerConfig::default()
        },
    )
    .await
    .context("starting the public MCP server")?;

    let adapter = match config.tunnel_override {
        Some(adapter) => adapter,
        None => build_tunnel_adapter(settings, &paths).await,
    };
    let tunnel = tunnel::start(adapter, public.local_mcp_url());

    let has_registry = config.reconcile.is_some() || config.live_registry;
    let registry = Arc::new(Mutex::new(if has_registry {
        RegistryStatus::Unknown
    } else {
        RegistryStatus::NotImplemented
    }));
    // FORK (C2): the live registry drives chatgpt.com through chrome-mcp;
    // it re-runs whenever the tunnel endpoint changes.
    let mut registry_service: Option<Arc<registry::RegistryService>> = None;
    let reconcile = match config.reconcile {
        Some(hook) => Some(hook),
        None if config.live_registry => {
            let api: Arc<dyn registry::ConnectorApi> =
                Arc::new(registry_api::ChromeMcpPageApi::from_settings(settings));
            let service = registry::RegistryService::new(
                api,
                &settings.connector_name,
                &settings.connector_description,
                paths.connector.clone(),
                tunnel.state(),
                Arc::clone(&registry),
            );
            let hook = service.hook();
            registry_service = Some(service);
            Some(hook)
        }
        None => None,
    };
    let control_state = Arc::new(control::ControlState {
        broker: Arc::clone(&broker),
        token: token.clone(),
        version: DAEMON_VERSION.to_string(),
        tunnel: tunnel.state(),
        registry,
        reconcile,
        shutdown: cancel.clone(),
        shutdown_when_idle: AtomicBool::new(false),
        reconcile_in_flight: AtomicBool::new(false),
    });
    let (control_addr, control_task) = control::start(
        Arc::clone(&control_state),
        settings.daemon_port,
        cancel.clone(),
    )
    .await
    .context("starting the control API")?;

    let mut tasks = vec![control_task];
    if let Some(service) = registry_service.as_ref() {
        tasks.push(service.spawn_watcher(cancel.clone()));
    }
    tasks.push(broker.spawn_sweeper(Duration::from_secs(5), cancel.clone()));
    tasks.push(spawn_state_writer(
        paths.clone(),
        control_addr.port(),
        tunnel.state(),
        Arc::clone(&control_state),
        cancel.clone(),
    ));
    tasks.push(spawn_idle_watch(
        Arc::clone(&control_state),
        config.idle_shutdown,
        cancel.clone(),
    ));

    Ok(RunningDaemon {
        endpoint: DaemonEndpoint {
            control_url: format!("http://{control_addr}"),
            token,
            pid: std::process::id(),
        },
        control_addr,
        public,
        tunnel,
        control: control_state,
        broker,
        paths,
        cancel,
        tasks,
        _lock: lock,
    })
}

/// Keeps `daemon.json` current with the tunnel/registry state.
fn spawn_state_writer(
    paths: DaemonPaths,
    control_port: u16,
    mut tunnel: tokio::sync::watch::Receiver<TunnelState>,
    control: Arc<control::ControlState>,
    cancel: CancellationToken,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        let started_at_ms = state::now_ms();
        let write = |tunnel_state: &TunnelState, registry: &RegistryStatus| {
            let daemon_state = DaemonState {
                version: state::STATE_VERSION,
                pid: std::process::id(),
                control_port,
                started_at_ms,
                codex_version: DAEMON_VERSION.to_string(),
                public_url: tunnel_state.endpoint().map(TunnelEndpoint::public_label),
                registry_status: registry.label().to_string(),
            };
            if let Err(error) = state::write_json(&paths.state, &daemon_state) {
                tracing::warn!("chatgpt_web daemon: could not write daemon.json: {error}");
            }
        };
        write(&tunnel.borrow().clone(), &control.registry_status());
        loop {
            tokio::select! {
                _ = cancel.cancelled() => break,
                changed = tunnel.changed() => {
                    if changed.is_err() { break; }
                    write(&tunnel.borrow().clone(), &control.registry_status());
                }
                _ = tokio::time::sleep(Duration::from_secs(10)) => {
                    write(&tunnel.borrow().clone(), &control.registry_status());
                }
            }
        }
    })
}

/// Stops the daemon when idle, on request or after `idle_shutdown` of quiet.
fn spawn_idle_watch(
    control: Arc<control::ControlState>,
    idle_shutdown: Option<Duration>,
    cancel: CancellationToken,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        let mut idle_since = tokio::time::Instant::now();
        loop {
            tokio::select! {
                _ = cancel.cancelled() => break,
                _ = tokio::time::sleep(Duration::from_secs(1)) => {}
            }
            let (sessions, turns) = control.broker.stats();
            let idle = sessions == 0 && turns == 0;
            if !idle {
                idle_since = tokio::time::Instant::now();
                continue;
            }
            if control.shutdown_when_idle.load(Ordering::SeqCst) {
                tracing::info!("chatgpt_web daemon: idle after shutdown_when_idle; exiting");
                cancel.cancel();
                break;
            }
            if let Some(limit) = idle_shutdown
                && idle_since.elapsed() >= limit
            {
                tracing::info!("chatgpt_web daemon: idle for {}s; exiting", limit.as_secs());
                cancel.cancel();
                break;
            }
        }
    })
}

/// Runs the daemon until it is told to stop (ctrl-c, `stop`, idle).
pub async fn run(config: DaemonRunConfig) -> anyhow::Result<()> {
    let daemon = start(config).await?;
    tracing::info!(
        "chatgpt_web daemon: control API on {}, public MCP server on {}",
        daemon.control_addr,
        daemon.public.local_addr()
    );
    let cancel = daemon.cancel_token();
    tokio::select! {
        _ = cancel.cancelled() => {}
        _ = tokio::signal::ctrl_c() => {
            tracing::info!("chatgpt_web daemon: ctrl-c");
        }
    }
    daemon.shutdown().await;
    Ok(())
}

/// What `codex chatgpt-web status` prints.
#[derive(Debug, Clone, serde::Serialize)]
pub struct DaemonStatus {
    pub state: Option<DaemonState>,
    pub alive: bool,
    pub health: Option<wire::HealthResponse>,
    pub connector: Option<ConnectorRecord>,
}

async fn fetch_health(control_url: &str) -> Option<wire::HealthResponse> {
    let response = http_client()
        .get(format!("{control_url}/healthz"))
        .timeout(Duration::from_secs(3))
        .send()
        .await
        .ok()?;
    if !response.status().is_success() {
        return None;
    }
    response.json().await.ok()
}

/// Reads `daemon.json` and probes the control API.
pub async fn status(codex_home: &Path) -> DaemonStatus {
    let paths = DaemonPaths::new(codex_home);
    let state: Option<DaemonState> = state::read_json_opt(&paths.state);
    let connector: Option<ConnectorRecord> = state::read_json_opt(&paths.connector);
    let Some(state) = state else {
        return DaemonStatus {
            state: None,
            alive: false,
            health: None,
            connector,
        };
    };
    let alive = state::pid_alive(state.pid);
    let health = if alive {
        fetch_health(&state.control_url()).await
    } else {
        None
    };
    DaemonStatus {
        alive: alive && health.is_some(),
        state: Some(state),
        health,
        connector,
    }
}

fn kill_pid(pid: u32) {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        use windows_sys::Win32::System::Threading::CREATE_NO_WINDOW;
        // FORK: without this the stop path flashes a console window.
        let _ = std::process::Command::new("taskkill")
            .args(["/T", "/F", "/PID", &pid.to_string()])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .creation_flags(CREATE_NO_WINDOW)
            .status();
    }
    #[cfg(not(windows))]
    {
        // SAFETY: signalling a pid we read from our own state file.
        unsafe {
            libc::kill(pid as libc::pid_t, libc::SIGTERM);
        }
    }
}

/// Asks the daemon to exit when idle, then kills it if it lingers.
/// Returns `false` when no daemon was running.
pub async fn stop(codex_home: &Path) -> anyhow::Result<bool> {
    let paths = DaemonPaths::new(codex_home);
    let Some(state) = state::read_json_opt::<DaemonState>(&paths.state) else {
        return Ok(false);
    };
    if !state::pid_alive(state.pid) {
        let _ = std::fs::remove_file(&paths.state);
        return Ok(false);
    }
    if let Some(token) = state::read_secret(&paths.token) {
        let _ = http_client()
            .post(format!(
                "{}/v1/admin/shutdown_when_idle",
                state.control_url()
            ))
            .bearer_auth(token)
            .timeout(Duration::from_secs(3))
            .send()
            .await;
    }
    let deadline = tokio::time::Instant::now() + STOP_TIMEOUT;
    while state::pid_alive(state.pid) && tokio::time::Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    if state::pid_alive(state.pid) {
        kill_pid(state.pid);
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    let _ = std::fs::remove_file(&paths.state);
    Ok(true)
}

/// Reads the running daemon's endpoint, if it is alive and answers.
pub async fn running_endpoint(codex_home: &Path) -> Option<DaemonEndpoint> {
    let paths = DaemonPaths::new(codex_home);
    let state: DaemonState = state::read_json_opt(&paths.state)?;
    if !state::pid_alive(state.pid) {
        return None;
    }
    let health = fetch_health(&state.control_url()).await?;
    let token = state::read_secret(&paths.token)?;
    if health.version != DAEMON_VERSION {
        tracing::info!(
            "chatgpt_web daemon: running version {} differs from {DAEMON_VERSION}; asking it to retire",
            health.version
        );
        let _ = http_client()
            .post(format!(
                "{}/v1/admin/shutdown_when_idle",
                state.control_url()
            ))
            .bearer_auth(&token)
            .timeout(Duration::from_secs(3))
            .send()
            .await;
        return None;
    }
    Some(DaemonEndpoint {
        control_url: state.control_url(),
        token,
        pid: state.pid,
    })
}

/// FORK (C5): the `[chatgpt_web]` settings the daemon must share with the
/// session that starts it, as `-c` overrides for the spawned process.
///
/// The daemon reads `config.toml` like any other command, so a session that
/// runs under `-c chatgpt_web.tunnel="cloudflared"` (or any other override)
/// used to autostart a daemon with the *file's* settings — the wrong tunnel,
/// port or connector name. Only the keys the daemon itself consumes are
/// forwarded; secrets never travel here (the key file path does, not the key).
pub fn daemon_overrides(settings: &ChatGptWebSettings) -> Vec<String> {
    fn quote(value: &str) -> String {
        // A JSON string literal is a valid TOML basic string.
        serde_json::to_string(value).unwrap_or_else(|_| String::from("\"\""))
    }
    fn path(value: &Path) -> String {
        quote(&value.to_string_lossy())
    }
    let tunnel = match settings.tunnel {
        ChatGptWebTunnel::Openai => "openai",
        ChatGptWebTunnel::Cloudflared => "cloudflared",
        ChatGptWebTunnel::Manual => "manual",
    };
    let mut overrides = vec![
        format!("chatgpt_web.tunnel={}", quote(tunnel)),
        format!("chatgpt_web.tunnel_port={}", settings.tunnel_port),
        format!("chatgpt_web.daemon_port={}", settings.daemon_port),
        format!(
            "chatgpt_web.daemon_idle_shutdown_ms={}",
            settings.daemon_idle_shutdown_ms
        ),
        format!(
            "chatgpt_web.connector_name={}",
            quote(&settings.connector_name)
        ),
        format!(
            "chatgpt_web.connector_description={}",
            quote(&settings.connector_description)
        ),
        format!("chatgpt_web.daemon_url={}", quote(&settings.daemon_url)),
        format!("chatgpt_web.base_url={}", quote(&settings.base_url)),
        format!(
            "chatgpt_web.tunnel_client_version={}",
            quote(&settings.tunnel_client_version)
        ),
        format!(
            "chatgpt_web.connector_auto_developer_mode={}",
            settings.connector_auto_developer_mode
        ),
        format!(
            "chatgpt_web.connector_call_timeout_ms={}",
            settings.connector_call_timeout.as_millis()
        ),
        format!(
            "chatgpt_web.connector_exec_default_yield_ms={}",
            settings.connector_exec_default_yield.as_millis()
        ),
        format!("chatgpt_web.turn_ttl_ms={}", settings.turn_ttl.as_millis()),
    ];
    if !settings.cloudflared_extra_args.is_empty() {
        let args: Vec<String> = settings
            .cloudflared_extra_args
            .iter()
            .map(|arg| quote(arg))
            .collect();
        overrides.push(format!(
            "chatgpt_web.cloudflared_extra_args=[{}]",
            args.join(",")
        ));
    }
    if let Some(id) = settings.tunnel_id.as_deref() {
        overrides.push(format!("chatgpt_web.tunnel_id={}", quote(id)));
    }
    if let Some(url) = settings.manual_mcp_url.as_deref() {
        overrides.push(format!("chatgpt_web.manual_mcp_url={}", quote(url)));
    }
    if let Some(value) = settings.token_file.as_deref() {
        overrides.push(format!("chatgpt_web.token_file={}", path(value)));
    }
    if let Some(value) = settings.tunnel_key_file.as_deref() {
        overrides.push(format!("chatgpt_web.tunnel_key_file={}", path(value)));
    }
    if let Some(value) = settings.tunnel_client_path.as_deref() {
        overrides.push(format!("chatgpt_web.tunnel_client_path={}", path(value)));
    }
    if let Some(value) = settings.cloudflared_path.as_deref() {
        overrides.push(format!("chatgpt_web.cloudflared_path={}", path(value)));
    }
    overrides
}

/// Spawns `codex chatgpt-web daemon` detached from this process, carrying the
/// given `-c` overrides (see [`daemon_overrides`]).
pub fn spawn_detached(codex_home: &Path, overrides: &[String]) -> anyhow::Result<u32> {
    let exe = std::env::current_exe().context("locating the codex executable")?;
    let mut command = std::process::Command::new(exe);
    for entry in overrides {
        command.arg("-c").arg(entry);
    }
    command
        .args(["chatgpt-web", "daemon"])
        .env("CODEX_HOME", codex_home)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        use windows_sys::Win32::System::Threading::CREATE_NEW_PROCESS_GROUP;
        use windows_sys::Win32::System::Threading::CREATE_NO_WINDOW;
        use windows_sys::Win32::System::Threading::DETACHED_PROCESS;
        command.creation_flags(DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP | CREATE_NO_WINDOW);
        // FORK: `Stdio::null()` only replaces the child's *standard* handles.
        // `CreateProcess` still copies every other inheritable handle, and when
        // this CLI runs under a shell pipe (`codex chatgpt-web setup | tail`,
        // an agent, CI) our own stdout/stderr pipe ends are inheritable — so
        // the daemon kept the pipe open and the caller waited on EOF for as
        // long as the daemon lived. Strip inheritance from our std handles
        // before spawning; they are ours, the child never needs them.
        detach_std_handles_from_inheritance();
    }
    #[cfg(not(windows))]
    {
        use std::os::unix::process::CommandExt;
        command.process_group(0);
    }
    let child = command.spawn().context("spawning the chatgpt-web daemon")?;
    Ok(child.id())
}

/// FORK: marks this process's stdin/stdout/stderr handles non-inheritable so a
/// detached child cannot hold a caller's pipe open (see `spawn_detached`).
#[cfg(windows)]
fn detach_std_handles_from_inheritance() {
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Foundation::HANDLE_FLAG_INHERIT;
    use windows_sys::Win32::Foundation::SetHandleInformation;
    let handles = [
        std::io::stdin().as_raw_handle(),
        std::io::stdout().as_raw_handle(),
        std::io::stderr().as_raw_handle(),
    ];
    for handle in handles {
        if handle.is_null() {
            continue;
        }
        // Best effort: a console handle or an already non-inheritable handle
        // fails or no-ops harmlessly.
        // SAFETY: `handle` is a live standard handle owned by this process.
        unsafe {
            SetHandleInformation(handle as _, HANDLE_FLAG_INHERIT, 0);
        }
    }
}

/// Uses the running daemon or starts one, waiting until it answers.
pub async fn ensure_daemon(
    codex_home: &Path,
    overrides: &[String],
) -> anyhow::Result<DaemonEndpoint> {
    if let Some(endpoint) = running_endpoint(codex_home).await {
        return Ok(endpoint);
    }
    let paths = DaemonPaths::new(codex_home);
    paths.ensure_dir()?;
    // Whoever wins the lock serves; the loser simply exits.
    let pid = spawn_detached(codex_home, overrides)?;
    tracing::info!("chatgpt_web daemon: spawned pid {pid}");
    let deadline = tokio::time::Instant::now() + AUTOSTART_TIMEOUT;
    loop {
        tokio::time::sleep(Duration::from_millis(300)).await;
        if let Some(endpoint) = running_endpoint(codex_home).await {
            return Ok(endpoint);
        }
        if tokio::time::Instant::now() >= deadline {
            return Err(anyhow!(
                "the chatgpt-web daemon did not come up within {}s; run `codex chatgpt-web daemon --foreground` to see why",
                AUTOSTART_TIMEOUT.as_secs()
            ));
        }
    }
}

// ---------------------------------------------------------------------------
// Helpers for the `codex chatgpt-web` CLI (which has no HTTP client of its own).

/// Config edits `codex chatgpt-web setup` persists: the tunnel id and the
/// `openai` transport.
pub fn setup_config_edits(tunnel_id: &str) -> Vec<crate::config::edit::ConfigEdit> {
    vec![
        crate::config::edit::ConfigEdit::SetPath {
            segments: vec!["chatgpt_web".to_string(), "tunnel_id".to_string()],
            value: toml_edit::value(tunnel_id),
        },
        crate::config::edit::ConfigEdit::SetPath {
            segments: vec!["chatgpt_web".to_string(), "tunnel".to_string()],
            value: toml_edit::value("openai"),
        },
    ]
}

/// Validates and stores the tunnel credentials, then updates `config.toml`.
pub fn setup_tunnel(codex_home: &Path, tunnel_id: &str, api_key: &str) -> anyhow::Result<PathBuf> {
    if !tunnel::is_valid_tunnel_id(tunnel_id) {
        anyhow::bail!("tunnel id must look like tunnel_<32 hex chars>, got `{tunnel_id}`");
    }
    let api_key = api_key.trim();
    if api_key.is_empty() {
        anyhow::bail!("the API key file is empty");
    }
    let paths = DaemonPaths::new(codex_home);
    paths.ensure_dir()?;
    state::write_secret(&paths.tunnel_key, api_key).context("writing tunnel.key")?;
    crate::config::edit::apply_blocking(codex_home, &setup_config_edits(tunnel_id))
        .context("updating config.toml")?;
    Ok(paths.tunnel_key)
}

/// `GET <origin>/healthz` of the chrome-mcp daemon named by `daemon_url`.
pub async fn probe_chrome_mcp(daemon_url: &str) -> anyhow::Result<serde_json::Value> {
    let url = url::Url::parse(daemon_url).context("parsing daemon_url")?;
    let origin = format!(
        "{}://{}{}",
        url.scheme(),
        url.host_str().unwrap_or("127.0.0.1"),
        url.port()
            .map(|port| format!(":{port}"))
            .unwrap_or_default()
    );
    let response = http_client()
        .get(format!("{origin}/healthz"))
        .timeout(Duration::from_secs(3))
        .send()
        .await
        .with_context(|| format!("chrome-mcp daemon unreachable at {origin}"))?;
    let status = response.status();
    let body: serde_json::Value = response.json().await.unwrap_or_default();
    if !status.is_success() {
        anyhow::bail!("chrome-mcp healthz answered HTTP {status}");
    }
    Ok(body)
}

/// Asks the running daemon to reconcile the connector registry.
pub async fn reconcile_via_daemon(
    codex_home: &Path,
    settings: &ChatGptWebSettings,
) -> anyhow::Result<serde_json::Value> {
    let endpoint = ensure_daemon(codex_home, &daemon_overrides(settings)).await?;
    let response = http_client()
        .post(format!("{}/v1/registry/reconcile", endpoint.control_url))
        .bearer_auth(&endpoint.token)
        .timeout(Duration::from_secs(120))
        .send()
        .await
        .context("calling the daemon")?;
    let status = response.status();
    let body: serde_json::Value = response.json().await.unwrap_or_default();
    if !status.is_success() {
        anyhow::bail!(
            "reconcile failed (HTTP {status}): {}",
            body["error"].as_str().unwrap_or("unknown error")
        );
    }
    Ok(body)
}

/// Polls the daemon until its tunnel is ready (or fatal / timed out).
pub async fn wait_tunnel_ready(
    codex_home: &Path,
    settings: &ChatGptWebSettings,
    timeout: Duration,
) -> anyhow::Result<wire::HealthResponse> {
    let endpoint = ensure_daemon(codex_home, &daemon_overrides(settings)).await?;
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        if let Some(health) = fetch_health(&endpoint.control_url).await {
            if health.tunnel_state == "ready" {
                return Ok(health);
            }
            if let Some(reason) = health.tunnel_state.strip_prefix("fatal: ") {
                anyhow::bail!("tunnel failed: {reason}");
            }
            if tokio::time::Instant::now() >= deadline {
                anyhow::bail!(
                    "tunnel not ready after {}s (state: {})",
                    timeout.as_secs(),
                    health.tunnel_state
                );
            }
        } else if tokio::time::Instant::now() >= deadline {
            anyhow::bail!("the daemon stopped answering");
        }
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
}

#[cfg(test)]
#[path = "mod_tests.rs"]
mod tests;
