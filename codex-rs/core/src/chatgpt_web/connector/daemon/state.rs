//! FORK: on-disk state of the shared daemon under `CODEX_HOME/chatgpt_web/`.
//!
//! - `daemon.lock`: single-instance lock (exclusive open on Windows, `flock`
//!   elsewhere) held for the daemon's lifetime;
//! - `daemon.json`: where the running daemon can be reached, minus secrets;
//! - `daemon.token`: bearer for the loopback control API (0600);
//! - `connector.json`: the connector the registry created (owned by the
//!   registry, read here for `status`).
//!
//! Every write is `write temp + rename`, so a reader never sees a torn file.

use serde::Deserialize;
use serde::Serialize;
use std::fs::File;
use std::io;
use std::path::Path;
use std::path::PathBuf;
use std::time::SystemTime;
use std::time::UNIX_EPOCH;

pub const STATE_DIR_NAME: &str = "chatgpt_web";
pub const LOCK_FILE_NAME: &str = "daemon.lock";
pub const STATE_FILE_NAME: &str = "daemon.json";
pub const TOKEN_FILE_NAME: &str = "daemon.token";
pub const CONNECTOR_FILE_NAME: &str = "connector.json";
pub const LOG_FILE_NAME: &str = "daemon.log";
pub const TUNNEL_KEY_FILE_NAME: &str = "tunnel.key";
pub const BIN_DIR_NAME: &str = "bin";

/// Schema version of `daemon.json`.
pub const STATE_VERSION: u32 = 1;

#[derive(Debug, Clone)]
pub struct DaemonPaths {
    pub dir: PathBuf,
    pub lock: PathBuf,
    pub state: PathBuf,
    pub token: PathBuf,
    pub connector: PathBuf,
    pub log: PathBuf,
    pub tunnel_key: PathBuf,
    pub bin_dir: PathBuf,
}

impl DaemonPaths {
    pub fn new(codex_home: &Path) -> Self {
        let dir = codex_home.join(STATE_DIR_NAME);
        Self {
            lock: dir.join(LOCK_FILE_NAME),
            state: dir.join(STATE_FILE_NAME),
            token: dir.join(TOKEN_FILE_NAME),
            connector: dir.join(CONNECTOR_FILE_NAME),
            log: dir.join(LOG_FILE_NAME),
            tunnel_key: dir.join(TUNNEL_KEY_FILE_NAME),
            bin_dir: dir.join(BIN_DIR_NAME),
            dir,
        }
    }

    pub fn ensure_dir(&self) -> io::Result<()> {
        std::fs::create_dir_all(&self.dir)
    }
}

/// FORK: why a reconcile failed, and whether waiting can fix it.
///
/// The turn-side gate used to wait out its whole `ready_timeout` for every
/// failure alike. A tunnel the ChatGPT account cannot see, a login that is
/// gone, or a connector that will not converge are not going to resolve in 90
/// seconds of polling — naming them lets the turn fail in a couple of seconds
/// with something the user can act on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum FailureKind {
    /// Retrying may well work (network hiccup, tunnel still coming up).
    #[default]
    Transient,
    /// The ChatGPT API is rate limiting us; wait longer, but do wait.
    RateLimited,
    /// The configured tunnel is not visible to the ChatGPT account in Chrome.
    TunnelNotVisible,
    /// The page has no ChatGPT login.
    LoginRequired,
    /// The connector cannot be brought into the shape we need.
    SetupRequired,
}

impl FailureKind {
    /// Whether waiting is pointless: the user has to change something.
    pub fn is_terminal(self) -> bool {
        match self {
            Self::Transient | Self::RateLimited => false,
            Self::TunnelNotVisible | Self::LoginRequired | Self::SetupRequired => true,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Transient => "transient",
            Self::RateLimited => "rate_limited",
            Self::TunnelNotVisible => "tunnel_not_visible",
            Self::LoginRequired => "login_required",
            Self::SetupRequired => "setup_required",
        }
    }

    /// Parses the label back, for the turn-side client reading `/healthz`.
    pub fn parse(label: &str) -> Option<Self> {
        match label {
            "transient" => Some(Self::Transient),
            "rate_limited" => Some(Self::RateLimited),
            "tunnel_not_visible" => Some(Self::TunnelNotVisible),
            "login_required" => Some(Self::LoginRequired),
            "setup_required" => Some(Self::SetupRequired),
            _ => None,
        }
    }
}

