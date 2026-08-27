//! FORK: port of `chatgpt-pro-mcp/src/tab.ts` — the pool of dedicated ChatGPT
//! tabs THIS process owns, plus the cross-process ownership registry.
//!
//! The registry (`~/.chatgpt-pro-mcp/tabs.json`, `{"owners":[{"tabId","pid",
//! "since"}]}`) uses exactly the Node format and the same `mkdir tabs.json.lock`
//! mutex, so a Node `chatgpt-pro-mcp` running next to this process never types
//! into our tabs and we adopt tabs whose owner died instead of leaking windows.
//!
//! Inside the process, sends are queued PER TAB with conversation affinity:
//! concurrent sends to different conversations run in parallel on different
//! tabs (the pool grows up to `max_tabs`), while sends to the same conversation
//! serialize on its bound tab.

// TODO(M3): the provider does not consume the pool yet; drop once `ops` wires
// it in.
#![allow(dead_code)]

use std::collections::HashSet;
use std::future::Future;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Mutex as StdMutex;
use std::sync::OnceLock;
use std::sync::PoisonError;
use std::sync::Weak;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;
use std::time::Duration;
use std::time::Instant;
use std::time::SystemTime;
use std::time::UNIX_EPOCH;

use futures::FutureExt;
use futures::future::BoxFuture;
use regex_lite::Regex;
use serde::Deserialize;
use serde::Serialize;
use serde_json::Value;
use serde_json::json;
use tokio::sync::Semaphore;
use tracing::info;
use tracing::warn;

use super::DriverError;
use super::DriverErrorKind;
use super::DriverResult;
use super::daemon::DEFAULT_TOOL_TIMEOUT_MS;
use super::daemon::DaemonClient;
use super::daemon::ToolResult;
use super::page_scripts;

/// Chrome tab id as the daemon reports it (`z.number().int()`; negative ids
/// are the daemon's clean-session tabs).
pub(crate) type TabId = i64;

pub(crate) const DEFAULT_MAX_TABS: usize = 3;
pub(crate) const MAX_TABS_CAP: usize = 8;
pub(crate) const DEFAULT_TAB_IDLE_MS: u64 = 300_000;
const MIN_TAB_IDLE_MS: u64 = 3_000;
/// Locks older than this are considered abandoned by a crashed process.
const REGISTRY_LOCK_STALE_AFTER: Duration = Duration::from_secs(10);
const REGISTRY_LOCK_DEADLINE: Duration = Duration::from_secs(5);
const REGISTRY_LOCK_POLL: Duration = Duration::from_millis(100);
const NAVIGATE_TIMEOUT_MS: u64 = 30_000;
const RELOAD_TIMEOUT_MS: u64 = 20_000;
const READY_TIMEOUT: Duration = Duration::from_secs(25);
/// Hidden pool tabs throttle page timers (down to one tick/minute after ~5min
/// occluded), so the transport cap needs slack beyond the page-side deadline.
const HIDDEN_TAB_SLACK_MS: u64 = 60_000;
const NO_CONTEXT_RETRY_DELAY: Duration = Duration::from_millis(600);

/// The slice of the daemon the pool needs, so tests can drive it with a fake.
pub(crate) trait TabDaemon: Send + Sync {
    fn call<'a>(
        &'a self,
        tool: &'a str,
        args: Value,
        timeout_ms: u64,
    ) -> BoxFuture<'a, DriverResult<ToolResult>>;

    fn eval_in<'a>(
        &'a self,
        tab_id: TabId,
        expression: String,
        timeout_ms: u64,
    ) -> BoxFuture<'a, DriverResult<Value>>;
}

impl TabDaemon for DaemonClient {
    fn call<'a>(
        &'a self,
        tool: &'a str,
        args: Value,
        timeout_ms: u64,
    ) -> BoxFuture<'a, DriverResult<ToolResult>> {
        DaemonClient::call(self, tool, args, timeout_ms).boxed()
    }

    fn eval_in<'a>(
        &'a self,
        tab_id: TabId,
        expression: String,
        timeout_ms: u64,
    ) -> BoxFuture<'a, DriverResult<Value>> {
        DaemonClient::eval_in(self, tab_id, expression, timeout_ms).boxed()
    }
}

/// One entry of `browser_tabs {action:"list"}`.
#[derive(Debug, Clone, Default, PartialEq, Deserialize, Serialize)]
#[serde(default, rename_all = "camelCase")]
pub(crate) struct TabInfo {
    pub(crate) id: Option<TabId>,
    pub(crate) title: Option<String>,
    pub(crate) url: Option<String>,
    pub(crate) active: bool,
    pub(crate) window_id: Option<i64>,
}

/// Result of the `waitReady` page script.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub(crate) struct ReadyState {
    pub(crate) ready: bool,
    pub(crate) login_required: bool,
    pub(crate) url: String,
}

// ---- cross-process ownership registry ---------------------------------------

/// One row of `tabs.json`. `pid: null` = released, tab up for adoption.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct OwnerEntry {
    pub(crate) tab_id: TabId,
    pub(crate) pid: Option<u32>,
    /// Unix epoch milliseconds.
    pub(crate) since: u64,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub(crate) struct Registry {
    pub(crate) owners: Vec<OwnerEntry>,
}

/// Default registry location (`config.ts:54`).
pub(crate) fn default_registry_path() -> Option<PathBuf> {
    dirs::home_dir().map(|home| home.join(".chatgpt-pro-mcp").join("tabs.json"))
}

