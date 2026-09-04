//! FORK: how the loopback MCP server becomes reachable by ChatGPT.
//!
//! Two real adapters and one for tests:
//!
//! - `OpenAiTunnel` (default): the official `tunnel-client` keeps an outbound
//!   long-poll to OpenAI's control plane and relays MCP requests to our
//!   loopback URL. Nothing is exposed on the network and the endpoint identity
//!   (`tunnel_id`) is stable, so the connector is created once.
//! - `CloudflaredTunnel` (fallback): a `cloudflared` quick tunnel with a fresh
//!   `*.trycloudflare.com` URL on every start; the registry recreates the
//!   connector whenever the URL changes.
//! - `NoopTunnel`: reports a fixed endpoint immediately (tests, `manual`).
//!
//! Every adapter runs a supervised loop that publishes `TunnelState` through a
//! `watch` channel; the daemon's registry reacts to `Ready` with a new endpoint.

use futures::future::BoxFuture;
use sha2::Digest;
use std::fmt;
use std::path::Path;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::AsyncBufReadExt;
use tokio::io::BufReader;
use tokio::process::Child;
use tokio::process::Command;
use tokio::sync::mpsc;
use tokio::sync::watch;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

/// Release of `tunnel-client` this build was verified against.
pub const PINNED_TUNNEL_CLIENT_VERSION: &str = "0.0.12";

/// SHA-256 of the pinned release archives, by `(version, os, arch)`.
///
/// From chat-on-steroids' `packaging-versions.mjs`; extend when the pin moves.
const PINNED_ARCHIVES: &[(&str, &str, &str, &str)] = &[
    (
        "0.0.12",
        "windows",
        "amd64",
        "2a2804933924e38a502d62b61f0266cb80d56d65744f4c29876b2bf9c1544356",
    ),
    (
        "0.0.12",
        "windows",
        "arm64",
        "65ab54221554481bb1c23b6015b99abe0b7f79b08593f4fb17a9e2e25532281d",
    ),
    (
        "0.0.12",
        "darwin",
        "amd64",
        "33de53aec680faafedc795f8f8268d6861577bddb871cb2d49529c91f88c2009",
    ),
    (
        "0.0.12",
        "darwin",
        "arm64",
        "42fb3138dc9c081d5777cb7e8bd1e041cc48b67c4978dbab3c5167ca1aabca02",
    ),
    (
        "0.0.12",
        "linux",
        "amd64",
        "2bb693bd7b5cd28da7ce09cd9e309529dbb33b7cc9dc0058e62a064688f92c81",
    ),
    (
        "0.0.12",
        "linux",
        "arm64",
        "6813878a3edb82ebebb32fe5a859bc6327a81cce5bc7b635a2313174d26365d6",
    ),
];

/// Largest release archive we are willing to download.
const MAX_ARCHIVE_BYTES: usize = 100 * 1024 * 1024;

/// `tunnel-client run` flags, in one place so a version bump that renames a
/// flag is a one-line fix. Confirm with `tunnel-client run --help`.
pub const TUNNEL_CLIENT_RUN_ARGS: &[&str] = &["run"];
pub const TUNNEL_CLIENT_TUNNEL_ID_FLAG: &str = "--control-plane.tunnel-id";
pub const TUNNEL_CLIENT_HEALTH_LISTEN_FLAG: &str = "--health.listen-addr";
pub const TUNNEL_CLIENT_HEALTH_URL_FILE_FLAG: &str = "--health.url-file";
pub const TUNNEL_CLIENT_LOG_FLAGS: &[&str] = &["--log.format", "json", "--log.level", "info"];
/// Environment-backed configuration (credentials and the secret path never go
/// on the command line).
pub const TUNNEL_CLIENT_API_KEY_ENV: &str = "CONTROL_PLANE_API_KEY";
pub const TUNNEL_CLIENT_MCP_URL_ENV: &str = "MCP_SERVER_URL";
pub const TUNNEL_CLIENT_STARTUP_WAIT_ENV: &str = "MCP_STARTUP_WAIT_TIMEOUT";

const READY_TIMEOUT: Duration = Duration::from_secs(120);
const CLOUDFLARED_URL_TIMEOUT: Duration = Duration::from_secs(45);
const CLOUDFLARED_READY_TIMEOUT: Duration = Duration::from_secs(60);
const WATCH_INTERVAL: Duration = Duration::from_secs(30);
const OFFLINE_RECHECK: Duration = Duration::from_secs(5);
const MAX_BACKOFF: Duration = Duration::from_secs(60);
const KILL_GRACE: Duration = Duration::from_secs(3);

/// What the registry needs to build the connector.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TunnelEndpoint {
    OpenAi { tunnel_id: String },
    Public { mcp_url: String },
}

