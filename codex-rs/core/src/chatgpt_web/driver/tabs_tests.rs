// FORK: tests for the `tab.ts` port (`TabPool`, the tabs.json registry).
use super::*;
use pretty_assertions::assert_eq;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::AtomicI64;
use tokio::sync::Notify;

const BASE_URL: &str = "https://chatgpt.com";

/// Records every daemon call and answers like chrome-mcp would for the tools
/// the pool uses.
struct FakeDaemon {
    calls: StdMutex<Vec<(String, Value)>>,
    tabs: StdMutex<Vec<TabInfo>>,
    next_id: AtomicI64,
    /// How many `browser_eval`s to fail with a no-execution-context error.
    eval_failures: AtomicUsize,
    /// What the `waitReady` script resolves to.
    ready: StdMutex<Value>,
    fail_close: AtomicBool,
}

impl FakeDaemon {
    fn new(tabs: Vec<TabInfo>) -> Arc<Self> {
        Arc::new(Self {
            calls: StdMutex::new(Vec::new()),
            tabs: StdMutex::new(tabs),
            next_id: AtomicI64::new(1000),
            eval_failures: AtomicUsize::new(0),
            ready: StdMutex::new(
                json!({"ready": true, "loginRequired": false, "url": "https://chatgpt.com/"}),
            ),
            fail_close: AtomicBool::new(false),
        })
    }

    fn calls(&self) -> Vec<(String, Value)> {
        self.calls.lock().expect("calls").clone()
    }

    fn calls_named(&self, tool: &str) -> Vec<Value> {
        self.calls()
            .into_iter()
            .filter(|(name, _)| name == tool)
            .map(|(_, args)| args)
            .collect()
    }

    fn tab_actions(&self) -> Vec<(String, Option<TabId>)> {
        self.calls()
            .into_iter()
            .filter(|(name, _)| name == "browser_tabs")
            .map(|(_, args)| {
                (
                    args["action"].as_str().unwrap_or_default().to_string(),
                    args["tabId"].as_i64(),
                )
            })
            .collect()
    }

    fn set_ready(&self, value: Value) {
        *self.ready.lock().expect("ready") = value;
    }
}

fn tab(id: TabId, url: &str, active: bool) -> TabInfo {
    TabInfo {
        id: Some(id),
        title: Some(format!("tab {id}")),
        url: Some(url.to_string()),
        active,
        window_id: Some(1),
    }
}

fn text(value: &Value) -> ToolResult {
    ToolResult {
        text: serde_json::to_string_pretty(value).expect("serialize"),
        images: Vec::new(),
    }
}

impl TabDaemon for FakeDaemon {
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
        async move {
            let action = args["action"].as_str().unwrap_or("goto");
            match (tool, action) {
                ("browser_tabs", "list") => {
                    let tabs = self.tabs.lock().expect("tabs").clone();
                    Ok(text(&serde_json::to_value(tabs).expect("serialize")))
                }
                ("browser_tabs", "create") => {
                    let id = self.next_id.fetch_add(1, Ordering::SeqCst);
                    let url = args["url"].as_str().unwrap_or_default().to_string();
                    self.tabs
                        .lock()
                        .expect("tabs")
                        .push(tab(id, &url, /*active*/ false));
                    Ok(text(
                        &json!({"id": id, "windowId": 2, "url": url, "dedicated": true}),
                    ))
                }
                ("browser_tabs", "close") => {
                    if self.fail_close.load(Ordering::SeqCst) {
                        return Err(DriverError::tool("No tab with id"));
                    }
                    let id = args["tabId"].as_i64();
                    self.tabs.lock().expect("tabs").retain(|t| t.id != id);
                    Ok(text(&json!({"closed": id})))
                }
                ("browser_tabs", "activate") => {
                    let id = args["tabId"].as_i64();
                    for t in self.tabs.lock().expect("tabs").iter_mut() {
                        t.active = t.id == id;
                    }
                    Ok(text(&json!({"active": id})))
                }
                ("browser_navigate", "goto") => {
                    let id = args["tabId"].as_i64();
                    if let Some(url) = args["url"].as_str() {
                        for t in self.tabs.lock().expect("tabs").iter_mut() {
                            if t.id == id {
                                t.url = Some(url.to_string());
                            }
                        }
                    }
                    Ok(text(&json!({"url": args["url"]})))
                }
                _ => Ok(text(&json!({}))),
            }
        }
        .boxed()
    }

    fn eval_in<'a>(
        &'a self,
        tab_id: TabId,
        expression: String,
        timeout_ms: u64,
    ) -> BoxFuture<'a, DriverResult<Value>> {
        self.calls.lock().expect("calls").push((
            "browser_eval".to_string(),
            json!({"tabId": tab_id, "expression": expression, "timeoutMs": timeout_ms}),
        ));
        async move {
            let remaining = self.eval_failures.load(Ordering::SeqCst);
            if remaining > 0 {
                self.eval_failures.store(remaining - 1, Ordering::SeqCst);
                return Err(DriverError::tool("Cannot find default execution context"));
            }
            Ok(self.ready.lock().expect("ready").clone())
        }
        .boxed()
    }
}