/// Port of `loadRegistry`: lenient — a missing/corrupt file is an empty
/// registry and malformed rows are dropped, never fatal.
pub(crate) fn load_registry(path: &Path) -> Registry {
    let Ok(raw) = std::fs::read_to_string(path) else {
        return Registry::default();
    };
    let Ok(value) = serde_json::from_str::<Value>(&raw) else {
        return Registry::default();
    };
    let owners = value
        .get("owners")
        .and_then(Value::as_array)
        .map(|rows| rows.iter().filter_map(owner_entry_from_value).collect())
        .unwrap_or_default();
    Registry { owners }
}

fn owner_entry_from_value(row: &Value) -> Option<OwnerEntry> {
    let tab_id = row.get("tabId")?.as_i64()?;
    let pid = match row.get("pid") {
        None | Some(Value::Null) => None,
        Some(value) => Some(u32::try_from(value.as_u64()?).ok()?),
    };
    let since = row.get("since").and_then(Value::as_u64).unwrap_or(0);
    Some(OwnerEntry { tab_id, pid, since })
}

/// Port of `saveRegistry`: `JSON.stringify(reg, null, 2)`.
pub(crate) fn save_registry(path: &Path, registry: &Registry) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let text = serde_json::to_string_pretty(registry).map_err(std::io::Error::other)?;
    std::fs::write(path, text)
}

/// Staleness/deadline knobs of the registry mutex (tests shrink them).
#[derive(Debug, Clone, Copy)]
pub(crate) struct RegistryLockOptions {
    pub(crate) stale_after: Duration,
    pub(crate) deadline: Duration,
    pub(crate) poll: Duration,
}

impl Default for RegistryLockOptions {
    fn default() -> Self {
        Self {
            stale_after: REGISTRY_LOCK_STALE_AFTER,
            deadline: REGISTRY_LOCK_DEADLINE,
            poll: REGISTRY_LOCK_POLL,
        }
    }
}

fn lock_dir_for(registry_path: &Path) -> PathBuf {
    let mut name = registry_path.as_os_str().to_os_string();
    name.push(".lock");
    PathBuf::from(name)
}

/// Port of `withRegistryLock`: coarse cross-process mutex around registry
/// mutations (mkdir is atomic on every platform). Held only across file
/// reads/writes — never across daemon calls — so contention is milliseconds;
/// locks older than `stale_after` are considered abandoned and stolen.
pub(crate) async fn with_registry_lock<T>(
    registry_path: &Path,
    options: RegistryLockOptions,
    f: impl FnOnce() -> T,
) -> DriverResult<T> {
    let lock_dir = lock_dir_for(registry_path);
    if let Some(parent) = registry_path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let deadline = Instant::now() + options.deadline;
    loop {
        if std::fs::create_dir(&lock_dir).is_ok() {
            break;
        }
        match std::fs::metadata(&lock_dir).and_then(|meta| meta.modified()) {
            Ok(modified) => {
                let age = SystemTime::now()
                    .duration_since(modified)
                    .unwrap_or_default();
                if age > options.stale_after {
                    let _ = std::fs::remove_dir(&lock_dir);
                    continue;
                }
            }
            // Lock vanished between attempts — retry.
            Err(_) => continue,
        }
        if Instant::now() > deadline {
            return Err(DriverError::other(
                "tab registry lock timeout (tabs.json.lock)",
            ));
        }
        tokio::time::sleep(options.poll).await;
    }
    let result = f();
    // Already stolen is fine.
    let _ = std::fs::remove_dir(&lock_dir);
    Ok(result)
}

/// Port of `pidAlive`: access denied means the process exists but is not ours
/// — still alive (the EPERM case of `process.kill(pid, 0)`).
#[cfg(windows)]
pub(crate) fn pid_alive(pid: u32) -> bool {
    use windows_sys::Win32::Foundation::CloseHandle;
    use windows_sys::Win32::Foundation::ERROR_ACCESS_DENIED;
    use windows_sys::Win32::Foundation::GetLastError;
    use windows_sys::Win32::Foundation::STILL_ACTIVE;
    use windows_sys::Win32::System::Threading::GetExitCodeProcess;
    use windows_sys::Win32::System::Threading::OpenProcess;
    use windows_sys::Win32::System::Threading::PROCESS_QUERY_LIMITED_INFORMATION;

    // SAFETY: plain Win32 calls with valid arguments; the handle is closed
    // before returning.
    unsafe {
        let handle = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
        if handle == 0 {
            return GetLastError() == ERROR_ACCESS_DENIED;
        }
        let mut exit_code = 0u32;
        let queried = GetExitCodeProcess(handle, &mut exit_code);
        CloseHandle(handle);
        queried != 0 && exit_code == STILL_ACTIVE as u32
    }
}

/// Port of `pidAlive`: EPERM means the process exists but is not ours — still
/// alive.
#[cfg(unix)]
pub(crate) fn pid_alive(pid: u32) -> bool {
    let Ok(pid) = libc::pid_t::try_from(pid) else {
        return false;
    };
    // SAFETY: signal 0 only probes for existence.
    let rc = unsafe { libc::kill(pid, 0) };
    rc == 0 || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis() as u64)
        .unwrap_or(0)
}

fn gate_closed(what: &str) -> DriverError {
    DriverError::other(format!("{what} gate closed"))
}