/// Where the registry stands; the daemon reports it on `/healthz` and refuses
/// turns until it is `Verified`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum RegistryStatus {
    #[default]
    Unknown,
    DeveloperModeOff,
    BrowserUnavailable,
    Reconciling,
    Verified {
        connector_id: String,
        link_id: String,
        mcp_url: String,
    },
    Failed {
        reason: String,
        retry_at_ms: u64,
        /// FORK: what kind of failure this is, so a turn can fail fast on the
        /// ones no amount of waiting fixes.
        #[serde(default)]
        kind: FailureKind,
        /// FORK: the watcher has stopped retrying this one (see
        /// `PARK_AFTER_IDENTICAL_TERMINAL_FAILURES`). A turn, a manual
        /// reconcile or a tunnel change wakes it.
        #[serde(default)]
        parked: bool,
    },
    /// The registry is not implemented in this build (C1); C2 replaces it.
    NotImplemented,
}

impl RegistryStatus {
    /// Short label for JSON status fields.
    pub fn label(&self) -> &'static str {
        match self {
            Self::Unknown => "unknown",
            Self::DeveloperModeOff => "developer_mode_off",
            Self::BrowserUnavailable => "browser_unavailable",
            Self::Reconciling => "reconciling",
            Self::Verified { .. } => "verified",
            Self::Failed { .. } => "failed",
            Self::NotImplemented => "not_implemented",
        }
    }
}

/// `daemon.json`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct DaemonState {
    #[serde(default)]
    pub version: u32,
    #[serde(default)]
    pub pid: u32,
    #[serde(default)]
    pub control_port: u16,
    #[serde(default)]
    pub started_at_ms: u64,
    #[serde(default)]
    pub codex_version: String,
    /// Host of the public endpoint (`https://x.trycloudflare.com` or
    /// `tunnel:<id>`), never the secret path.
    #[serde(default)]
    pub public_url: Option<String>,
    #[serde(default)]
    pub registry_status: String,
}

impl DaemonState {
    pub fn control_url(&self) -> String {
        format!("http://127.0.0.1:{}", self.control_port)
    }
}

/// `connector.json`: what the registry created on the ChatGPT side.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct ConnectorRecord {
    #[serde(default)]
    pub connector_id: String,
    #[serde(default)]
    pub link_id: String,
    /// The endpoint the connector points at: `tunnel:<id>` or the public URL.
    #[serde(default)]
    pub mcp_url: String,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub contract_version: u32,
    #[serde(default)]
    pub verified_at_ms: u64,
    #[serde(default)]
    pub actions: Vec<String>,
}

pub fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Reads a JSON file, defaulting when missing or malformed.
pub fn read_json<T: serde::de::DeserializeOwned + Default>(path: &Path) -> T {
    match std::fs::read(path) {
        Ok(raw) => serde_json::from_slice(&raw).unwrap_or_default(),
        Err(_) => T::default(),
    }
}

/// Reads a JSON file, `None` when missing or malformed.
pub fn read_json_opt<T: serde::de::DeserializeOwned>(path: &Path) -> Option<T> {
    let raw = std::fs::read(path).ok()?;
    serde_json::from_slice(&raw).ok()
}

fn temp_path_for(path: &Path) -> PathBuf {
    let mut name = path
        .file_name()
        .map(std::ffi::OsStr::to_os_string)
        .unwrap_or_default();
    name.push(format!(".{}.tmp", std::process::id()));
    path.with_file_name(name)
}

/// Writes `state` as pretty JSON through a temp file and a rename.
pub fn write_json<T: Serialize>(path: &Path, state: &T) -> io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let body = serde_json::to_vec_pretty(state).map_err(io::Error::other)?;
    let temp = temp_path_for(path);
    std::fs::write(&temp, body)?;
    std::fs::rename(&temp, path)
}

/// Writes a secret file readable only by the owner.
pub fn write_secret(path: &Path, contents: &str) -> io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let temp = temp_path_for(path);
    {
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create(true).truncate(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options.open(&temp)?;
        use std::io::Write;
        file.write_all(contents.as_bytes())?;
        file.sync_all()?;
    }
    std::fs::rename(&temp, path)
}