fn fast_lock_options() -> RegistryLockOptions {
    RegistryLockOptions {
        stale_after: Duration::from_secs(10),
        deadline: Duration::from_millis(500),
        poll: Duration::from_millis(10),
    }
}

fn pool_with(daemon: &Arc<FakeDaemon>, registry_path: &Path, max_tabs: usize) -> TabPool {
    let daemon = Arc::clone(daemon);
    let daemon: Arc<dyn TabDaemon> = daemon;
    TabPool::with_daemon_and_lock_options(
        daemon,
        TabPoolOptions {
            max_tabs,
            idle_ms: DEFAULT_TAB_IDLE_MS,
            registry_path: registry_path.to_path_buf(),
            base_url: BASE_URL.to_string(),
        },
        fast_lock_options(),
    )
}

fn registry_path(dir: &tempfile::TempDir) -> PathBuf {
    dir.path().join(".chatgpt-pro-mcp").join("tabs.json")
}

/// A child process that has already exited: its pid cannot be alive.
fn dead_pid() -> u32 {
    let mut child = if cfg!(windows) {
        std::process::Command::new("cmd")
            .args(["/c", "exit", "0"])
            .spawn()
    } else {
        std::process::Command::new("true").spawn()
    }
    .expect("spawn child");
    let pid = child.id();
    child.wait().expect("wait child");
    pid
}

/// A child process that stays alive until dropped.
struct LiveChild(std::process::Child);

impl LiveChild {
    fn spawn() -> Self {
        let child = if cfg!(windows) {
            std::process::Command::new("ping")
                .args(["-n", "60", "127.0.0.1"])
                .stdout(std::process::Stdio::null())
                .spawn()
        } else {
            std::process::Command::new("sleep").arg("60").spawn()
        }
        .expect("spawn child");
        Self(child)
    }

    fn pid(&self) -> u32 {
        self.0.id()
    }
}

impl Drop for LiveChild {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

#[test]
fn registry_round_trips_the_node_json_shape() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = registry_path(&dir);
    let registry = Registry {
        owners: vec![
            OwnerEntry {
                tab_id: 626460085,
                pid: Some(82060),
                since: 1787796229275,
            },
            OwnerEntry {
                tab_id: 7,
                pid: None,
                since: 1,
            },
        ],
    };
    save_registry(&path, &registry).expect("save");
    let written = std::fs::read_to_string(&path).expect("read");
    assert_eq!(
        written,
        "{\n  \"owners\": [\n    {\n      \"tabId\": 626460085,\n      \"pid\": 82060,\n      \"since\": 1787796229275\n    },\n    {\n      \"tabId\": 7,\n      \"pid\": null,\n      \"since\": 1\n    }\n  ]\n}"
    );
    assert_eq!(load_registry(&path), registry);
}

#[test]
fn registry_parses_the_live_node_file_and_drops_malformed_rows() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = registry_path(&dir);
    std::fs::create_dir_all(path.parent().expect("parent")).expect("mkdir");
    std::fs::write(
        &path,
        r#"{
  "owners": [
    {
      "tabId": 626460085,
      "pid": 82060,
      "since": 1787796229275
    },
    { "tabId": "not-a-number", "pid": 1, "since": 2 },
    { "tabId": 9, "pid": "x", "since": 2 },
    { "tabId": 10, "pid": null }
  ]
}"#,
    )
    .expect("write");
    assert_eq!(
        load_registry(&path),
        Registry {
            owners: vec![
                OwnerEntry {
                    tab_id: 626460085,
                    pid: Some(82060),
                    since: 1787796229275,
                },
                OwnerEntry {
                    tab_id: 10,
                    pid: None,
                    since: 0,
                },
            ],
        }
    );
    assert_eq!(
        load_registry(&dir.path().join("missing.json")),
        Registry::default()
    );
    std::fs::write(&path, "{ not json").expect("write");
    assert_eq!(load_registry(&path), Registry::default());
}