impl TunnelEndpoint {
    /// Host-only description for `daemon.json`/`healthz` (no secret path).
    pub fn public_label(&self) -> String {
        match self {
            Self::OpenAi { tunnel_id } => format!("tunnel:{tunnel_id}"),
            Self::Public { mcp_url } => url::Url::parse(mcp_url)
                .ok()
                .and_then(|url| {
                    url.host_str()
                        .map(|host| format!("{}://{host}", url.scheme()))
                })
                .unwrap_or_else(|| "public".to_string()),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TunnelState {
    Connecting,
    Ready {
        endpoint: TunnelEndpoint,
    },
    Down {
        reason: String,
    },
    /// Not retried: wrong key or tunnel id, missing binary.
    Fatal {
        reason: String,
    },
}

impl TunnelState {
    pub fn label(&self) -> String {
        match self {
            Self::Connecting => "connecting".to_string(),
            Self::Ready { .. } => "ready".to_string(),
            Self::Down { reason } => format!("down: {reason}"),
            Self::Fatal { reason } => format!("fatal: {reason}"),
        }
    }

    pub fn endpoint(&self) -> Option<&TunnelEndpoint> {
        match self {
            Self::Ready { endpoint } => Some(endpoint),
            _ => None,
        }
    }
}

/// Everything an adapter's loop needs.
pub struct TunnelContext {
    /// `http://127.0.0.1:<port>/mcp/<secret>`.
    pub local_mcp_url: String,
    pub state: watch::Sender<TunnelState>,
    pub cancel: CancellationToken,
}

impl TunnelContext {
    fn publish(&self, state: TunnelState) {
        if *self.state.borrow() != state {
            tracing::info!("chatgpt_web tunnel: {}", state.label());
        }
        let _ = self.state.send(state);
    }

    fn local_port(&self) -> u16 {
        url::Url::parse(&self.local_mcp_url)
            .ok()
            .and_then(|url| url.port())
            .unwrap_or(0)
    }

    fn mcp_path(&self) -> String {
        url::Url::parse(&self.local_mcp_url)
            .map(|url| url.path().to_string())
            .unwrap_or_else(|_| "/mcp".to_string())
    }
}

pub trait TunnelAdapter: Send + Sync + fmt::Debug {
    /// Runs until `ctx.cancel` fires, publishing state changes as they happen.
    fn run(&self, ctx: TunnelContext) -> BoxFuture<'static, ()>;
}

/// A running tunnel loop.
pub struct TunnelHandle {
    state: watch::Receiver<TunnelState>,
    cancel: CancellationToken,
    task: Option<JoinHandle<()>>,
}

impl fmt::Debug for TunnelHandle {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TunnelHandle")
            .field("state", &*self.state.borrow())
            .finish_non_exhaustive()
    }
}

impl TunnelHandle {
    pub fn state(&self) -> watch::Receiver<TunnelState> {
        self.state.clone()
    }

    pub fn current(&self) -> TunnelState {
        self.state.borrow().clone()
    }

    /// Waits for `Ready`; `Err` carries the fatal reason or a timeout note.
    pub async fn wait_ready(&self, timeout: Duration) -> Result<TunnelEndpoint, String> {
        let mut rx = self.state.clone();
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            match &*rx.borrow() {
                TunnelState::Ready { endpoint } => return Ok(endpoint.clone()),
                TunnelState::Fatal { reason } => return Err(reason.clone()),
                _ => {}
            }
            match tokio::time::timeout_at(deadline, rx.changed()).await {
                Ok(Ok(())) => continue,
                Ok(Err(_)) => return Err("tunnel loop ended".to_string()),
                Err(_) => {
                    return Err(format!(
                        "tunnel not ready after {}s ({})",
                        timeout.as_secs(),
                        rx.borrow().label()
                    ));
                }
            }
        }
    }

    pub async fn shutdown(mut self) {
        self.cancel.cancel();
        if let Some(task) = self.task.take() {
            let _ = tokio::time::timeout(Duration::from_secs(10), task).await;
        }
    }
}

impl Drop for TunnelHandle {
    fn drop(&mut self) {
        self.cancel.cancel();
    }
}

/// Starts an adapter's loop for the given loopback MCP URL.
pub fn start(adapter: Arc<dyn TunnelAdapter>, local_mcp_url: String) -> TunnelHandle {
    let (tx, rx) = watch::channel(TunnelState::Connecting);
    let cancel = CancellationToken::new();
    let ctx = TunnelContext {
        local_mcp_url,
        state: tx,
        cancel: cancel.clone(),
    };
    let task = tokio::spawn(adapter.run(ctx));
    TunnelHandle {
        state: rx,
        cancel,
        task: Some(task),
    }
}

/// Reports a fixed endpoint at once. Used by tests and by `tunnel = "manual"`.
#[derive(Debug, Clone)]
pub struct NoopTunnel {
    pub endpoint: TunnelEndpoint,
}

impl TunnelAdapter for NoopTunnel {
    fn run(&self, ctx: TunnelContext) -> BoxFuture<'static, ()> {
        let endpoint = self.endpoint.clone();
        Box::pin(async move {
            ctx.publish(TunnelState::Ready { endpoint });
            ctx.cancel.cancelled().await;
        })
    }
}