// ---- per-tab FIFO queue -------------------------------------------------------

/// Per-tab FIFO queue (`TabLock` in the TS). `pending` counts queued+running
/// jobs, which is what the pool's pick logic uses to spot an idle tab — it is
/// incremented synchronously by [`PoolTab::arm`] so that a pick immediately
/// followed by the run is visible to every other picker.
pub(crate) struct TabLock {
    pending: AtomicUsize,
    /// A 1-permit semaphore is FIFO-fair, which is what the promise chain
    /// gave us (and it is the workspace's async mutex idiom).
    gate: Semaphore,
}

impl TabLock {
    fn new() -> Self {
        Self {
            pending: AtomicUsize::new(0),
            gate: Semaphore::new(/*permits*/ 1),
        }
    }

    pub(crate) fn pending(&self) -> usize {
        self.pending.load(Ordering::SeqCst)
    }
}

/// A slot armed on a tab's queue; released (pending -= 1) on drop, so a
/// cancelled job never leaves the tab looking busy.
pub(crate) struct Armed {
    tab: Arc<PoolTab>,
}

impl Drop for Armed {
    fn drop(&mut self) {
        self.tab.lock.pending.fetch_sub(1, Ordering::SeqCst);
    }
}

struct TabState {
    /// Conversation this tab is currently showing/working (send affinity).
    bound_conversation: Option<String>,
    last_used: Instant,
}

/// One dedicated tab of the pool.
pub(crate) struct PoolTab {
    pub(crate) id: TabId,
    lock: TabLock,
    state: StdMutex<TabState>,
}

impl PoolTab {
    fn new(id: TabId) -> Arc<Self> {
        Arc::new(Self {
            id,
            lock: TabLock::new(),
            state: StdMutex::new(TabState {
                bound_conversation: None,
                last_used: Instant::now(),
            }),
        })
    }

    /// Reserve a slot on this tab's queue (synchronous, no await).
    fn arm(self: &Arc<Self>) -> Armed {
        self.lock.pending.fetch_add(1, Ordering::SeqCst);
        Armed {
            tab: Arc::clone(self),
        }
    }

    pub(crate) fn pending(&self) -> usize {
        self.lock.pending()
    }

    pub(crate) fn bound_conversation(&self) -> Option<String> {
        self.state
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .bound_conversation
            .clone()
    }

    fn last_used(&self) -> Instant {
        self.state
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .last_used
    }

    fn touch(&self) {
        self.state
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .last_used = Instant::now();
    }

    fn idle_for(&self) -> Duration {
        self.last_used().elapsed()
    }

    /// Run `f` holding this tab's exclusive lock, in FIFO order.
    async fn run<T, Fut>(
        self: &Arc<Self>,
        armed: Armed,
        f: impl FnOnce(TabId) -> Fut,
    ) -> DriverResult<T>
    where
        Fut: Future<Output = DriverResult<T>>,
    {
        self.touch();
        let permit = self
            .lock
            .gate
            .acquire()
            .await
            .map_err(|_| gate_closed("tab queue"))?;
        let result = f(self.id).await;
        drop(permit);
        self.touch();
        drop(armed);
        result
    }
}

/// Pool overview for status/debugging (`poolInfo()`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct PoolTabInfo {
    pub(crate) tab_id: TabId,
    pub(crate) conversation_id: Option<String>,
    pub(crate) queued: usize,
}

// ---- the pool -------------------------------------------------------------------

#[derive(Debug, Clone)]
pub(crate) struct TabPoolOptions {
    /// Max tabs THIS instance may own (clamped to `1..=8`).
    pub(crate) max_tabs: usize,
    /// Close a surplus pool tab after this much idle time (ms, min 3000).
    pub(crate) idle_ms: u64,
    /// Cross-process tab ownership registry (`tabs.json`).
    pub(crate) registry_path: PathBuf,
    /// ChatGPT origin, no trailing slash.
    pub(crate) base_url: String,
}

pub(crate) fn clamp_max_tabs(requested: usize) -> usize {
    requested.clamp(1, MAX_TABS_CAP)
}

pub(crate) fn clamp_idle_ms(requested: u64) -> u64 {
    requested.max(MIN_TAB_IDLE_MS)
}

struct PoolInner {
    daemon: Arc<dyn TabDaemon>,
    max_tabs: usize,
    idle: Duration,
    registry_path: PathBuf,
    base_url: String,
    lock_options: RegistryLockOptions,
    pid: u32,
    tabs: StdMutex<Vec<Arc<PoolTab>>>,
    /// Serializes pool growth (adopt/create) so the cap holds.
    growth: Semaphore,
    /// Serializes window-focus juggling (menu operations need an active tab).
    focus: Semaphore,
    sweeper: StdMutex<Option<tokio::task::JoinHandle<()>>>,
}

/// Owns THIS instance's dedicated ChatGPT tabs (`TabManager` in the TS).
pub(crate) struct TabPool {
    inner: Arc<PoolInner>,
}

impl TabPool {
    pub(crate) fn new(daemon: Arc<DaemonClient>, options: TabPoolOptions) -> Self {
        Self::with_daemon(daemon, options)
    }

    /// Same as [`Self::new`] over any [`TabDaemon`] (tests use a fake).
    pub(crate) fn with_daemon(daemon: Arc<dyn TabDaemon>, options: TabPoolOptions) -> Self {
        Self::with_daemon_and_lock_options(daemon, options, RegistryLockOptions::default())
    }