#[tokio::test]
async fn registry_lock_is_stolen_when_stale() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = registry_path(&dir);
    std::fs::create_dir_all(path.parent().expect("parent")).expect("mkdir");
    let lock_dir = lock_dir_for(&path);
    std::fs::create_dir(&lock_dir).expect("mkdir lock");
    // Windows will not let a test rewrite a directory's mtime, so age the
    // lock by shrinking the staleness window instead.
    let options = RegistryLockOptions {
        stale_after: Duration::from_millis(20),
        deadline: Duration::from_millis(500),
        poll: Duration::from_millis(10),
    };
    std::thread::sleep(Duration::from_millis(150));
    let ran = with_registry_lock(&path, options, || 42)
        .await
        .expect("stale lock must be stolen");
    assert_eq!(ran, 42);
    assert!(!lock_dir.exists(), "lock dir must be released afterwards");
}

#[tokio::test]
async fn registry_lock_times_out_while_a_fresh_lock_is_held() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = registry_path(&dir);
    std::fs::create_dir_all(path.parent().expect("parent")).expect("mkdir");
    let lock_dir = lock_dir_for(&path);
    std::fs::create_dir(&lock_dir).expect("mkdir lock");

    let options = RegistryLockOptions {
        stale_after: Duration::from_secs(10),
        deadline: Duration::from_millis(200),
        poll: Duration::from_millis(10),
    };
    let error = with_registry_lock(&path, options, || ())
        .await
        .expect_err("held lock must time out");
    assert_eq!(error.kind, DriverErrorKind::Other);
    assert_eq!(error.message, "tab registry lock timeout (tabs.json.lock)");
    assert!(lock_dir.exists(), "a live lock must not be stolen");
}

#[test]
fn max_tabs_is_clamped_between_one_and_eight() {
    assert_eq!(clamp_max_tabs(0), 1);
    assert_eq!(clamp_max_tabs(3), 3);
    assert_eq!(clamp_max_tabs(20), 8);
    assert_eq!(clamp_idle_ms(10), 3_000);
    assert_eq!(clamp_idle_ms(300_000), 300_000);
    let dir = tempfile::tempdir().expect("tempdir");
    let daemon = FakeDaemon::new(Vec::new());
    assert_eq!(pool_with(&daemon, &registry_path(&dir), 0).max_tabs(), 1);
    assert_eq!(pool_with(&daemon, &registry_path(&dir), 50).max_tabs(), 8);
}

#[test]
fn pid_alive_sees_this_process_and_not_an_exited_child() {
    assert!(pid_alive(std::process::id()));
    assert!(!pid_alive(dead_pid()));
}

#[test]
fn page_urls_are_compared_without_query_and_trailing_slash() {
    assert_eq!(
        normalize_page_url("https://chatgpt.com/c/abc?model=x#y"),
        "https://chatgpt.com/c/abc"
    );
    assert_eq!(
        normalize_page_url("https://chatgpt.com/"),
        "https://chatgpt.com"
    );
}

#[tokio::test]
async fn pool_adopts_a_registry_tab_whose_owner_is_dead() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = registry_path(&dir);
    save_registry(
        &path,
        &Registry {
            owners: vec![OwnerEntry {
                tab_id: 900,
                pid: Some(dead_pid()),
                since: 5,
            }],
        },
    )
    .expect("seed registry");
    let daemon = FakeDaemon::new(vec![
        tab(1, "https://example.com/", true),
        tab(900, "https://chatgpt.com/", false),
    ]);
    let pool = pool_with(&daemon, &path, 3);

    let used = pool
        .with_tab_for(None, |tab_id| async move { Ok(tab_id) })
        .await
        .expect("with_tab_for");
    assert_eq!(used, 900);
    assert!(
        daemon
            .calls_named("browser_tabs")
            .iter()
            .all(|args| args["action"] != "create"),
        "an adoptable tab must not trigger a create: {:?}",
        daemon.calls()
    );
    let registry = load_registry(&path);
    assert_eq!(registry.owners.len(), 1);
    assert_eq!(registry.owners[0].tab_id, 900);
    assert_eq!(registry.owners[0].pid, Some(std::process::id()));
    assert!(registry.owners[0].since > 5);
    assert_eq!(pool.primary_id(), Some(900));
}