/// Publishes `Fatal` and idles; used when the configured tunnel cannot start.
#[derive(Debug, Clone)]
pub struct FatalTunnel {
    pub reason: String,
}

impl TunnelAdapter for FatalTunnel {
    fn run(&self, ctx: TunnelContext) -> BoxFuture<'static, ()> {
        let reason = self.reason.clone();
        Box::pin(async move {
            ctx.publish(TunnelState::Fatal { reason });
            ctx.cancel.cancelled().await;
        })
    }
}

// ---------------------------------------------------------------------------
// Child process management

/// A child whose whole tree dies with it.
struct ManagedChild {
    child: Child,
    #[cfg(windows)]
    job: Option<codex_utils_pty::JobObject>,
}

impl ManagedChild {
    fn spawn(mut command: Command) -> std::io::Result<Self> {
        command
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        #[cfg(windows)]
        {
            use windows_sys::Win32::System::Threading::CREATE_NO_WINDOW;
            // FORK: the job object's suspended spawn overwrites the creation
            // flags, so `CREATE_NO_WINDOW` has to be handed to it rather than
            // set on the command; otherwise the tunnel child (tunnel-client or
            // cloudflared) comes up with a visible empty console.
            command.creation_flags(CREATE_NO_WINDOW);
            match codex_utils_pty::JobObject::create_without_breakaway() {
                Ok(job) => {
                    let child = job.spawn_contained_with_flags(&mut command, CREATE_NO_WINDOW)?;
                    Ok(Self {
                        child,
                        job: Some(job),
                    })
                }
                Err(error) => {
                    tracing::warn!("chatgpt_web tunnel: job object unavailable: {error}");
                    Ok(Self {
                        child: command.spawn()?,
                        job: None,
                    })
                }
            }
        }
        #[cfg(not(windows))]
        {
            command.process_group(0);
            Ok(Self {
                child: command.spawn()?,
            })
        }
    }

    /// Merged stdout+stderr lines.
    fn lines(&mut self) -> mpsc::Receiver<String> {
        let (tx, rx) = mpsc::channel(256);
        if let Some(stdout) = self.child.stdout.take() {
            let tx = tx.clone();
            tokio::spawn(async move {
                let mut lines = BufReader::new(stdout).lines();
                while let Ok(Some(line)) = lines.next_line().await {
                    if tx.send(line).await.is_err() {
                        break;
                    }
                }
            });
        }
        if let Some(stderr) = self.child.stderr.take() {
            tokio::spawn(async move {
                let mut lines = BufReader::new(stderr).lines();
                while let Ok(Some(line)) = lines.next_line().await {
                    if tx.send(line).await.is_err() {
                        break;
                    }
                }
            });
        }
        rx
    }

    async fn kill_tree(&mut self) {
        #[cfg(windows)]
        {
            if let Some(job) = &self.job
                && let Err(error) = job.terminate()
            {
                tracing::debug!("chatgpt_web tunnel: job terminate failed: {error}");
            }
            let _ = self.child.start_kill();
            if tokio::time::timeout(KILL_GRACE, self.child.wait())
                .await
                .is_err()
                && let Some(pid) = self.child.id()
            {
                let mut taskkill = Command::new("taskkill");
                taskkill
                    .args(["/T", "/F", "/PID", &pid.to_string()])
                    .stdout(Stdio::null())
                    .stderr(Stdio::null());
                #[cfg(windows)]
                taskkill
                    .creation_flags(windows_sys::Win32::System::Threading::CREATE_NO_WINDOW);
                let _ = taskkill.status().await;
                let _ = self.child.wait().await;
            }
        }
        #[cfg(not(windows))]
        {
            if let Some(pid) = self.child.id() {
                // SAFETY: signalling our own child's process group.
                unsafe {
                    libc::killpg(pid as libc::pid_t, libc::SIGTERM);
                }
            }
            if tokio::time::timeout(KILL_GRACE, self.child.wait())
                .await
                .is_err()
            {
                if let Some(pid) = self.child.id() {
                    // SAFETY: as above.
                    unsafe {
                        libc::killpg(pid as libc::pid_t, libc::SIGKILL);
                    }
                }
                let _ = self.child.start_kill();
                let _ = self.child.wait().await;
            }
        }
    }
}