    pub(crate) fn with_daemon_and_lock_options(
        daemon: Arc<dyn TabDaemon>,
        options: TabPoolOptions,
        lock_options: RegistryLockOptions,
    ) -> Self {
        Self {
            inner: Arc::new(PoolInner {
                daemon,
                max_tabs: clamp_max_tabs(options.max_tabs),
                idle: Duration::from_millis(clamp_idle_ms(options.idle_ms)),
                registry_path: options.registry_path,
                base_url: options.base_url.trim_end_matches('/').to_string(),
                lock_options,
                pid: std::process::id(),
                tabs: StdMutex::new(Vec::new()),
                growth: Semaphore::new(/*permits*/ 1),
                focus: Semaphore::new(/*permits*/ 1),
                sweeper: StdMutex::new(None),
            }),
        }
    }

    pub(crate) fn max_tabs(&self) -> usize {
        self.inner.max_tabs
    }

    /// `browser_tabs {action:"list"}`.
    pub(crate) async fn list_tabs(&self) -> DriverResult<Vec<TabInfo>> {
        self.inner.list_tabs().await
    }

    /// Run `f` holding an exclusive lock on a tab appropriate for
    /// `conversation_id`:
    ///  - a tab already bound to that conversation (even if busy — same-thread
    ///    sends must serialize);
    ///  - else an idle tab;
    ///  - else grow the pool (up to `max_tabs`);
    ///  - else queue on the least busy tab.
    ///
    /// The idle pick and lock arming happen under one lock, so a concurrent
    /// caller sees `pending > 0` and correctly grows the pool instead of
    /// double-booking the same tab.
    pub(crate) async fn with_tab_for<T, F, Fut>(
        &self,
        conversation_id: Option<&str>,
        f: F,
    ) -> DriverResult<T>
    where
        F: FnOnce(TabId) -> Fut,
        Fut: Future<Output = DriverResult<T>>,
    {
        let inner = &self.inner;
        if !inner.is_empty() {
            let live = inner.live_tab_ids().await?;
            inner.prune_pool(&live);
        }
        if inner.is_empty() {
            let live = inner.live_tab_ids().await?;
            let _growth = inner
                .growth
                .acquire()
                .await
                .map_err(|_| gate_closed("pool growth"))?;
            if inner.is_empty() {
                inner.acquire_new_tab(&live).await?;
            }
        }

        if let Some(armed) = inner.pick_bound_or_idle(conversation_id) {
            let tab = Arc::clone(&armed.tab);
            return tab.run(armed, f).await;
        }

        let armed = if inner.len() < inner.max_tabs {
            let _growth = inner
                .growth
                .acquire()
                .await
                .map_err(|_| gate_closed("pool growth"))?;
            if let Some(armed) = inner.pick_idle() {
                armed
            } else if inner.len() >= inner.max_tabs {
                inner.least_busy()?
            } else {
                let live = inner.live_tab_ids().await?;
                let tab = inner.acquire_new_tab(&live).await?;
                tab.arm()
            }
        } else {
            inner.least_busy()?
        };
        let tab = Arc::clone(&armed.tab);
        tab.run(armed, f).await
    }

    /// Record which conversation a tab ended up on (send affinity).
    pub(crate) fn bind(&self, tab_id: TabId, conversation_id: Option<&str>) {
        for tab in self.inner.snapshot() {
            let mut state = tab.state.lock().unwrap_or_else(PoisonError::into_inner);
            if tab.id == tab_id {
                state.bound_conversation = conversation_id.map(str::to_string);
            } else if conversation_id.is_some()
                && state.bound_conversation.as_deref() == conversation_id
            {
                state.bound_conversation = None;
            }
        }
    }

    /// Pool overview for status / debugging.
    pub(crate) fn pool_info(&self) -> Vec<PoolTabInfo> {
        self.inner
            .snapshot()
            .iter()
            .map(|tab| PoolTabInfo {
                tab_id: tab.id,
                conversation_id: tab.bound_conversation(),
                queued: tab.pending(),
            })
            .collect()
    }

    /// Ensure at least one tab exists and return the primary (first) one —
    /// status/screenshot and API reads target this when no better tab is known.
    pub(crate) async fn ensure(&self) -> DriverResult<TabId> {
        Ok(self.inner.primary().await?.id)
    }

    /// Best tab for a read-only in-page eval (API fetches need chatgpt.com
    /// cookies, not any specific page): prefer the conversation's bound tab,
    /// then an idle tab (won't be navigated mid-eval), else the primary.
    pub(crate) async fn eval_tab_id(&self, conversation_id: Option<&str>) -> DriverResult<TabId> {
        let tabs = self.inner.snapshot();
        if let Some(conversation_id) = conversation_id
            && let Some(bound) = tabs
                .iter()
                .find(|tab| tab.bound_conversation().as_deref() == Some(conversation_id))
        {
            return Ok(bound.id);
        }
        if let Some(idle) = tabs.iter().find(|tab| tab.pending() == 0) {
            return Ok(idle.id);
        }
        self.ensure().await
    }

    /// Chrome's view of the primary tab, if the pool has one.
    pub(crate) async fn info(&self) -> DriverResult<Option<TabInfo>> {
        let Some(primary) = self.primary_id() else {
            return Ok(None);
        };
        let tabs = self.inner.list_tabs().await?;
        Ok(tabs.into_iter().find(|tab| tab.id == Some(primary)))
    }