#[tokio::test]
async fn pool_creates_a_dedicated_tab_when_every_registered_tab_has_a_live_owner() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = registry_path(&dir);
    let sibling = LiveChild::spawn();
    save_registry(
        &path,
        &Registry {
            owners: vec![OwnerEntry {
                tab_id: 900,
                pid: Some(sibling.pid()),
                since: 5,
            }],
        },
    )
    .expect("seed registry");
    let daemon = FakeDaemon::new(vec![
        tab(1, "https://example.com/", true),
        tab(900, "https://chatgpt.com/", false),
    ]);
    let pool = pool_with(&daemon, &path, 3);

    let used = pool
        .with_tab_for(None, |tab_id| async move { Ok(tab_id) })
        .await
        .expect("with_tab_for");
    assert_eq!(used, 1000);
    assert_eq!(
        daemon.calls_named("browser_tabs"),
        vec![
            json!({"action": "list"}),
            json!({"action": "create", "url": "https://chatgpt.com/", "dedicated": true}),
        ]
    );
    let evals = daemon.calls_named("browser_eval");
    assert_eq!(evals.len(), 1, "a created tab waits for the composer once");
    assert_eq!(evals[0]["tabId"], json!(1000));
    assert!(evals[0]["timeoutMs"].as_u64().expect("timeout") >= 60_000);
    assert!(
        evals[0]["expression"]
            .as_str()
            .expect("expression")
            .starts_with("() =>"),
        "waitReady must be a page function"
    );

    let registry = load_registry(&path);
    assert_eq!(registry.owners.len(), 2);
    assert_eq!(registry.owners[0].tab_id, 900);
    assert_eq!(registry.owners[0].pid, Some(sibling.pid()));
    assert_eq!(registry.owners[1].tab_id, 1000);
    assert_eq!(registry.owners[1].pid, Some(std::process::id()));
    drop(sibling);
}

#[tokio::test]
async fn same_conversation_serializes_on_its_tab_while_others_grow_the_pool() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = registry_path(&dir);
    let daemon = FakeDaemon::new(vec![tab(1, "https://example.com/", true)]);
    let pool = Arc::new(pool_with(&daemon, &path, 3));

    let first = pool
        .with_tab_for(None, |tab_id| async move { Ok(tab_id) })
        .await
        .expect("first");
    pool.bind(first, Some("conv-a"));

    let release = Arc::new(Notify::new());
    let holder = {
        let pool = Arc::clone(&pool);
        let release = Arc::clone(&release);
        tokio::spawn(async move {
            pool.with_tab_for(Some("conv-a"), |tab_id| async move {
                release.notified().await;
                Ok(tab_id)
            })
            .await
        })
    };
    // Wait until the holder is running on the bound tab.
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if pool
                .pool_info()
                .iter()
                .any(|info| info.tab_id == first && info.queued == 1)
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("holder must start");

    // Another send to the same conversation queues behind the holder …
    let queued = {
        let pool = Arc::clone(&pool);
        tokio::spawn(async move {
            pool.with_tab_for(Some("conv-a"), |tab_id| async move { Ok(tab_id) })
                .await
        })
    };
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if pool
                .pool_info()
                .iter()
                .any(|info| info.tab_id == first && info.queued == 2)
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("second send must queue on the bound tab");

    // … while a send to another conversation grows the pool and runs now.
    let other = tokio::time::timeout(
        Duration::from_secs(5),
        pool.with_tab_for(Some("conv-b"), |tab_id| async move { Ok(tab_id) }),
    )
    .await
    .expect("must not wait for the holder")
    .expect("other");
    assert_ne!(other, first);
    assert_eq!(pool.pool_info().len(), 2);

    release.notify_waiters();
    release.notify_one();
    let held = holder.await.expect("join").expect("holder");
    let second = queued.await.expect("join").expect("queued");
    assert_eq!(held, first);
    assert_eq!(second, first);
    assert!(pool.pool_info().iter().all(|info| info.queued == 0));
}