fn backoff(attempt: u32) -> Duration {
    let secs = 2u64.saturating_mul(1u64 << attempt.min(6));
    Duration::from_secs(secs).min(MAX_BACKOFF)
}

/// Outcome of one public probe, distinguishing "no server answered" (DNS,
/// connect, TLS) from "a server answered, but not 2xx".
enum Probe {
    Ok,
    HttpError(u16),
    Unreachable(String),
}

async fn probe_public(http: &reqwest::Client, url: &str) -> Probe {
    match http.get(url).timeout(Duration::from_secs(5)).send().await {
        Ok(response) if response.status().is_success() => Probe::Ok,
        Ok(response) => Probe::HttpError(response.status().as_u16()),
        Err(error) => Probe::Unreachable(error.to_string()),
    }
}

/// cloudflared's "I am connected to the edge" line.
pub fn is_registered_line(line: &str) -> bool {
    line.contains("Registered tunnel connection")
}

async fn probe_ok(http: &reqwest::Client, url: &str) -> bool {
    matches!(
        http.get(url)
            .timeout(Duration::from_secs(5))
            .send()
            .await,
        Ok(response) if response.status().is_success()
    )
}

// ---------------------------------------------------------------------------
// cloudflared

#[derive(Debug, Clone)]
pub struct CloudflaredTunnel {
    pub binary: PathBuf,
    pub extra_args: Vec<String>,
    pub http: reqwest::Client,
}

/// The public URL cloudflared prints once the quick tunnel is up.
pub fn parse_trycloudflare_url(line: &str) -> Option<String> {
    let pattern = regex_lite::Regex::new(r"https://[a-z0-9-]+\.trycloudflare\.com").ok()?;
    pattern.find(line).map(|m| m.as_str().to_string())
}

impl TunnelAdapter for CloudflaredTunnel {
    fn run(&self, ctx: TunnelContext) -> BoxFuture<'static, ()> {
        let this = self.clone();
        Box::pin(async move { this.supervise(ctx).await })
    }
}

impl CloudflaredTunnel {
    async fn supervise(self, ctx: TunnelContext) {
        let mut attempt: u32 = 0;
        while !ctx.cancel.is_cancelled() {
            ctx.publish(TunnelState::Connecting);
            match self.run_once(&ctx).await {
                Ok(()) => attempt = 0,
                Err(reason) => {
                    ctx.publish(TunnelState::Down {
                        reason: reason.clone(),
                    });
                    attempt = attempt.saturating_add(1);
                }
            }
            if ctx.cancel.is_cancelled() {
                break;
            }
            let delay = backoff(attempt);
            tokio::select! {
                _ = ctx.cancel.cancelled() => break,
                _ = tokio::time::sleep(delay) => {}
            }
        }
    }