/// Reads a secret file, trimmed; `None` when missing or empty.
pub fn read_secret(path: &Path) -> Option<String> {
    let raw = std::fs::read_to_string(path).ok()?;
    let trimmed = raw.trim();
    (!trimmed.is_empty()).then(|| trimmed.to_string())
}

/// 32 random bytes as base64url.
pub fn new_token() -> String {
    use base64::Engine;
    use rand::RngCore;
    let mut bytes = [0u8; 32];
    rand::rng().fill_bytes(&mut bytes);
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
}

/// Single-instance lock: held as long as the returned file is alive.
pub struct InstanceLock {
    _file: File,
    path: PathBuf,
}

impl std::fmt::Debug for InstanceLock {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("InstanceLock")
            .field("path", &self.path)
            .finish()
    }
}

/// Why the lock could not be taken.
#[derive(Debug)]
pub enum LockError {
    /// Another daemon holds it.
    Held,
    Io(io::Error),
}

impl std::fmt::Display for LockError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Held => f.write_str("another chatgpt-web daemon holds the lock"),
            Self::Io(error) => write!(f, "{error}"),
        }
    }
}

impl std::error::Error for LockError {}

impl InstanceLock {
    /// Tries once; `Err(Held)` when another process has it.
    pub fn try_acquire(path: &Path) -> Result<Self, LockError> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(LockError::Io)?;
        }
        #[cfg(windows)]
        {
            use std::os::windows::fs::OpenOptionsExt;
            // FORK: `share_mode(0)` denies every other open, which is the only
            // exclusive primitive Windows offers without `LockFileEx`; a second
            // daemon fails at open time with a sharing violation.
            match std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .create(true)
                .truncate(false)
                .share_mode(0)
                .open(path)
            {
                Ok(file) => Ok(Self {
                    _file: file,
                    path: path.to_path_buf(),
                }),
                Err(error) if error.raw_os_error() == Some(32) => Err(LockError::Held),
                Err(error) if error.kind() == io::ErrorKind::PermissionDenied => {
                    Err(LockError::Held)
                }
                Err(error) => Err(LockError::Io(error)),
            }
        }
        #[cfg(not(windows))]
        {
            use std::os::unix::io::AsRawFd;
            let file = std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .create(true)
                .truncate(false)
                .open(path)
                .map_err(LockError::Io)?;
            // SAFETY: flock on a valid, owned descriptor.
            let rc = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
            if rc == 0 {
                Ok(Self {
                    _file: file,
                    path: path.to_path_buf(),
                })
            } else {
                let error = io::Error::last_os_error();
                if error.raw_os_error() == Some(libc::EWOULDBLOCK) {
                    Err(LockError::Held)
                } else {
                    Err(LockError::Io(error))
                }
            }
        }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
}

/// Whether a process id is alive (best effort).
pub fn pid_alive(pid: u32) -> bool {
    if pid == 0 {
        return false;
    }
    #[cfg(windows)]
    {
        use windows_sys::Win32::Foundation::CloseHandle;
        use windows_sys::Win32::Foundation::STILL_ACTIVE;
        use windows_sys::Win32::System::Threading::GetExitCodeProcess;
        use windows_sys::Win32::System::Threading::OpenProcess;
        use windows_sys::Win32::System::Threading::PROCESS_QUERY_LIMITED_INFORMATION;
        // SAFETY: plain Win32 calls with a handle we close ourselves.
        unsafe {
            let handle = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
            if handle == 0 {
                // Access denied means it exists but belongs to someone else.
                return io::Error::last_os_error().raw_os_error() == Some(5);
            }
            let mut code: u32 = 0;
            let ok = GetExitCodeProcess(handle, &mut code);
            CloseHandle(handle);
            ok != 0 && code == STILL_ACTIVE as u32
        }
    }
    #[cfg(not(windows))]
    {
        // SAFETY: signal 0 only checks for existence.
        let rc = unsafe { libc::kill(pid as libc::pid_t, 0) };
        rc == 0 || io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
    }
}

#[cfg(test)]
#[path = "state_tests.rs"]
mod tests;