#[tokio::test]
async fn bind_moves_the_conversation_between_tabs() {
    let dir = tempfile::tempdir().expect("tempdir");
    let daemon = FakeDaemon::new(vec![tab(1, "https://example.com/", true)]);
    let pool = pool_with(&daemon, &registry_path(&dir), 3);
    let first = pool.ensure().await.expect("ensure");
    pool.bind(first, Some("conv-a"));
    assert_eq!(
        pool.pool_info(),
        vec![PoolTabInfo {
            tab_id: first,
            conversation_id: Some("conv-a".to_string()),
            queued: 0,
        }]
    );
    assert_eq!(
        pool.eval_tab_id(Some("conv-a")).await.expect("eval tab"),
        first
    );
    assert_eq!(pool.eval_tab_id(None).await.expect("eval tab"), first);
    pool.bind(first, None);
    assert_eq!(pool.pool_info()[0].conversation_id, None);
}

#[tokio::test]
async fn goto_on_navigates_with_load_and_waits_for_the_composer() {
    let dir = tempfile::tempdir().expect("tempdir");
    let daemon = FakeDaemon::new(vec![tab(1, "https://example.com/", true)]);
    let pool = pool_with(&daemon, &registry_path(&dir), 3);
    let tab_id = pool.ensure().await.expect("ensure");
    let before = daemon.calls().len();

    pool.goto_on(tab_id, "https://chatgpt.com/c/abc")
        .await
        .expect("goto");
    let calls = daemon.calls().split_off(before);
    assert_eq!(calls.len(), 2);
    assert_eq!(
        calls[0],
        (
            "browser_navigate".to_string(),
            json!({
                "tabId": tab_id,
                "url": "https://chatgpt.com/c/abc",
                "waitUntil": "load",
                "timeoutMs": 30_000,
            })
        )
    );
    assert_eq!(calls[1].0, "browser_eval");
    assert_eq!(calls[1].1["tabId"], json!(tab_id));
}

#[tokio::test]
async fn wait_ready_on_retries_while_the_page_has_no_execution_context() {
    let dir = tempfile::tempdir().expect("tempdir");
    let daemon = FakeDaemon::new(vec![tab(1, "https://example.com/", true)]);
    let pool = pool_with(&daemon, &registry_path(&dir), 3);
    let tab_id = pool.ensure().await.expect("ensure");
    let before = daemon.calls_named("browser_eval").len();
    daemon.eval_failures.store(2, Ordering::SeqCst);

    let state = pool
        .wait_ready_on(tab_id, Duration::from_secs(10))
        .await
        .expect("ready after retries");
    assert!(state.ready);
    assert_eq!(daemon.calls_named("browser_eval").len() - before, 3);
}

#[tokio::test]
async fn wait_ready_on_reports_a_login_wall_and_a_missing_composer() {
    let dir = tempfile::tempdir().expect("tempdir");
    let daemon = FakeDaemon::new(vec![tab(1, "https://example.com/", true)]);
    let pool = pool_with(&daemon, &registry_path(&dir), 3);
    let tab_id = pool.ensure().await.expect("ensure");

    daemon.set_ready(
        json!({"ready": false, "loginRequired": true, "url": "https://chatgpt.com/auth/login"}),
    );
    let error = pool
        .wait_ready_on(tab_id, Duration::from_secs(1))
        .await
        .expect_err("login wall");
    assert_eq!(error.kind, DriverErrorKind::LoginRequired);

    daemon
        .set_ready(json!({"ready": false, "loginRequired": false, "url": "https://chatgpt.com/"}));
    let error = pool
        .wait_ready_on(tab_id, Duration::from_secs(1))
        .await
        .expect_err("no composer");
    assert_eq!(error.kind, DriverErrorKind::UiChanged);
    assert!(error.message.contains("https://chatgpt.com/"));
}

#[tokio::test]
async fn with_activated_on_activates_reloads_and_restores_focus() {
    let dir = tempfile::tempdir().expect("tempdir");
    let daemon = FakeDaemon::new(vec![tab(1, "https://example.com/", true)]);
    let pool = pool_with(&daemon, &registry_path(&dir), 3);
    let tab_id = pool.ensure().await.expect("ensure");
    let before = daemon.calls().len();

    let seen = pool
        .with_activated_on(tab_id, |id| async move { Ok(id) })
        .await
        .expect("activated");
    assert_eq!(seen, tab_id);
    let calls = daemon.calls().split_off(before);
    assert_eq!(
        calls,
        vec![
            ("browser_tabs".to_string(), json!({"action": "list"})),
            (
                "browser_tabs".to_string(),
                json!({"action": "activate", "tabId": tab_id})
            ),
            (
                "browser_navigate".to_string(),
                json!({"tabId": tab_id, "action": "reload", "timeoutMs": 20_000})
            ),
            (
                "browser_tabs".to_string(),
                json!({"action": "activate", "tabId": 1})
            ),
        ]
    );
}