    /// One cloudflared lifetime: start, wait for the URL, verify, then watch
    /// until it dies or goes unhealthy. `Ok` means it ran for a while.
    async fn run_once(&self, ctx: &TunnelContext) -> Result<(), String> {
        let port = ctx.local_port();
        let mut command = Command::new(&self.binary);
        command
            .arg("tunnel")
            .arg("--no-autoupdate")
            .arg("--url")
            .arg(format!("http://127.0.0.1:{port}"))
            .arg("--http-host-header")
            .arg(format!("127.0.0.1:{port}"))
            .args(&self.extra_args);
        let mut child = ManagedChild::spawn(command)
            .map_err(|error| format!("could not start cloudflared: {error}"))?;
        let mut lines = child.lines();

        let mut url_registered = false;
        let url = {
            let deadline = tokio::time::Instant::now() + CLOUDFLARED_URL_TIMEOUT;
            let mut found = None;
            loop {
                tokio::select! {
                    _ = ctx.cancel.cancelled() => break,
                    line = lines.recv() => match line {
                        Some(line) => {
                            if is_registered_line(&line) {
                                url_registered = true;
                            }
                            if let Some(url) = parse_trycloudflare_url(&line) {
                                found = Some(url);
                                break;
                            }
                        }
                        None => break,
                    },
                    _ = tokio::time::sleep_until(deadline) => break,
                }
            }
            found
        };
        let Some(public_base) = url else {
            child.kill_tree().await;
            return Err("cloudflared did not print a trycloudflare.com URL".to_string());
        };
        let mcp_path = ctx.mcp_path();
        let public_mcp_url = format!("{public_base}{mcp_path}");
        let public_health = format!("{public_mcp_url}/healthz");
        // FORK (C5): readiness. The public healthz is the strongest proof, but
        // it is probed from *this* machine, and a fresh `*.trycloudflare.com`
        // name is routinely NXDOMAIN on local resolvers for a while (negative
        // caching) even though ChatGPT — which resolves through Cloudflare —
        // reaches it fine (verified live: the local resolver never answered
        // within 90 s while DoH did after 20 s). So: a registered tunnel
        // connection plus a probe that fails *before* any HTTP answer counts as
        // ready-but-unverified; the connector registry (ChatGPT connects at
        // create time and returns 424 otherwise) is the effective check. A
        // probe that reaches a server but gets a non-2xx keeps the tunnel down.
        let mut registered = url_registered;
        let mut verified_locally = false;
        let mut saw_http_answer = false;
        {
            let deadline = tokio::time::Instant::now() + CLOUDFLARED_READY_TIMEOUT;
            loop {
                match probe_public(&self.http, &public_health).await {
                    Probe::Ok => {
                        verified_locally = true;
                        break;
                    }
                    Probe::HttpError(status) => {
                        tracing::debug!(
                            "chatgpt_web tunnel: public healthz answered HTTP {status}"
                        );
                        saw_http_answer = true;
                    }
                    Probe::Unreachable(reason) => {
                        tracing::debug!("chatgpt_web tunnel: public healthz unreachable: {reason}");
                    }
                }
                if tokio::time::Instant::now() >= deadline {
                    break;
                }
                tokio::select! {
                    _ = ctx.cancel.cancelled() => break,
                    line = lines.recv() => match line {
                        Some(line) => {
                            if is_registered_line(&line) {
                                registered = true;
                            }
                        }
                        None => break,
                    },
                    _ = tokio::time::sleep(Duration::from_secs(1)) => {}
                }
            }
        }
        if ctx.cancel.is_cancelled() {
            child.kill_tree().await;
            return Ok(());
        }
        if !verified_locally {
            if registered && !saw_http_answer {
                // The secret path never reaches the log: host only.
                tracing::warn!(
                    "chatgpt_web tunnel: cloudflared registered its connection but {public_base} is unreachable from this machine (local DNS?); trusting the tunnel and leaving verification to the connector registry"
                );
            } else {
                child.kill_tree().await;
                return Err(if saw_http_answer {
                    "cloudflared URL answered healthz with an error".to_string()
                } else {
                    "cloudflared URL never answered healthz".to_string()
                });
            }
        }
        ctx.publish(TunnelState::Ready {
            endpoint: TunnelEndpoint::Public {
                mcp_url: public_mcp_url,
            },
        });

        // Keep the log drained and watch health until something gives. When
        // the public probe never worked from here, only the process and its
        // own log are watched; a later successful probe upgrades the watch.
        let ran_for = tokio::time::Instant::now();
        let outcome = loop {
            tokio::select! {
                _ = ctx.cancel.cancelled() => break Ok(()),
                line = lines.recv() => {
                    if line.is_none() {
                        break Err("cloudflared exited".to_string());
                    }
                }
                _ = tokio::time::sleep(WATCH_INTERVAL) => {
                    match probe_public(&self.http, &public_health).await {
                        Probe::Ok => verified_locally = true,
                        Probe::HttpError(_) | Probe::Unreachable(_)
                            if verified_locally && !probe_ok(&self.http, &public_health).await =>
                        {
                            break Err("cloudflared URL stopped answering".to_string());
                        }
                        _ => {}
                    }
                }
            }
        };
        child.kill_tree().await;
        match outcome {
            Ok(()) => Ok(()),
            // A tunnel that lived a while does not count against the backoff.
            Err(reason) if ran_for.elapsed() > Duration::from_secs(120) => {
                tracing::warn!("chatgpt_web tunnel: {reason}; restarting");
                Ok(())
            }
            Err(reason) => Err(reason),
        }
    }
}

// ---------------------------------------------------------------------------
// OpenAI Secure MCP Tunnel (tunnel-client)

#[derive(Clone)]
pub struct OpenAiTunnel {
    pub binary: PathBuf,
    pub tunnel_id: String,
    /// The restricted API key. Only ever placed in the child's environment.
    pub api_key: String,
    pub http: reqwest::Client,
}

impl fmt::Debug for OpenAiTunnel {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("OpenAiTunnel")
            .field("binary", &self.binary)
            .field("tunnel_id", &self.tunnel_id)
            .finish_non_exhaustive()
    }
}

/// `tunnel_<32 hex>`.
pub fn is_valid_tunnel_id(candidate: &str) -> bool {
    candidate
        .strip_prefix("tunnel_")
        .is_some_and(|hex| hex.len() == 32 && hex.bytes().all(|b| b.is_ascii_hexdigit()))
}

/// Log lines that mean the key or tunnel id is wrong (terminal).
pub fn is_auth_failure(line: &str) -> bool {
    static PATTERN: std::sync::OnceLock<Option<regex_lite::Regex>> = std::sync::OnceLock::new();
    PATTERN
        .get_or_init(|| {
            regex_lite::Regex::new(
                r"(?i)\b(401|403|unauthorized|invalid[_ ]api[_ ]key|invalid_request_error|forbidden)\b",
            )
            .ok()
        })
        .as_ref()
        .is_some_and(|pattern| pattern.is_match(line))
}