    /// The primary (first) pool tab, without creating one (`get id()`).
    pub(crate) fn primary_id(&self) -> Option<TabId> {
        self.inner.snapshot().first().map(|tab| tab.id)
    }

    /// Navigate a pool tab and wait for the composer.
    pub(crate) async fn goto_on(&self, tab_id: TabId, url: &str) -> DriverResult<()> {
        self.inner.goto_on(tab_id, url).await
    }

    /// Make sure tab `tab_id` is showing `conversation_id` (or the new-chat
    /// page when `None`). Navigates only when needed.
    pub(crate) async fn show_conversation_on(
        &self,
        tab_id: TabId,
        conversation_id: Option<&str>,
    ) -> DriverResult<()> {
        let inner = &self.inner;
        let current = inner
            .list_tabs()
            .await?
            .into_iter()
            .find(|tab| tab.id == Some(tab_id))
            .and_then(|tab| tab.url)
            .map(|url| normalize_page_url(&url));
        let want = match conversation_id {
            Some(conversation_id) => format!("{}/c/{conversation_id}", inner.base_url),
            None => format!("{}/", inner.base_url),
        };
        if current.as_deref() != Some(want.trim_end_matches('/')) {
            inner.goto_on(tab_id, &want).await?;
        }
        Ok(())
    }

    /// `waitReady` against an explicit tab id.
    pub(crate) async fn wait_ready_on(
        &self,
        tab_id: TabId,
        timeout: Duration,
    ) -> DriverResult<ReadyState> {
        self.inner.wait_ready_on(tab_id, timeout).await
    }

    /// Run `f` with tab `tab_id` activated (focused window) — required for
    /// Radix menu content to mount — then restore the user's focused tab and
    /// reload the tab to clear stale menu overlays. Focus juggling is a
    /// process-wide mutex: two menus fighting over window focus would tear
    /// each other down.
    pub(crate) async fn with_activated_on<T, F, Fut>(&self, tab_id: TabId, f: F) -> DriverResult<T>
    where
        F: FnOnce(TabId) -> Fut,
        Fut: Future<Output = DriverResult<T>>,
    {
        let inner = &self.inner;
        let _focus = inner
            .focus
            .acquire()
            .await
            .map_err(|_| gate_closed("focus"))?;
        let tabs = inner.list_tabs().await?;
        let previously_active = tabs
            .iter()
            .find(|tab| tab.active && tab.id != Some(tab_id))
            .and_then(|tab| tab.id);
        inner
            .daemon
            .call(
                "browser_tabs",
                json!({"action": "activate", "tabId": tab_id}),
                DEFAULT_TOOL_TIMEOUT_MS,
            )
            .await?;
        let result = f(tab_id).await;
        // Menus opened by synthetic events never unmount on their own; a reload
        // leaves the page pristine for the next operation.
        if let Err(error) = inner
            .daemon
            .call(
                "browser_navigate",
                json!({"tabId": tab_id, "action": "reload", "timeoutMs": RELOAD_TIMEOUT_MS}),
                RELOAD_TIMEOUT_MS,
            )
            .await
        {
            warn!("[chatgpt_web tab] post-menu reload failed: {error}");
        }
        if let Some(previous) = previously_active
            && let Err(error) = inner
                .daemon
                .call(
                    "browser_tabs",
                    json!({"action": "activate", "tabId": previous}),
                    DEFAULT_TOOL_TIMEOUT_MS,
                )
                .await
        {
            warn!("[chatgpt_web tab] could not restore focus to tab {previous}: {error}");
        }
        result
    }

    /// Close our dedicated tabs and drop our owner rows. A tab whose close
    /// fails is released (`pid: null`) instead, so the next instance adopts it.
    pub(crate) async fn shutdown(&self) {
        let inner = &self.inner;
        inner.stop_sweeper();
        let tabs = inner.take_all();
        if tabs.is_empty() {
            return;
        }
        let mut closed = HashSet::new();
        let mut released = HashSet::new();
        for tab in &tabs {
            match inner
                .daemon
                .call(
                    "browser_tabs",
                    json!({"action": "close", "tabId": tab.id}),
                    DEFAULT_TOOL_TIMEOUT_MS,
                )
                .await
            {
                Ok(_) => {
                    closed.insert(tab.id);
                }
                Err(error) => {
                    warn!(
                        "[chatgpt_web tab] close of {} failed at shutdown: {error}",
                        tab.id
                    );
                    released.insert(tab.id);
                }
            }
        }
        let pid = inner.pid;
        let path = inner.registry_path.clone();
        let outcome = with_registry_lock(&path, inner.lock_options, || {
            let mut registry = load_registry(&path);
            registry
                .owners
                .retain(|owner| !closed.contains(&owner.tab_id));
            for owner in &mut registry.owners {
                if released.contains(&owner.tab_id) && owner.pid == Some(pid) {
                    owner.pid = None;
                }
            }
            save_registry(&path, &registry)
        })
        .await;
        match outcome {
            Ok(Ok(())) => {}
            Ok(Err(error)) => {
                warn!("[chatgpt_web tab] registry cleanup at shutdown failed: {error}");
            }
            Err(error) => {
                warn!("[chatgpt_web tab] registry cleanup at shutdown failed: {error}");
            }
        }
        info!("[chatgpt_web tab] shut down {} tab(s)", tabs.len());
    }