#[tokio::test]
async fn show_conversation_on_navigates_only_when_needed() {
    let dir = tempfile::tempdir().expect("tempdir");
    let daemon = FakeDaemon::new(vec![tab(1, "https://example.com/", true)]);
    let pool = pool_with(&daemon, &registry_path(&dir), 3);
    let tab_id = pool.ensure().await.expect("ensure");
    daemon
        .call(
            "browser_navigate",
            json!({"tabId": tab_id, "url": "https://chatgpt.com/c/abc?model=x"}),
            0,
        )
        .await
        .expect("seed url");
    let before = daemon.calls_named("browser_navigate").len();

    pool.show_conversation_on(tab_id, Some("abc"))
        .await
        .expect("already there");
    assert_eq!(daemon.calls_named("browser_navigate").len(), before);

    pool.show_conversation_on(tab_id, None)
        .await
        .expect("navigate home");
    let navigations = daemon.calls_named("browser_navigate");
    assert_eq!(navigations.len(), before + 1);
    assert_eq!(navigations[before]["url"], json!("https://chatgpt.com/"));

    pool.show_conversation_on(tab_id, None)
        .await
        .expect("already home");
    assert_eq!(daemon.calls_named("browser_navigate").len(), before + 1);
}

#[tokio::test]
async fn shutdown_closes_our_tabs_and_removes_our_rows() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = registry_path(&dir);
    let daemon = FakeDaemon::new(vec![tab(1, "https://example.com/", true)]);
    let pool = pool_with(&daemon, &path, 3);
    let tab_id = pool.ensure().await.expect("ensure");
    assert_eq!(load_registry(&path).owners.len(), 1);

    pool.shutdown().await;
    assert!(
        daemon
            .tab_actions()
            .contains(&("close".to_string(), Some(tab_id)))
    );
    assert_eq!(load_registry(&path), Registry::default());
    assert_eq!(pool.primary_id(), None);
    assert!(
        daemon
            .tabs
            .lock()
            .expect("tabs")
            .iter()
            .all(|t| t.id != Some(tab_id))
    );
}

#[tokio::test]
async fn shutdown_releases_a_tab_it_could_not_close() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = registry_path(&dir);
    let daemon = FakeDaemon::new(vec![tab(1, "https://example.com/", true)]);
    let pool = pool_with(&daemon, &path, 3);
    let tab_id = pool.ensure().await.expect("ensure");
    daemon.fail_close.store(true, Ordering::SeqCst);

    pool.shutdown().await;
    let registry = load_registry(&path);
    assert_eq!(registry.owners.len(), 1);
    assert_eq!(registry.owners[0].tab_id, tab_id);
    assert_eq!(registry.owners[0].pid, None);
}

#[tokio::test]
async fn dropping_the_pool_releases_our_rows_for_adoption() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = registry_path(&dir);
    let daemon = FakeDaemon::new(vec![tab(1, "https://example.com/", true)]);
    let pool = pool_with(&daemon, &path, 3);
    let tab_id = pool.ensure().await.expect("ensure");
    assert_eq!(load_registry(&path).owners[0].pid, Some(std::process::id()));

    drop(pool);
    let registry = load_registry(&path);
    assert_eq!(registry.owners.len(), 1);
    assert_eq!(registry.owners[0].tab_id, tab_id);
    assert_eq!(registry.owners[0].pid, None);
    assert!(!lock_dir_for(&path).exists());
}

#[tokio::test]
async fn pool_prunes_tabs_chrome_no_longer_has() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = registry_path(&dir);
    let daemon = FakeDaemon::new(vec![tab(1, "https://example.com/", true)]);
    let pool = pool_with(&daemon, &path, 3);
    let first = pool.ensure().await.expect("ensure");
    // The user closed our tab by hand.
    daemon
        .tabs
        .lock()
        .expect("tabs")
        .retain(|t| t.id != Some(first));

    let second = pool
        .with_tab_for(None, |tab_id| async move { Ok(tab_id) })
        .await
        .expect("with_tab_for");
    assert_ne!(second, first);
    assert_eq!(pool.pool_info().len(), 1);
    assert_eq!(pool.primary_id(), Some(second));
}