/// Log lines that mean this machine cannot reach OpenAI right now (retry).
pub fn is_unreachable(line: &str) -> bool {
    static PATTERN: std::sync::OnceLock<Option<regex_lite::Regex>> = std::sync::OnceLock::new();
    PATTERN
        .get_or_init(|| {
            regex_lite::Regex::new(
                r"(?i)poll failed|no such host|dial tcp|i/o timeout|connection (was )?(aborted|refused|reset)|network is (unreachable|down)|no route to host|tls handshake timeout|temporary failure in name resolution|forcibly closed",
            )
            .ok()
        })
        .as_ref()
        .is_some_and(|pattern| pattern.is_match(line))
}

impl TunnelAdapter for OpenAiTunnel {
    fn run(&self, ctx: TunnelContext) -> BoxFuture<'static, ()> {
        let this = self.clone();
        Box::pin(async move { this.supervise(ctx).await })
    }
}

impl OpenAiTunnel {
    async fn supervise(self, ctx: TunnelContext) {
        if !is_valid_tunnel_id(&self.tunnel_id) {
            ctx.publish(TunnelState::Fatal {
                reason:
                    "tunnel_id must look like tunnel_<32 hex chars>; run `codex chatgpt-web setup`"
                        .to_string(),
            });
            ctx.cancel.cancelled().await;
            return;
        }
        let mut attempt: u32 = 0;
        while !ctx.cancel.is_cancelled() {
            ctx.publish(TunnelState::Connecting);
            match self.run_once(&ctx).await {
                Ok(()) => attempt = 0,
                Err(RunFailure::Fatal(reason)) => {
                    ctx.publish(TunnelState::Fatal { reason });
                    ctx.cancel.cancelled().await;
                    return;
                }
                Err(RunFailure::Retry(reason)) => {
                    ctx.publish(TunnelState::Down { reason });
                    attempt = attempt.saturating_add(1);
                }
            }
            if ctx.cancel.is_cancelled() {
                break;
            }
            tokio::select! {
                _ = ctx.cancel.cancelled() => break,
                _ = tokio::time::sleep(backoff(attempt)) => {}
            }
        }
    }