    /// Mark ALL our entries released so the next instance adopts the tabs
    /// (best effort, synchronous — runs from `Drop`).
    pub(crate) fn release_sync(&self) {
        self.inner.release_sync();
    }
}

impl Drop for TabPool {
    fn drop(&mut self) {
        self.inner.stop_sweeper();
        self.inner.release_sync();
    }
}

/// `url.split(/[?#]/)[0].replace(/\/$/, "")`.
fn normalize_page_url(url: &str) -> String {
    let base = url.split(['?', '#']).next().unwrap_or(url);
    base.strip_suffix('/').unwrap_or(base).to_string()
}

fn no_execution_context(message: &str) -> bool {
    static PATTERN: OnceLock<Regex> = OnceLock::new();
    let regex = PATTERN.get_or_init(|| {
        #[expect(clippy::expect_used, reason = "the pattern is a compile-time literal")]
        Regex::new("(?i)execution context|No frame|Frame .* detached|target closed")
            .expect("no-context regex must compile")
    });
    regex.is_match(message)
}

impl PoolInner {
    fn snapshot(&self) -> Vec<Arc<PoolTab>> {
        self.tabs
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    fn len(&self) -> usize {
        self.tabs
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .len()
    }

    fn is_empty(&self) -> bool {
        self.len() == 0
    }

    fn take_all(&self) -> Vec<Arc<PoolTab>> {
        std::mem::take(&mut *self.tabs.lock().unwrap_or_else(PoisonError::into_inner))
    }

    fn my_tab_ids(&self) -> HashSet<TabId> {
        self.snapshot().iter().map(|tab| tab.id).collect()
    }

    async fn list_tabs(&self) -> DriverResult<Vec<TabInfo>> {
        let result = self
            .daemon
            .call(
                "browser_tabs",
                json!({"action": "list"}),
                DEFAULT_TOOL_TIMEOUT_MS,
            )
            .await?;
        let value = result.json().ok_or_else(|| {
            DriverError::other(format!(
                "browser_tabs list returned non-JSON: {}",
                result.text
            ))
        })?;
        serde_json::from_value(value).map_err(|error| {
            DriverError::other(format!(
                "browser_tabs list has an unexpected shape: {error}"
            ))
        })
    }

    async fn live_tab_ids(&self) -> DriverResult<HashSet<TabId>> {
        Ok(self
            .list_tabs()
            .await?
            .into_iter()
            .filter_map(|tab| tab.id)
            .collect())
    }

    /// Drop pool tabs that no longer exist in Chrome (closed by hand, crash…).
    fn prune_pool(&self, live: &HashSet<TabId>) {
        let mut tabs = self.tabs.lock().unwrap_or_else(PoisonError::into_inner);
        let before = tabs.len();
        tabs.retain(|tab| live.contains(&tab.id));
        if tabs.len() != before {
            info!(
                "[chatgpt_web tab] pruned {} dead tab(s) from the pool",
                before - tabs.len()
            );
        }
    }

    /// Bound tab (even if busy) or an idle tab, armed under the pool lock.
    fn pick_bound_or_idle(&self, conversation_id: Option<&str>) -> Option<Armed> {
        let tabs = self.tabs.lock().unwrap_or_else(PoisonError::into_inner);
        let bound = conversation_id.and_then(|conversation_id| {
            tabs.iter()
                .find(|tab| tab.bound_conversation().as_deref() == Some(conversation_id))
        });
        let tab = bound.or_else(|| tabs.iter().find(|tab| tab.pending() == 0))?;
        Some(tab.arm())
    }

    fn pick_idle(&self) -> Option<Armed> {
        let tabs = self.tabs.lock().unwrap_or_else(PoisonError::into_inner);
        tabs.iter().find(|tab| tab.pending() == 0).map(PoolTab::arm)
    }

    fn least_busy(&self) -> DriverResult<Armed> {
        let tabs = self.tabs.lock().unwrap_or_else(PoisonError::into_inner);
        let tab = tabs
            .iter()
            .min_by(|a, b| {
                a.pending()
                    .cmp(&b.pending())
                    .then_with(|| a.last_used().cmp(&b.last_used()))
            })
            .ok_or_else(|| DriverError::other("tab pool is empty"))?;
        Ok(tab.arm())
    }

    async fn primary(self: &Arc<Self>) -> DriverResult<Arc<PoolTab>> {
        if !self.is_empty() {
            let live = self.live_tab_ids().await?;
            self.prune_pool(&live);
            if let Some(first) = self.snapshot().first() {
                return Ok(Arc::clone(first));
            }
        }
        let live = self.live_tab_ids().await?;
        let _growth = self
            .growth
            .acquire()
            .await
            .map_err(|_| gate_closed("pool growth"))?;
        if let Some(first) = self.snapshot().first() {
            return Ok(Arc::clone(first));
        }
        self.acquire_new_tab(&live).await
    }

    /// Adopt a released/dead-owned tab from the registry or create a fresh
    /// dedicated one, register it as ours, and add it to the pool. Callers
    /// must hold `growth`.
    async fn acquire_new_tab(
        self: &Arc<Self>,
        live: &HashSet<TabId>,
    ) -> DriverResult<Arc<PoolTab>> {
        let mine = self.my_tab_ids();
        let pid = self.pid;
        let path = self.registry_path.clone();
        let claimed = with_registry_lock(&path, self.lock_options, || {
            let mut registry = load_registry(&path);
            registry.owners.retain(|owner| live.contains(&owner.tab_id));
            // pid == our pid but not in our pool = leftover from a previous
            // process that reused our pid — equally adoptable.
            let stale = registry.owners.iter_mut().find(|owner| {
                !mine.contains(&owner.tab_id)
                    && owner
                        .pid
                        .is_none_or(|owner_pid| owner_pid == pid || !pid_alive(owner_pid))
            });
            let claimed = stale.map(|owner| {
                owner.pid = Some(pid);
                owner.since = now_ms();
                owner.tab_id
            });
            if let Err(error) = save_registry(&path, &registry) {
                warn!("[chatgpt_web tab] could not save the tab registry: {error}");
            }
            claimed
        })
        .await?;

        if let Some(tab_id) = claimed {
            let tab = self.push_tab(tab_id);
            self.start_sweeper();
            self.spawn_sweep_released(live.clone());
            info!(
                "[chatgpt_web tab] adopted ChatGPT tab {tab_id} from a dead/released owner (pool: {})",
                self.len()
            );
            return Ok(tab);
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
            .await?;
        let tab_id = created
            .json()
            .and_then(|value| value.get("id").and_then(Value::as_i64))
            .ok_or_else(|| {
                DriverError::other(format!(
                    "browser_tabs create returned no tab id: {}",
                    created.text
                ))
            })?;
        let tab = self.push_tab(tab_id);
        self.start_sweeper();
        let path = self.registry_path.clone();
        with_registry_lock(&path, self.lock_options, || {
            let mut registry = load_registry(&path);
            registry.owners.retain(|owner| owner.tab_id != tab_id);
            registry.owners.push(OwnerEntry {
                tab_id,
                pid: Some(pid),
                since: now_ms(),
            });
            if let Err(error) = save_registry(&path, &registry) {
                warn!("[chatgpt_web tab] could not save the tab registry: {error}");
            }
        })
        .await?;
        self.spawn_sweep_released(live.clone());
        info!(
            "[chatgpt_web tab] created dedicated ChatGPT tab {tab_id} (pool: {})",
            self.len()
        );
        self.wait_ready_on(tab_id, READY_TIMEOUT).await?;
        Ok(tab)
    }

    fn push_tab(&self, tab_id: TabId) -> Arc<PoolTab> {
        let tab = PoolTab::new(tab_id);
        self.tabs
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(Arc::clone(&tab));
        tab
    }

    async fn goto_on(&self, tab_id: TabId, url: &str) -> DriverResult<()> {
        self.daemon
            .call(
                "browser_navigate",
                json!({
                    "tabId": tab_id,
                    "url": url,
                    "waitUntil": "load",
                    "timeoutMs": NAVIGATE_TIMEOUT_MS,
                }),
                NAVIGATE_TIMEOUT_MS,
            )
            .await?;
        self.wait_ready_on(tab_id, READY_TIMEOUT).await?;
        Ok(())
    }

    async fn wait_ready_on(&self, tab_id: TabId, timeout: Duration) -> DriverResult<ReadyState> {
        // Right after tab creation / navigation the renderer has no execution
        // context yet and evals fail with "Cannot find default execution
        // context"; retry those until the page commits.
        let deadline = Instant::now() + timeout;
        let state = loop {
            let page_wait_ms = deadline
                .saturating_duration_since(Instant::now())
                .as_millis()
                .max(1000) as u64;
            match self
                .daemon
                .eval_in(
                    tab_id,
                    page_scripts::wait_ready(page_wait_ms),
                    page_wait_ms + HIDDEN_TAB_SLACK_MS,
                )
                .await
            {
                Ok(value) => break serde_json::from_value::<ReadyState>(value)?,
                Err(error) => {
                    if !no_execution_context(&error.message) || Instant::now() >= deadline {
                        return Err(error);
                    }
                    tokio::time::sleep(NO_CONTEXT_RETRY_DELAY).await;
                }
            }
        };
        if state.login_required {
            return Err(DriverError::new(
                DriverErrorKind::LoginRequired,
                "ChatGPT shows a login wall in Chrome. Log in to chatgpt.com in the browser, then retry.",
            ));
        }
        if !state.ready {
            return Err(DriverError::ui_changed(format!(
                "ChatGPT composer did not appear within {}ms (tab url: {}). The page may be stuck or the UI changed.",
                timeout.as_millis(),
                state.url
            )));
        }
        Ok(state)
    }

    /// Close a pool tab and drop its registry entry. A failed close keeps the
    /// tab pooled/registered so a later sweep retries instead of orphaning it.
    async fn close_tab(&self, tab_id: TabId, reason: &str) {
        if let Err(error) = self
            .daemon
            .call(
                "browser_tabs",
                json!({"action": "close", "tabId": tab_id}),
                DEFAULT_TOOL_TIMEOUT_MS,
            )
            .await
        {
            warn!("[chatgpt_web tab] close of {tab_id} failed ({reason}): {error}");
            return;
        }
        self.tabs
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .retain(|tab| tab.id != tab_id);
        let path = self.registry_path.clone();
        let outcome = with_registry_lock(&path, self.lock_options, || {
            let mut registry = load_registry(&path);
            registry.owners.retain(|owner| owner.tab_id != tab_id);
            save_registry(&path, &registry)
        })
        .await;
        match outcome {
            Ok(Ok(())) => {}
            Ok(Err(error)) => {
                warn!("[chatgpt_web tab] registry cleanup for closed tab {tab_id} failed: {error}");
            }
            Err(error) => {
                warn!("[chatgpt_web tab] registry cleanup for closed tab {tab_id} failed: {error}");
            }
        }
        info!(
            "[chatgpt_web tab] closed {reason} tab {tab_id} (pool: {})",
            self.len()
        );
    }

    fn sweeper_interval(&self) -> Duration {
        Duration::from_millis((self.idle.as_millis() as u64 / 2).clamp(1_000, 60_000))
    }

    fn start_sweeper(self: &Arc<Self>) {
        let mut slot = self.sweeper.lock().unwrap_or_else(PoisonError::into_inner);
        if slot.is_some() {
            return;
        }
        let interval = self.sweeper_interval();
        let weak: Weak<Self> = Arc::downgrade(self);
        *slot = Some(tokio::spawn(async move {
            loop {
                tokio::time::sleep(interval).await;
                let Some(inner) = weak.upgrade() else {
                    break;
                };
                inner.sweep_idle().await;
            }
        }));
    }

    fn stop_sweeper(&self) {
        if let Some(handle) = self
            .sweeper
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take()
        {
            handle.abort();
        }
    }

    /// Shrink the pool back down: any tab beyond the first that has sat idle
    /// for `idle` gets closed. The close runs holding the tab's lock and
    /// aborts if anything queued behind it meanwhile, so a send can never land
    /// on a tab that is being closed.
    async fn sweep_idle(&self) {
        for tab in self.snapshot() {
            if self.len() <= 1 {
                return;
            }
            if tab.pending() > 0 || tab.idle_for() < self.idle {
                continue;
            }
            let armed = tab.arm();
            let tab_ref = Arc::clone(&tab);
            let _ = tab
                .run(armed, |tab_id| async move {
                    // A caller queued behind us — abort.
                    if tab_ref.pending() > 1 || tab_ref.idle_for() < self.idle || self.len() <= 1 {
                        return Ok(());
                    }
                    self.close_tab(tab_id, "idle").await;
                    Ok(())
                })
                .await;
        }
    }

    fn spawn_sweep_released(self: &Arc<Self>, live: HashSet<TabId>) {
        let inner = Arc::clone(self);
        tokio::spawn(async move {
            inner.sweep_released(&live).await;
        });
    }

    /// Close surplus adoptable tabs left behind by dead instances, keeping ONE
    /// as a spare for the next instance to adopt cheaply. Entries are removed
    /// from the registry atomically before closing, so a sibling adopting
    /// concurrently can never be handed a tab we are about to close.
    async fn sweep_released(&self, live: &HashSet<TabId>) {
        let mine = self.my_tab_ids();
        let pid = self.pid;
        let path = self.registry_path.clone();
        let to_close = with_registry_lock(&path, self.lock_options, || {
            let mut registry = load_registry(&path);
            let surplus: HashSet<TabId> = registry
                .owners
                .iter()
                .filter(|owner| {
                    live.contains(&owner.tab_id)
                        && !mine.contains(&owner.tab_id)
                        && owner
                            .pid
                            .is_none_or(|owner_pid| owner_pid == pid || !pid_alive(owner_pid))
                })
                .map(|owner| owner.tab_id)
                .skip(1)
                .collect();
            if surplus.is_empty() {
                return Vec::new();
            }
            registry
                .owners
                .retain(|owner| !surplus.contains(&owner.tab_id));
            if let Err(error) = save_registry(&path, &registry) {
                warn!("[chatgpt_web tab] could not save the tab registry: {error}");
            }
            surplus.into_iter().collect::<Vec<_>>()
        })
        .await;
        let to_close = match to_close {
            Ok(ids) => ids,
            Err(error) => {
                warn!("[chatgpt_web tab] released-tab sweep failed: {error}");
                return;
            }
        };
        for tab_id in to_close {
            match self
                .daemon
                .call(
                    "browser_tabs",
                    json!({"action": "close", "tabId": tab_id}),
                    DEFAULT_TOOL_TIMEOUT_MS,
                )
                .await
            {
                Ok(_) => info!("[chatgpt_web tab] closed surplus released tab {tab_id}"),
                Err(error) => {
                    warn!("[chatgpt_web tab] close of surplus tab {tab_id} failed: {error}");
                }
            }
        }
    }

    /// Port of `releaseSync`: mark ALL our entries released so the next
    /// instance adopts the tabs. No retry — contended at exit means pid-death
    /// detection covers us anyway.
    fn release_sync(&self) {
        let mine = self.my_tab_ids();
        if mine.is_empty() {
            return;
        }
        let lock_dir = lock_dir_for(&self.registry_path);
        if std::fs::create_dir(&lock_dir).is_err() {
            return;
        }
        let mut registry = load_registry(&self.registry_path);
        for owner in &mut registry.owners {
            if mine.contains(&owner.tab_id) && owner.pid == Some(self.pid) {
                owner.pid = None;
            }
        }
        if let Err(error) = save_registry(&self.registry_path, &registry) {
            warn!("[chatgpt_web tab] release at exit failed: {error}");
        }
        let _ = std::fs::remove_dir(&lock_dir);
    }
}

#[cfg(test)]
#[path = "tabs_tests.rs"]
mod tests;