    async fn run_once(&self, ctx: &TunnelContext) -> Result<(), RunFailure> {
        let work_dir = tempfile::Builder::new()
            .prefix("codex-tunnel-")
            .tempdir()
            .map_err(|error| RunFailure::Retry(format!("temp dir: {error}")))?;
        let health_file = work_dir.path().join("health.url");

        let mut command = Command::new(&self.binary);
        command
            .args(TUNNEL_CLIENT_RUN_ARGS)
            .arg(TUNNEL_CLIENT_TUNNEL_ID_FLAG)
            .arg(&self.tunnel_id)
            .arg(TUNNEL_CLIENT_HEALTH_LISTEN_FLAG)
            .arg("127.0.0.1:0")
            .arg(TUNNEL_CLIENT_HEALTH_URL_FILE_FLAG)
            .arg(&health_file)
            .args(TUNNEL_CLIENT_LOG_FLAGS)
            .env(TUNNEL_CLIENT_API_KEY_ENV, &self.api_key)
            .env(
                TUNNEL_CLIENT_MCP_URL_ENV,
                format!("url={},channel=main", ctx.local_mcp_url),
            )
            .env(TUNNEL_CLIENT_STARTUP_WAIT_ENV, "60s");
        let mut child = ManagedChild::spawn(command).map_err(|error| {
            RunFailure::Fatal(format!("could not start tunnel-client: {error}"))
        })?;
        let mut lines = child.lines();

        // Phase 1: wait for the health URL file while scanning the log.
        let deadline = tokio::time::Instant::now() + READY_TIMEOUT;
        let mut health_base: Option<String> = None;
        let mut last_unreachable: Option<String> = None;
        loop {
            if health_base.is_none()
                && let Ok(raw) = std::fs::read_to_string(&health_file)
            {
                let trimmed = raw.trim().trim_end_matches('/').to_string();
                if !trimmed.is_empty() {
                    health_base = Some(trimmed);
                }
            }
            if let Some(base) = &health_base {
                let ready_url = format!("{base}/readyz");
                if probe_ok(&self.http, &ready_url).await {
                    break;
                }
            }
            if tokio::time::Instant::now() >= deadline {
                child.kill_tree().await;
                return Err(RunFailure::Retry(match last_unreachable {
                    Some(reason) => format!("control plane unreachable: {reason}"),
                    None => "tunnel-client did not become ready in time".to_string(),
                }));
            }
            tokio::select! {
                _ = ctx.cancel.cancelled() => { child.kill_tree().await; return Ok(()); }
                line = lines.recv() => match line {
                    Some(line) => {
                        if is_auth_failure(&line) {
                            child.kill_tree().await;
                            return Err(RunFailure::Fatal(
                                "OpenAI rejected the tunnel API key or tunnel id; run `codex chatgpt-web setup` again".to_string(),
                            ));
                        }
                        if is_unreachable(&line) {
                            last_unreachable = Some(trim_log_line(&line));
                        }
                    }
                    None => {
                        child.kill_tree().await;
                        return Err(RunFailure::Retry("tunnel-client exited during startup".to_string()));
                    }
                },
                _ = tokio::time::sleep(Duration::from_millis(500)) => {}
            }
        }
        let health_base = health_base.unwrap_or_default();
        ctx.publish(TunnelState::Ready {
            endpoint: TunnelEndpoint::OpenAi {
                tunnel_id: self.tunnel_id.clone(),
            },
        });

        // Phase 2: watch. Unreachable lines flip to Down without restarting the
        // client (it retries by itself); readyz going red restarts it.
        let started = tokio::time::Instant::now();
        let ready_url = format!("{health_base}/readyz");
        let mut offline = false;
        let outcome = loop {
            let interval = if offline {
                OFFLINE_RECHECK
            } else {
                WATCH_INTERVAL
            };
            tokio::select! {
                _ = ctx.cancel.cancelled() => break Ok(()),
                line = lines.recv() => match line {
                    Some(line) => {
                        if is_auth_failure(&line) {
                            break Err(RunFailure::Fatal(
                                "OpenAI rejected the tunnel API key or tunnel id; run `codex chatgpt-web setup` again".to_string(),
                            ));
                        }
                        if is_unreachable(&line) && !offline {
                            offline = true;
                            ctx.publish(TunnelState::Down { reason: format!("control plane unreachable: {}", trim_log_line(&line)) });
                        }
                    }
                    None => break Err(RunFailure::Retry("tunnel-client exited".to_string())),
                },
                _ = tokio::time::sleep(interval) => {
                    if probe_ok(&self.http, &ready_url).await {
                        if offline {
                            offline = false;
                            ctx.publish(TunnelState::Ready {
                                endpoint: TunnelEndpoint::OpenAi { tunnel_id: self.tunnel_id.clone() },
                            });
                        }
                    } else if !probe_ok(&self.http, &ready_url).await {
                        break Err(RunFailure::Retry("tunnel-client readyz stopped answering".to_string()));
                    }
                }
            }
        };
        child.kill_tree().await;
        match outcome {
            Err(RunFailure::Retry(reason)) if started.elapsed() > Duration::from_secs(120) => {
                tracing::warn!("chatgpt_web tunnel: {reason}; restarting");
                Ok(())
            }
            other => other,
        }
    }
}

enum RunFailure {
    Retry(String),
    Fatal(String),
}

fn trim_log_line(line: &str) -> String {
    let trimmed = line.trim();
    if trimmed.len() > 160 {
        format!("{}…", &trimmed[..160])
    } else {
        trimmed.to_string()
    }
}

// ---------------------------------------------------------------------------
// Binary resolution

fn exe_name(base: &str) -> String {
    if cfg!(windows) {
        format!("{base}.exe")
    } else {
        base.to_string()
    }
}

fn first_existing(candidates: impl IntoIterator<Item = PathBuf>) -> Option<PathBuf> {
    candidates.into_iter().find(|path| path.is_file())
}

/// `cloudflared`: explicit path → PATH → well-known install locations.
pub fn resolve_cloudflared(explicit: Option<&Path>) -> Option<PathBuf> {
    if let Some(path) = explicit {
        return path.is_file().then(|| path.to_path_buf());
    }
    if let Ok(path) = which::which("cloudflared") {
        return Some(path);
    }
    let mut candidates = Vec::new();
    #[cfg(windows)]
    {
        for root in ["ProgramFiles(x86)", "ProgramFiles"] {
            if let Ok(dir) = std::env::var(root) {
                candidates.push(
                    PathBuf::from(dir)
                        .join("cloudflared")
                        .join("cloudflared.exe"),
                );
            }
        }
        candidates.push(PathBuf::from(
            r"C:\Program Files (x86)\cloudflared\cloudflared.exe",
        ));
    }
    candidates.extend(
        ["/opt/homebrew/bin", "/usr/local/bin", "/usr/bin"]
            .iter()
            .map(|dir| PathBuf::from(dir).join("cloudflared")),
    );
    first_existing(candidates)
}

/// Where the pinned download lands.
pub fn managed_tunnel_client_path(bin_dir: &Path, version: &str) -> PathBuf {
    bin_dir.join(exe_name(&format!("tunnel-client-v{version}")))
}

/// `tunnel-client`: explicit path → managed download → PATH → homebrew.
pub fn resolve_tunnel_client(
    explicit: Option<&Path>,
    bin_dir: &Path,
    version: &str,
) -> Option<PathBuf> {
    if let Some(path) = explicit {
        return path.is_file().then(|| path.to_path_buf());
    }
    let managed = managed_tunnel_client_path(bin_dir, version);
    if managed.is_file() {
        return Some(managed);
    }
    if let Ok(path) = which::which("tunnel-client") {
        return Some(path);
    }
    first_existing(
        ["/opt/homebrew/bin", "/usr/local/bin"]
            .iter()
            .map(|dir| PathBuf::from(dir).join("tunnel-client")),
    )
}

fn release_os() -> &'static str {
    if cfg!(target_os = "windows") {
        "windows"
    } else if cfg!(target_os = "macos") {
        "darwin"
    } else {
        "linux"
    }
}

fn release_arch() -> &'static str {
    if cfg!(target_arch = "aarch64") {
        "arm64"
    } else {
        "amd64"
    }
}

/// Release asset name for this platform.
pub fn release_asset_name(version: &str) -> String {
    format!(
        "tunnel-client-v{version}-{}-{}.zip",
        release_os(),
        release_arch()
    )
}

/// Pinned archive hash for this platform, when the version is pinned.
pub fn pinned_archive_sha256(version: &str) -> Option<&'static str> {
    let arch = release_arch();
    PINNED_ARCHIVES
        .iter()
        .find(|(v, os, a, _)| *v == version && *os == release_os() && *a == arch)
        .map(|(_, _, _, sha)| *sha)
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
struct DownloadManifest {
    version: String,
    asset: String,
    archive_sha256: String,
    binary_sha256: String,
}

fn hex_sha256(bytes: &[u8]) -> String {
    sha2::Sha256::digest(bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// Downloads and verifies the pinned `tunnel-client` release into `bin_dir`.
///
/// Refuses versions without a pinned hash rather than trusting the network.
pub async fn ensure_tunnel_client(
    http: &reqwest::Client,
    bin_dir: &Path,
    version: &str,
) -> anyhow::Result<PathBuf> {
    let target = managed_tunnel_client_path(bin_dir, version);
    if target.is_file() {
        return Ok(target);
    }
    let Some(expected) = pinned_archive_sha256(version) else {
        anyhow::bail!(
            "no pinned checksum for tunnel-client {version} on {}-{}; install it yourself and set `[chatgpt_web] tunnel_client_path`",
            release_os(),
            release_arch()
        );
    };
    let asset = release_asset_name(version);
    let url =
        format!("https://github.com/openai/tunnel-client/releases/download/v{version}/{asset}");
    tracing::info!("chatgpt_web: downloading {url}");
    let response = http
        .get(&url)
        .header("user-agent", "codex-chatgpt-web")
        .send()
        .await?
        .error_for_status()?;
    let bytes = response.bytes().await?;
    if bytes.len() > MAX_ARCHIVE_BYTES {
        anyhow::bail!("{asset} is larger than the {MAX_ARCHIVE_BYTES}-byte cap");
    }
    let actual = hex_sha256(&bytes);
    if actual != expected {
        anyhow::bail!("checksum mismatch for {asset}: expected {expected}, got {actual}");
    }
    let wanted = exe_name("tunnel-client");
    let binary = tokio::task::spawn_blocking(move || -> anyhow::Result<Vec<u8>> {
        let mut archive = zip::ZipArchive::new(std::io::Cursor::new(bytes))?;
        for index in 0..archive.len() {
            let mut entry = archive.by_index(index)?;
            let name = entry
                .enclosed_name()
                .and_then(|path| path.file_name().map(|n| n.to_string_lossy().to_string()));
            if name.as_deref() == Some(wanted.as_str()) {
                let mut out = Vec::new();
                std::io::copy(&mut entry, &mut out)?;
                return Ok(out);
            }
        }
        anyhow::bail!("archive does not contain {wanted}")
    })
    .await??;
    std::fs::create_dir_all(bin_dir)?;
    let temp = target.with_extension("part");
    std::fs::write(&temp, &binary)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&temp, std::fs::Permissions::from_mode(0o755))?;
    }
    std::fs::rename(&temp, &target)?;
    let manifest = DownloadManifest {
        version: version.to_string(),
        asset,
        archive_sha256: actual,
        binary_sha256: hex_sha256(&binary),
    };
    let manifest_path = bin_dir.join(format!("tunnel-client-v{version}.json"));
    std::fs::write(&manifest_path, serde_json::to_vec_pretty(&manifest)?)?;
    Ok(target)
}

#[cfg(test)]
#[path = "tunnel_tests.rs"]
mod tests;
