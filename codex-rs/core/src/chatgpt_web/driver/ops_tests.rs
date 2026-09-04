// FORK: tests for the `ops.ts` port (`ChatGptOps`, model resolution, the send
// phase machine, the reply completion rule) plus the live suite against the
// chrome-mcp daemon (`#[ignore]`, run with `--test-threads=1`).
use super::*;
use crate::chatgpt_web::driver::api::Asset;
use crate::chatgpt_web::driver::api::ModelEntry;
use crate::chatgpt_web::driver::api::RawConversation;
use crate::chatgpt_web::driver::api::clear_models_cache;
use crate::chatgpt_web::driver::api::normalize;
use crate::chatgpt_web::driver::daemon::ToolResult;
use crate::chatgpt_web::driver::tabs::RegistryLockOptions;
use crate::chatgpt_web::driver::tabs::TabInfo;
use crate::chatgpt_web::driver::tabs::TabPoolOptions;
use pretty_assertions::assert_eq;
use std::collections::HashMap;
use std::collections::HashSet;
use std::collections::VecDeque;
use std::sync::atomic::AtomicI64;
use std::sync::atomic::Ordering;

const BASE_URL: &str = "https://chatgpt.com";
const FINISHED_ID: &str = "22222222-aaaa-4bbb-8ccc-000000000002";
const NEW_ID: &str = "33333333-aaaa-4bbb-8ccc-000000000003";

fn fixture(name: &str) -> Value {
    let text = match name {
        "finished" => include_str!("../fixtures/conv_finished.json"),
        "in_progress" => include_str!("../fixtures/conv_in_progress.json"),
        "stopped_old_in_progress" => include_str!("../fixtures/conv_stopped_old_in_progress.json"),
        "image_assets" => include_str!("../fixtures/conv_image_assets.json"),
        "thoughts" => include_str!("../fixtures/conv_thoughts.json"),
        other => panic!("unknown fixture {other}"),
    };
    serde_json::from_str(text).expect("fixture json")
}

fn conversation(name: &str) -> Conversation {
    let raw: RawConversation = serde_json::from_value(fixture(name)).expect("raw conversation");
    normalize(&raw)
}

fn models_payload() -> Value {
    json!({
        "status": 200,
        "json": {
            "models": [
                { "slug": "gpt-5-6", "title": "GPT-5.6" },
                { "slug": "gpt-5-6-instant", "title": "GPT-5.6 Instant" },
                { "slug": "gpt-5-6-thinking", "title": "GPT-5.6 Thinking" },
                { "slug": "gpt-5-6-pro", "title": "GPT-5.6 Pro" },
                { "slug": "gpt-4-1", "title": "GPT-4.1" }
            ],
            "default_model_slug": "gpt-5-6"
        },
        "text": null
    })
}

fn models_info() -> ModelsInfo {
    ModelsInfo {
        default_slug: Some("gpt-5-6-thinking".to_string()),
        models: [
            "gpt-5-6",
            "gpt-5-6-instant",
            "gpt-5-6-thinking",
            "gpt-5-6-pro",
            "gpt-4-1",
        ]
        .iter()
        .map(|slug| ModelEntry {
            slug: (*slug).to_string(),
            title: slug.to_uppercase(),
        })
        .collect(),
    }
}

fn fast_timings() -> OpsTimings {
    OpsTimings {
        label_wait: Duration::ZERO,
        label_poll: Duration::ZERO,
        generating_grace: Duration::ZERO,
        generating_poll: Duration::ZERO,
        settle_after_generating: Duration::ZERO,
        composer_reset_settle: Duration::ZERO,
        tiles_deadline: Duration::from_millis(200),
        tiles_poll: Duration::from_millis(20),
        late_popup_delay: Duration::ZERO,
        attachments_poll: Duration::from_millis(20),
        confirm_submit_wait: Duration::ZERO,
        confirm_submit_poll: Duration::ZERO,
        reply_poll: Duration::ZERO,
        stop_window: Duration::from_millis(200),
        stop_poll: Duration::from_millis(20),
    }
}

/// Which page script an eval expression is (by a marker unique to each script).
fn script_kind(expr: &str) -> &'static str {
    if expr.contains("loginRequired") {
        "wait_ready"
    } else if expr.contains("hasComposer:") {
        "composer_state"
    } else if expr.contains("const TEXT =") {
        "set_composer_text"
    } else if expr.contains("{ tiles, legacy }") {
        "attachment_tiles"
    } else if expr.contains("dismissed: true") {
        "dismiss_upload_dialog"
    } else if expr.contains("send button not found") {
        "click_send"
    } else if expr.contains("stillGenerating") {
        "click_stop"
    } else if expr.contains("/backend-api/models") {
        "api_models"
    } else if expr.contains("/backend-api/conversation/") {
        "api_conversation"
    } else if expr.contains("const TARGET =") {
        "menu_select"
    } else if expr.contains("pageErrorProbe") {
        "page_errors"
    } else {
        "other"
    }
}

/// Records every daemon call/eval and answers like chrome-mcp + chatgpt.com
/// would, with per-script canned responses (`script`) over defaults.
struct FakeDaemon {
    calls: StdMutex<Vec<(String, Value)>>,
    evals: StdMutex<Vec<(&'static str, TabId, String)>>,
    tabs: StdMutex<Vec<TabInfo>>,
    next_id: AtomicI64,
    scripted: StdMutex<HashMap<&'static str, VecDeque<DriverResult<Value>>>>,
    defaults: StdMutex<HashMap<&'static str, DriverResult<Value>>>,
    /// Basenames set on file inputs so far (drives the tile/attachment defaults).
    uploaded: StdMutex<Vec<String>>,
    /// Selectors whose `browser_upload` fails.
    failing_upload_selectors: StdMutex<HashSet<String>>,
}

impl FakeDaemon {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            calls: StdMutex::new(Vec::new()),
            evals: StdMutex::new(Vec::new()),
            tabs: StdMutex::new(Vec::new()),
            next_id: AtomicI64::new(500),
            scripted: StdMutex::new(HashMap::new()),
            defaults: StdMutex::new(HashMap::new()),
            uploaded: StdMutex::new(Vec::new()),
            failing_upload_selectors: StdMutex::new(HashSet::new()),
        })
    }

    /// Queue one response for the next eval of `kind`.
    fn script(&self, kind: &'static str, response: DriverResult<Value>) {
        self.scripted
            .lock()
            .expect("scripted")
            .entry(kind)
            .or_default()
            .push_back(response);
    }

    fn set_default(&self, kind: &'static str, response: DriverResult<Value>) {
        self.defaults
            .lock()
            .expect("defaults")
            .insert(kind, response);
    }

    fn evals_of(&self, kind: &str) -> Vec<String> {
        self.evals
            .lock()
            .expect("evals")
            .iter()
            .filter(|(k, _, _)| *k == kind)
            .map(|(_, _, expr)| expr.clone())
            .collect()
    }

    fn eval_kinds(&self) -> Vec<&'static str> {
        self.evals
            .lock()
            .expect("evals")
            .iter()
            .map(|(k, _, _)| *k)
            .collect()
    }

    fn calls_named(&self, tool: &str) -> Vec<Value> {
        self.calls
            .lock()
            .expect("calls")
            .iter()
            .filter(|(name, _)| name == tool)
            .map(|(_, args)| args.clone())
            .collect()
    }

    fn navigations(&self) -> Vec<String> {
        self.calls_named("browser_navigate")
            .into_iter()
            .filter_map(|args| args["url"].as_str().map(str::to_string))
            .collect()
    }

    fn tab_url(&self, id: TabId) -> Option<String> {
        self.tabs
            .lock()
            .expect("tabs")
            .iter()
            .find(|t| t.id == Some(id))
            .and_then(|t| t.url.clone())
    }

    fn default_for(&self, kind: &str) -> DriverResult<Value> {
        let uploaded = self.uploaded.lock().expect("uploaded").clone();
        let current_url = self
            .tabs
            .lock()
            .expect("tabs")
            .first()
            .and_then(|t| t.url.clone())
            .unwrap_or_else(|| format!("{BASE_URL}/"));
        match kind {
            "wait_ready" => Ok(json!({"ready": true, "loginRequired": false, "url": current_url})),
            "composer_state" => Ok(json!({
                "hasComposer": true,
                "url": current_url,
                "modelLabel": "Instant",
                "sendVisible": true,
                "sendEnabled": true,
                "generating": false,
                "attachments": uploaded.len(),
                "text": ""
            })),
            "set_composer_text" => Ok(json!({"ok": true, "length": 12})),
            "attachment_tiles" => Ok(json!({"tiles": uploaded, "legacy": 0})),
            "dismiss_upload_dialog" => Ok(json!({"found": false})),
            "click_send" => Ok(json!({
                "ok": true,
                "conversationId": NEW_ID,
                "generating": true,
                "url": format!("{BASE_URL}/c/{NEW_ID}")
            })),
            "click_stop" => Ok(json!({"ok": true, "stillGenerating": false})),
            "api_models" => Ok(models_payload()),
            "api_conversation" => {
                Ok(json!({"status": 200, "json": fixture("finished"), "text": null}))
            }
            "menu_select" => Ok(json!({"ok": true, "selected": "Alto", "triggerLabel": "Alto"})),
            "page_errors" => Ok(json!({"texts": []})),
            _ => Ok(json!({})),
        }
    }
}

fn text(value: &Value) -> ToolResult {
    ToolResult {
        text: serde_json::to_string(value).expect("serialize"),
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
        Box::pin(async move {
            let action = args["action"].as_str().unwrap_or("goto");
            match (tool, action) {
                ("browser_tabs", "list") => {
                    let tabs = self.tabs.lock().expect("tabs").clone();
                    Ok(text(&serde_json::to_value(tabs).expect("serialize")))
                }
                ("browser_tabs", "create") => {
                    let id = self.next_id.fetch_add(1, Ordering::SeqCst);
                    let url = args["url"].as_str().unwrap_or_default().to_string();
                    self.tabs.lock().expect("tabs").push(TabInfo {
                        id: Some(id),
                        title: Some("ChatGPT".to_string()),
                        url: Some(url.clone()),
                        active: false,
                        window_id: Some(2),
                    });
                    Ok(text(&json!({"id": id, "windowId": 2, "url": url})))
                }
                ("browser_tabs", "close") => {
                    let id = args["tabId"].as_i64();
                    self.tabs.lock().expect("tabs").retain(|t| t.id != id);
                    Ok(text(&json!({"closed": id})))
                }
                ("browser_tabs", "activate") => Ok(text(&json!({"active": args["tabId"]}))),
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
                ("browser_upload", _) => {
                    let selector = args["selector"].as_str().unwrap_or_default().to_string();
                    if self
                        .failing_upload_selectors
                        .lock()
                        .expect("failing")
                        .contains(&selector)
                    {
                        return Err(DriverError::tool(format!("No element matches {selector}")));
                    }
                    let names: Vec<String> = args["filePaths"]
                        .as_array()
                        .map(|paths| {
                            paths
                                .iter()
                                .filter_map(Value::as_str)
                                .map(|p| file_name_of(Path::new(p)))
                                .collect()
                        })
                        .unwrap_or_default();
                    self.uploaded.lock().expect("uploaded").extend(names);
                    Ok(text(&json!({"ok": true})))
                }
                _ => Ok(text(&json!({}))),
            }
        })
    }

    fn eval_in<'a>(
        &'a self,
        tab_id: TabId,
        expression: String,
        _timeout_ms: u64,
    ) -> BoxFuture<'a, DriverResult<Value>> {
        let kind = script_kind(&expression);
        self.evals
            .lock()
            .expect("evals")
            .push((kind, tab_id, expression));
        let scripted = self
            .scripted
            .lock()
            .expect("scripted")
            .get_mut(kind)
            .and_then(VecDeque::pop_front);
        let response = match scripted {
            Some(response) => response,
            None => match self.defaults.lock().expect("defaults").get(kind) {
                Some(response) => response.clone(),
                None => self.default_for(kind),
            },
        };
        Box::pin(async move { response })
    }
}

struct Harness {
    daemon: Arc<FakeDaemon>,
    ops: ChatGptOps,
    _dir: tempfile::TempDir,
}

fn harness() -> Harness {
    clear_models_cache("");
    let daemon = FakeDaemon::new();
    let dir = tempfile::tempdir().expect("tempdir");
    let as_daemon: Arc<dyn TabDaemon> = Arc::clone(&daemon) as Arc<dyn TabDaemon>;
    let tabs = Arc::new(TabPool::with_daemon_and_lock_options(
        Arc::clone(&as_daemon),
        TabPoolOptions {
            max_tabs: 1,
            idle_ms: 300_000,
            registry_path: dir.path().join("tabs.json"),
            base_url: BASE_URL.to_string(),
        },
        RegistryLockOptions {
            stale_after: Duration::from_secs(10),
            deadline: Duration::from_millis(500),
            poll: Duration::from_millis(10),
        },
    ));
    let ops = ChatGptOps::with_daemon(as_daemon, tabs, BASE_URL).with_timings(fast_timings());
    Harness {
        daemon,
        ops,
        _dir: dir,
    }
}

fn new_chat(text: &str, model: Option<ModelSpec>) -> SendRequest {
    SendRequest {
        conversation_id: None,
        text: text.to_string(),
        model,
        files: Vec::new(),
        mention: None,
        mention_strategy: MentionStrategy::default(),
    }
}

fn continuation(conversation_id: &str, text: &str) -> SendRequest {
    SendRequest {
        conversation_id: Some(conversation_id.to_string()),
        text: text.to_string(),
        model: None,
        files: Vec::new(),
        mention: None,
        mention_strategy: MentionStrategy::default(),
    }
}

// ---- model resolution ------------------------------------------------------

#[test]
fn resolve_model_leaves_the_account_default_for_auto_and_none() {
    let models = models_info();
    assert_eq!(
        resolve_model_with(None, &models).expect("none"),
        ResolvedModel::default()
    );
    assert_eq!(
        resolve_model_with(Some(&ModelSpec::Auto), &models).expect("auto"),
        ResolvedModel::default()
    );
}

#[test]
fn resolve_model_passes_an_exact_slug_through() {
    let resolved = resolve_model_with(
        Some(&ModelSpec::Slug("gpt-4-1".to_string())),
        &models_info(),
    )
    .expect("slug");
    assert_eq!(
        resolved,
        ResolvedModel {
            slug: Some("gpt-4-1".to_string()),
            menu_level: None,
            menu_index: None,
            expect_label: None,
        }
    );
}

#[test]
fn resolve_model_maps_instant_thinking_and_pro_to_family_slugs() {
    // The default slug is `gpt-5-6-thinking`; the family base is `gpt-5-6`.
    let models = models_info();
    assert_eq!(
        resolve_model_with(Some(&ModelSpec::Instant), &models).expect("instant"),
        ResolvedModel {
            slug: Some("gpt-5-6-instant".to_string()),
            menu_level: None,
            menu_index: None,
            expect_label: Some(level_spec("instant").expect("instant").loose()),
        }
    );
    assert_eq!(
        resolve_model_with(Some(&ModelSpec::Thinking), &models).expect("thinking"),
        ResolvedModel {
            slug: Some("gpt-5-6-thinking".to_string()),
            menu_level: None,
            menu_index: None,
            expect_label: None,
        }
    );
    assert_eq!(
        resolve_model_with(Some(&ModelSpec::Pro), &models).expect("pro"),
        ResolvedModel {
            slug: Some("gpt-5-6-pro".to_string()),
            menu_level: None,
            menu_index: None,
            expect_label: Some(level_spec("pro").expect("pro").loose()),
        }
    );
}

#[test]
fn resolve_model_picks_the_thinking_slug_plus_a_menu_level_for_effort_specs() {
    let models = models_info();
    for (spec, key, index) in [
        (ModelSpec::Medium, "medium", 2),
        (ModelSpec::High, "high", 3),
        (ModelSpec::ExtraHigh, "extra-high", 4),
    ] {
        let level = level_spec(key).expect("level");
        assert_eq!(
            resolve_model_with(Some(&spec), &models).expect("effort"),
            ResolvedModel {
                slug: Some("gpt-5-6-thinking".to_string()),
                // Selecting is anchored, verifying is not: the composer button
                // reads "GPT-5.6 Alta", not "Alta".
                menu_level: Some(level.anchored()),
                menu_index: Some(index),
                expect_label: Some(level.loose()),
            },
            "{spec:?}"
        );
    }
}

#[test]
fn resolve_model_falls_back_to_any_slug_with_the_suffix() {
    // Default family `gpt-5-7` has no instant slug; the only `-instant` wins.
    let models = ModelsInfo {
        default_slug: Some("gpt-5-7".to_string()),
        models: vec![
            ModelEntry {
                slug: "gpt-5-7".to_string(),
                title: String::new(),
            },
            ModelEntry {
                slug: "gpt-5-6-instant".to_string(),
                title: String::new(),
            },
        ],
    };
    let resolved = resolve_model_with(Some(&ModelSpec::Instant), &models).expect("instant");
    assert_eq!(resolved.slug.as_deref(), Some("gpt-5-6-instant"));
    // No pro slug anywhere: `slug: None` (account default), label still expected.
    let resolved = resolve_model_with(Some(&ModelSpec::Pro), &models).expect("pro");
    assert_eq!(resolved.slug, None);
    assert_eq!(
        resolved.expect_label,
        Some(level_spec("pro").expect("pro").loose())
    );
}

#[test]
fn resolve_model_rejects_an_unknown_name_listing_known_slugs() {
    let error = resolve_model_with(Some(&ModelSpec::Slug("gpt-9".to_string())), &models_info())
        .expect_err("unknown");
    assert_eq!(error.kind, DriverErrorKind::Other);
    assert!(
        error.message.contains("Unknown model 'gpt-9'"),
        "{}",
        error.message
    );
    assert!(error.message.contains("gpt-5-6-pro"), "{}", error.message);
}

#[test]
fn model_family_base_strips_known_suffixes() {
    assert_eq!(model_family_base(Some("gpt-5-6-thinking")), "gpt-5-6");
    assert_eq!(model_family_base(Some("gpt-5-6-instant")), "gpt-5-6");
    assert_eq!(model_family_base(Some("gpt-5-6-PRO")), "gpt-5-6");
    assert_eq!(model_family_base(Some("gpt-5-6-t-mini")), "gpt-5-6");
    assert_eq!(model_family_base(Some("gpt-5-6-mini")), "gpt-5-6");
    assert_eq!(model_family_base(Some("gpt-5-6")), "gpt-5-6");
    assert_eq!(model_family_base(None), "gpt-5-6");
}

#[test]
fn model_spec_parses_names_and_slugs() {
    assert_eq!(ModelSpec::parse(""), ModelSpec::Auto);
    assert_eq!(ModelSpec::parse("auto"), ModelSpec::Auto);
    assert_eq!(ModelSpec::parse(" instant "), ModelSpec::Instant);
    assert_eq!(ModelSpec::parse("extra-high"), ModelSpec::ExtraHigh);
    assert_eq!(
        ModelSpec::parse("gpt-5-6-pro"),
        ModelSpec::Slug("gpt-5-6-pro".to_string())
    );
    assert_eq!(ModelSpec::ExtraHigh.as_str(), "extra-high");
}

/// FORK: the picker's labels have moved once already (04/09: "Leve"/"Alta"),
/// so the table carries every spelling seen so far — and the *ordinal* is what
/// the selection is actually written against.
#[test]
fn level_labels_match_the_pt_and_en_menu_entries() {
    let selects = |key: &str, text: &str| {
        let spec = level_spec(key).expect("level");
        Regex::new(&format!("(?i){}", spec.anchored()))
            .expect("label regex")
            .is_match(text)
    };

    for text in ["Instantâneo", "Instantaneo", "Instant", "Leve"] {
        assert!(selects("instant", text), "{text}");
    }
    for text in ["Médio", "Media", "Medium"] {
        assert!(selects("medium", text), "{text}");
    }
    for text in ["Alto", "Alta", "High"] {
        assert!(selects("high", text), "{text}");
    }
    // Anchoring is what keeps "Alta" from also selecting "Extra alta".
    for text in ["Extra alto", "Extra alta", "Extra high"] {
        assert!(!selects("high", text), "{text}");
        assert!(selects("extra-high", text), "{text}");
    }
    assert!(selects("pro", "Pro"));
    assert!(!selects("pro", "Pro Max"));

    // The ordinals are the slider's own positions, 1..=5 in order.
    assert_eq!(level_spec("instant").expect("instant").index, 1);
    assert_eq!(level_spec("medium").expect("medium").index, 2);
    assert_eq!(level_spec("high").expect("high").index, 3);
    assert_eq!(level_spec("extra-high").expect("extra-high").index, 4);
    assert_eq!(level_spec("pro").expect("pro").index, 5);

    assert_eq!(
        level_label("high"),
        Some(level_spec("high").expect("high").anchored())
    );
    assert_eq!(level_label("thinking"), None);
    assert_eq!(level_spec("thinking"), None);
}

// ---- pure helpers ------------------------------------------------------------

#[test]
fn phase_error_tags_the_phase_and_the_landed_verdict() {
    let error = phase_error(
        DriverError::ui_changed("composer gone"),
        FailurePhase::Compose,
    );
    assert_eq!(error.kind, DriverErrorKind::UiChanged);
    assert_eq!(error.phase, Some(FailurePhase::Compose));
    assert_eq!(error.message_landed, Some(false));
    assert!(
        error
            .message
            .starts_with("[send phase: compose] composer gone | the message was NOT sent"),
        "{}",
        error.message
    );

    // Unknown verdict at submit → SubmitAmbiguous.
    let error = phase_error(DriverError::timeout("eval timed out"), FailurePhase::Submit);
    assert_eq!(error.kind, DriverErrorKind::SubmitAmbiguous);
    assert_eq!(error.message_landed, None);

    // A submit error that already knows nothing was clicked keeps its kind.
    let error = phase_error(
        DriverError::ui_changed("send button not found").landed(Some(false)),
        FailurePhase::Submit,
    );
    assert_eq!(error.kind, DriverErrorKind::UiChanged);
    assert_eq!(error.message_landed, Some(false));

    let error = phase_error(DriverError::other("read failed"), FailurePhase::Confirm);
    assert_eq!(error.message_landed, Some(true));
    assert_eq!(error.phase, Some(FailurePhase::Confirm));
}

#[test]
fn classify_page_error_maps_known_dialog_texts() {
    let classify = |text: &str| classify_page_error(&[text.to_string()]).map(|(kind, _)| kind);
    assert_eq!(
        classify("Too many requests. Please try again later."),
        Some(DriverErrorKind::RateLimited)
    );
    assert_eq!(
        classify("Muitas solicitações. Tente novamente mais tarde."),
        Some(DriverErrorKind::RateLimited)
    );
    assert_eq!(
        classify("You've reached the current usage limit for GPT-5.6 Pro."),
        Some(DriverErrorKind::RateLimited)
    );
    assert_eq!(
        classify(
            "The message you submitted was too long, please reload the conversation and submit something shorter."
        ),
        Some(DriverErrorKind::MessageTooLong)
    );
    assert_eq!(
        classify("A mensagem é muito longa."),
        Some(DriverErrorKind::MessageTooLong)
    );
    assert_eq!(
        classify("Something went wrong while generating the response."),
        Some(DriverErrorKind::Upstream)
    );
    assert_eq!(
        classify("Algo deu errado."),
        Some(DriverErrorKind::Upstream)
    );
    assert_eq!(
        classify("Your session has expired. Please log in again."),
        Some(DriverErrorKind::LoginRequired)
    );
    assert_eq!(classify("Você já carregou este arquivo."), None);
    assert_eq!(classify(""), None);
    assert_eq!(classify_page_error(&[]), None);
}

#[test]
fn conversation_id_is_extracted_from_the_page_url() {
    assert_eq!(
        conversation_id_from_url(&format!("{BASE_URL}/c/{NEW_ID}?model=x")),
        Some(NEW_ID.to_string())
    );
    assert_eq!(
        conversation_id_from_url(&format!("{BASE_URL}/?model=gpt")),
        None
    );
    assert_eq!(
        conversation_id_from_url("https://chatgpt.com/c/short"),
        None
    );
}

#[test]
fn split_already_uploaded_skips_files_with_the_same_name_and_size() {
    let dir = tempfile::tempdir().expect("tempdir");
    let notes = dir.path().join("notes.txt");
    let other = dir.path().join("other.txt");
    std::fs::write(&notes, b"twelve bytes").expect("write");
    std::fs::write(&other, b"different").expect("write");

    let mut conv = conversation("finished");
    let user_index = conv.last_user_turn_index().expect("user turn");
    conv.turns[user_index].assets.push(Asset {
        file_id: "file-1".to_string(),
        kind: AssetKind::File,
        name: Some("notes.txt".to_string()),
        width: None,
        height: None,
        size_bytes: Some(12),
    });

    let split = split_already_uploaded_with(&conv, &[notes.clone(), other.clone()]).expect("split");
    assert_eq!(split.fresh, vec![other.clone()]);
    assert_eq!(split.notes.len(), 1);
    assert!(
        split.notes[0].starts_with("'notes.txt' not re-uploaded"),
        "{}",
        split.notes[0]
    );

    // Same name, different size → uploaded again.
    std::fs::write(&notes, b"thirteen byte").expect("write");
    let split = split_already_uploaded_with(&conv, &[notes.clone(), other.clone()]).expect("split");
    assert_eq!(split.fresh, vec![notes, other]);
    assert!(split.notes.is_empty());

    let missing = dir.path().join("missing.txt");
    let error = split_already_uploaded_with(&conv, &[missing]).expect_err("missing file");
    assert!(
        error.message.starts_with("file not found:"),
        "{}",
        error.message
    );
}

#[test]
fn is_image_path_looks_at_the_extension_only() {
    assert!(is_image_path(Path::new("C:/tmp/pic.PNG")));
    assert!(is_image_path(Path::new("/tmp/photo.jpeg")));
    assert!(!is_image_path(Path::new("/tmp/doc.txt")));
    assert!(!is_image_path(Path::new("/tmp/noext")));
}

#[test]
fn file_names_are_sanitized_and_made_unique() {
    assert_eq!(sanitize_file_name("my file (1).png"), "my_file_1_.png");
    assert_eq!(
        pick_file_name(Some("gen image.png"), "file-1", "", ""),
        "gen_image.png"
    );
    assert_eq!(
        pick_file_name(
            None,
            "file-1",
            "image/png",
            "attachment; filename=\"a%20b.png\""
        ),
        "a_b.png"
    );
    assert_eq!(
        pick_file_name(None, "file-1", "image/webp", ""),
        "file-1.webp"
    );
    let dir = tempfile::tempdir().expect("tempdir");
    std::fs::write(dir.path().join("a.png"), b"x").expect("write");
    std::fs::write(dir.path().join("a-1.png"), b"x").expect("write");
    assert_eq!(unique_path(dir.path(), "a.png"), dir.path().join("a-2.png"));
    assert_eq!(unique_path(dir.path(), "b.png"), dir.path().join("b.png"));
}

#[test]
fn rename_regex_accepts_chatgpts_duplicate_suffix() {
    let re = rename_regex("report.v2.pdf").expect("regex");
    assert!(re.is_match("report.v2(1).pdf"));
    assert!(re.is_match("report.v2(12).pdf"));
    assert!(!re.is_match("report.v2.pdf"));
    assert!(!re.is_match("report-v2(1).pdf"));
}

// ---- reply completion rule ---------------------------------------------------

#[test]
fn check_reply_is_not_done_while_the_reply_is_in_progress() {
    let conv = conversation("in_progress");
    let check = check_reply(&conv, None, None);
    assert!(check.reply.is_some(), "the partial reply is visible");
    assert!(!check.idle);
    assert!(!check.done);
    // Even a stable fingerprint cannot complete a busy thread.
    let again = check_reply(&conv, None, Some(check.fingerprint));
    assert!(!again.done);
}

#[test]
fn check_reply_is_done_on_a_finished_end_turn_reply() {
    let conv = conversation("finished");
    let check = check_reply(&conv, None, None);
    assert!(check.idle);
    assert!(check.done);
    assert_eq!(
        check.reply.map(|r| r.text),
        Some("O arquivo contém três notas curtas sobre Rust.".to_string())
    );
}

#[test]
fn check_reply_is_not_blocked_by_an_old_stopped_generation() {
    let conv = conversation("stopped_old_in_progress");
    assert!(!conv.is_generating);
    let check = check_reply(&conv, None, None);
    assert!(check.idle);
    assert!(check.done);
    assert!(
        check
            .reply
            .is_some_and(|r| r.text.starts_with("Luz fria no cais"))
    );
}

#[test]
fn check_reply_requires_the_anchor_to_be_the_latest_user_turn() {
    let conv = conversation("finished");
    let stale = check_reply(&conv, Some("a message that was never sent"), None);
    assert_eq!(stale.reply, None);
    assert!(!stale.done);
    let anchored = check_reply(&conv, Some("  Resuma o arquivo anexo.  "), None);
    assert!(anchored.done);
}

#[test]
fn check_reply_completes_without_end_turn_once_the_fingerprint_is_stable() {
    let mut conv = conversation("finished");
    let last = conv.turns.len() - 1;
    conv.turns[last].end_turn = None;
    let first = check_reply(&conv, None, None);
    assert!(first.idle);
    assert!(!first.done, "one idle poll is not enough without end_turn");
    let second = check_reply(&conv, None, Some(first.fingerprint));
    assert!(second.done);
    // Growth between polls resets stability.
    conv.turns[last].text.push_str(" Mais.");
    let third = check_reply(&conv, None, Some(second.fingerprint));
    assert!(!third.done);
}

#[test]
fn check_reply_never_trusts_completion_while_an_async_run_is_active() {
    let mut conv = conversation("finished");
    conv.async_status = Some(1);
    let check = check_reply(&conv, None, None);
    assert!(!check.idle);
    assert!(!check.done);
    conv.async_status = Some(0);
    assert!(check_reply(&conv, None, None).done);
}

#[test]
fn check_reply_watches_asset_tool_turns() {
    let conv = conversation("image_assets");
    let check = check_reply(&conv, None, None);
    assert!(check.done);
    let reply = check.reply.expect("reply");
    assert_eq!(reply.role, "assistant");
    assert!(reply.end_turn == Some(true));
}

// ---- send phase machine (fake daemon + real pool) -------------------------------

#[tokio::test]
async fn send_new_chat_navigates_to_the_model_url_and_returns_the_conversation_id() {
    let h = harness();
    let sent = h
        .ops
        .send(new_chat("Reply with PONG.", Some(ModelSpec::Instant)))
        .await
        .expect("send");
    assert_eq!(sent.conversation_id, NEW_ID);
    assert_eq!(sent.phase_reached, FailurePhase::Confirm);
    assert_eq!(sent.model_label.as_deref(), Some("Instant"));
    assert_eq!(sent.notes, Vec::<String>::new());
    assert_eq!(
        h.daemon.navigations(),
        vec![format!("{BASE_URL}/?model=gpt-5-6-instant")]
    );
    let kinds = h.daemon.eval_kinds();
    let compose = kinds
        .iter()
        .position(|k| *k == "set_composer_text")
        .expect("compose");
    let submit = kinds
        .iter()
        .position(|k| *k == "click_send")
        .expect("submit");
    assert!(compose < submit);
    assert!(
        h.daemon.evals_of("set_composer_text")[0].contains("const TEXT = \"Reply with PONG.\";")
    );
    let info = h.ops.tabs().pool_info();
    assert_eq!(info.len(), 1);
    assert_eq!(info[0].conversation_id.as_deref(), Some(NEW_ID));
}

#[tokio::test]
async fn send_continuation_ignores_the_model_spec_with_a_note() {
    let h = harness();
    let mut request = continuation(FINISHED_ID, "Continue.");
    request.model = Some(ModelSpec::Pro);
    let sent = h.ops.send(request).await.expect("send");
    assert_eq!(sent.conversation_id, NEW_ID, "the click result's id wins");
    assert_eq!(
        sent.notes,
        vec!["model spec is ignored when continuing an existing conversation".to_string()]
    );
    assert_eq!(
        h.daemon.navigations(),
        vec![format!("{BASE_URL}/c/{FINISHED_ID}")]
    );
    assert!(
        h.daemon.evals_of("api_models").is_empty(),
        "no model lookup"
    );
}

#[tokio::test]
async fn send_attributes_a_compose_failure_to_the_compose_phase() {
    let h = harness();
    h.daemon.script(
        "set_composer_text",
        Ok(json!({"ok": false, "error": "composer (#prompt-textarea) not found"})),
    );
    let error = h
        .ops
        .send(new_chat("hello", None))
        .await
        .expect_err("compose must fail");
    assert_eq!(error.kind, DriverErrorKind::UiChanged);
    assert_eq!(error.phase, Some(FailurePhase::Compose));
    assert_eq!(error.message_landed, Some(false));
    assert!(
        error
            .message
            .starts_with("[send phase: compose] could not put the message"),
        "{}",
        error.message
    );
    assert!(
        h.daemon.evals_of("click_send").is_empty(),
        "nothing was clicked"
    );
}

#[tokio::test]
async fn send_with_an_ambiguous_submit_and_no_api_verdict_is_submit_ambiguous() {
    let h = harness();
    h.daemon.script(
        "click_send",
        Err(DriverError::timeout(
            "chrome-mcp browser_eval did not answer within 120s",
        )),
    );
    h.daemon.set_default(
        "api_conversation",
        Err(DriverError::daemon_down(
            "Cannot reach the chrome-mcp daemon",
        )),
    );
    let error = h
        .ops
        .send(continuation(FINISHED_ID, "Resuma o arquivo anexo."))
        .await
        .expect_err("ambiguous");
    assert_eq!(error.kind, DriverErrorKind::SubmitAmbiguous);
    assert_eq!(error.phase, Some(FailurePhase::Submit));
    assert_eq!(error.message_landed, None);
    assert!(error.message.contains("AMBIGUOUS"), "{}", error.message);
}

#[tokio::test]
async fn send_with_an_ambiguous_submit_confirmed_by_the_api_succeeds_with_a_note() {
    let h = harness();
    h.daemon.script(
        "click_send",
        Err(DriverError::timeout(
            "chrome-mcp browser_eval did not answer within 120s",
        )),
    );
    // The fixture's latest user turn is exactly this text.
    let sent = h
        .ops
        .send(continuation(FINISHED_ID, "Resuma o arquivo anexo."))
        .await
        .expect("confirmed");
    assert_eq!(sent.conversation_id, FINISHED_ID);
    assert_eq!(sent.model_label, None);
    assert_eq!(sent.notes.len(), 1);
    assert!(
        sent.notes[0].contains("WAS confirmed sent"),
        "{}",
        sent.notes[0]
    );
}

#[tokio::test]
async fn send_with_an_ambiguous_submit_denied_by_the_api_is_not_landed() {
    let h = harness();
    h.daemon.script(
        "click_send",
        Err(DriverError::timeout(
            "chrome-mcp browser_eval did not answer within 120s",
        )),
    );
    let error = h
        .ops
        .send(continuation(FINISHED_ID, "a brand new message"))
        .await
        .expect_err("denied");
    assert_eq!(error.kind, DriverErrorKind::Timeout, "original kind kept");
    assert_eq!(error.phase, Some(FailurePhase::Submit));
    assert_eq!(error.message_landed, Some(false));
    assert!(
        error
            .message
            .contains("API check: the message is NOT in the conversation"),
        "{}",
        error.message
    );
}

#[tokio::test]
async fn send_with_a_missing_send_button_retypes_once_then_fails_with_nothing_clicked() {
    let h = harness();
    let not_found =
        json!({"ok": false, "error": "send button not found — composer empty or UI changed"});
    h.daemon.script("click_send", Ok(not_found.clone()));
    h.daemon.script("click_send", Ok(not_found));
    let error = h
        .ops
        .send(new_chat("hello", None))
        .await
        .expect_err("send button missing");
    assert_eq!(error.kind, DriverErrorKind::UiChanged);
    assert_eq!(error.phase, Some(FailurePhase::Submit));
    assert_eq!(error.message_landed, Some(false));
    assert_eq!(
        h.daemon.evals_of("set_composer_text").len(),
        2,
        "retyped once"
    );
    assert_eq!(h.daemon.evals_of("click_send").len(), 2);
}

#[tokio::test]
async fn send_refuses_while_the_conversation_is_generating() {
    let h = harness();
    h.daemon.set_default(
        "composer_state",
        Ok(json!({
            "hasComposer": true, "url": format!("{BASE_URL}/c/{FINISHED_ID}"), "modelLabel": "Pro",
            "sendVisible": false, "sendEnabled": false, "generating": true, "attachments": 0, "text": ""
        })),
    );
    h.daemon.set_default(
        "api_conversation",
        Ok(json!({"status": 200, "json": fixture("in_progress"), "text": null})),
    );
    let error = h
        .ops
        .send(continuation(FINISHED_ID, "hello"))
        .await
        .expect_err("busy");
    assert_eq!(error.kind, DriverErrorKind::Busy);
    assert_eq!(error.phase, Some(FailurePhase::Precheck));
    assert_eq!(error.message_landed, Some(false));
    assert!(h.daemon.evals_of("set_composer_text").is_empty());
}

#[tokio::test]
async fn send_reloads_a_stuck_tab_when_the_api_says_idle() {
    let h = harness();
    // The stop button is stuck on screen, but the API says the thread is idle.
    h.daemon.script(
        "composer_state",
        Ok(json!({
            "hasComposer": true, "url": format!("{BASE_URL}/c/{FINISHED_ID}"), "modelLabel": "Pro",
            "sendVisible": false, "sendEnabled": false, "generating": true, "attachments": 0, "text": ""
        })),
    );
    let sent = h
        .ops
        .send(continuation(FINISHED_ID, "hello"))
        .await
        .expect("sent after reload");
    assert_eq!(sent.conversation_id, NEW_ID);
    assert_eq!(
        h.daemon.navigations(),
        vec![
            format!("{BASE_URL}/c/{FINISHED_ID}"),
            format!("{BASE_URL}/c/{FINISHED_ID}")
        ],
        "show + reload"
    );
}

#[tokio::test]
async fn send_classifies_a_rate_limit_dialog_when_the_click_goes_nowhere() {
    let h = harness();
    h.daemon.script(
        "click_send",
        Ok(json!({"ok": false, "conversationId": null, "generating": false, "error": "send did not start"})),
    );
    h.daemon.script(
        "page_errors",
        Ok(json!({"texts": ["Too many requests. Please try again later."]})),
    );
    let error = h
        .ops
        .send(new_chat("hello", None))
        .await
        .expect_err("rate limited");
    assert_eq!(error.kind, DriverErrorKind::RateLimited);
    assert_eq!(error.phase, Some(FailurePhase::Submit));
    assert_eq!(error.message_landed, Some(false));
}

#[tokio::test]
async fn send_without_a_stop_button_on_a_continuation_is_verified_against_the_api() {
    let h = harness();
    // Existing conversation: the URL already has /c/, so `ok` is true even
    // though the stop button never showed.
    h.daemon.script(
        "click_send",
        Ok(json!({"ok": true, "conversationId": FINISHED_ID, "generating": false})),
    );
    let error = h
        .ops
        .send(continuation(FINISHED_ID, "never landed"))
        .await
        .expect_err("not landed");
    assert_eq!(error.kind, DriverErrorKind::UiChanged);
    assert_eq!(error.phase, Some(FailurePhase::Confirm));
    assert_eq!(error.message_landed, Some(false));

    // ...while a message the API does show as latest is fine.
    h.daemon.script(
        "click_send",
        Ok(json!({"ok": true, "conversationId": FINISHED_ID, "generating": false})),
    );
    let sent = h
        .ops
        .send(continuation(FINISHED_ID, "Resuma o arquivo anexo."))
        .await
        .expect("landed");
    assert_eq!(sent.conversation_id, FINISHED_ID);
}

#[tokio::test]
async fn send_new_chat_without_a_url_flip_is_a_landed_error() {
    let h = harness();
    h.daemon.script(
        "click_send",
        Ok(json!({"ok": true, "conversationId": null, "generating": true, "url": format!("{BASE_URL}/")})),
    );
    let error = h
        .ops
        .send(new_chat("hello", None))
        .await
        .expect_err("no id");
    assert_eq!(error.kind, DriverErrorKind::UiChanged);
    assert_eq!(error.phase, Some(FailurePhase::Confirm));
    assert_eq!(error.message_landed, Some(true));
}

#[tokio::test]
async fn send_with_files_uploads_images_and_documents_on_their_inputs() {
    let h = harness();
    let dir = tempfile::tempdir().expect("tempdir");
    let pic = dir.path().join("pic.png");
    let doc = dir.path().join("doc.txt");
    std::fs::write(&pic, b"\x89PNG\r\n\x1a\n").expect("write");
    std::fs::write(&doc, b"hello").expect("write");
    // The photos input is missing on this page: the image falls back to the
    // generic file input.
    h.daemon
        .failing_upload_selectors
        .lock()
        .expect("failing")
        .insert(r#"input[data-testid="upload-photos-input"]"#.to_string());

    let sent = h
        .ops
        .send(SendRequest {
            conversation_id: None,
            text: "What is this?".to_string(),
            model: None,
            files: vec![pic.clone(), doc.clone()],
            mention: None,
            mention_strategy: MentionStrategy::default(),
        })
        .await
        .expect("send with files");
    assert_eq!(sent.conversation_id, NEW_ID);
    assert_eq!(sent.notes, Vec::<String>::new());

    let uploads = h.daemon.calls_named("browser_upload");
    let described: Vec<(String, Vec<String>)> = uploads
        .iter()
        .map(|args| {
            (
                args["selector"].as_str().unwrap_or_default().to_string(),
                args["filePaths"]
                    .as_array()
                    .map(|paths| {
                        paths
                            .iter()
                            .filter_map(Value::as_str)
                            .map(|p| file_name_of(Path::new(p)))
                            .collect()
                    })
                    .unwrap_or_default(),
            )
        })
        .collect();
    assert_eq!(
        described,
        vec![
            (
                r#"input[data-testid="upload-photos-input"]"#.to_string(),
                vec!["pic.png".to_string()]
            ),
            (
                r#"form input[type="file"]:not([accept*="image"])"#.to_string(),
                vec!["pic.png".to_string()]
            ),
            (
                r#"form input[type="file"]:not([accept*="image"])"#.to_string(),
                vec!["doc.txt".to_string()]
            ),
        ]
    );
    assert_eq!(uploads[0]["timeoutMs"], json!(UPLOAD_TIMEOUT_MS));
    let paths: Vec<&str> = uploads[2]["filePaths"]
        .as_array()
        .expect("paths")
        .iter()
        .filter_map(Value::as_str)
        .collect();
    assert!(Path::new(paths[0]).is_absolute(), "{}", paths[0]);
}

#[tokio::test]
async fn send_fails_when_a_file_never_gets_a_tile() {
    let h = harness();
    let dir = tempfile::tempdir().expect("tempdir");
    let doc = dir.path().join("doc.txt");
    std::fs::write(&doc, b"hello").expect("write");
    // Tiles never show the file (silently dropped by ChatGPT).
    h.daemon.set_default(
        "attachment_tiles",
        Ok(json!({"tiles": ["other.txt"], "legacy": 0})),
    );
    h.daemon.script(
        "dismiss_upload_dialog",
        Ok(json!({"found": true, "text": "Você já carregou este arquivo.", "dismissed": true})),
    );
    let error = h
        .ops
        .send(SendRequest {
            conversation_id: None,
            text: "Read it".to_string(),
            model: None,
            files: vec![doc],
            mention: None,
            mention_strategy: MentionStrategy::default(),
        })
        .await
        .expect_err("missing tile");
    assert_eq!(error.phase, Some(FailurePhase::Upload));
    assert_eq!(error.message_landed, Some(false));
    assert!(
        error.message.contains(
            "'doc.txt' did not attach — ChatGPT blocked it with: \"Você já carregou este arquivo.\""
        ),
        "{}",
        error.message
    );
    assert!(h.daemon.evals_of("set_composer_text").is_empty());
}

#[tokio::test]
async fn send_reports_a_missing_file_before_touching_the_page() {
    let h = harness();
    let error = h
        .ops
        .send(SendRequest {
            conversation_id: None,
            text: "Read it".to_string(),
            model: None,
            files: vec![PathBuf::from("Z:/definitely/missing.txt")],
            mention: None,
            mention_strategy: MentionStrategy::default(),
        })
        .await
        .expect_err("missing file");
    assert_eq!(error.phase, Some(FailurePhase::Upload));
    assert_eq!(error.message_landed, Some(false));
    assert!(h.daemon.calls_named("browser_upload").is_empty());
}

#[tokio::test]
async fn send_selects_the_level_via_the_menu_for_effort_specs() {
    let h = harness();
    let sent = h
        .ops
        .send(new_chat("hello", Some(ModelSpec::High)))
        .await
        .expect("send");
    assert_eq!(sent.conversation_id, NEW_ID);
    assert_eq!(
        h.daemon.navigations(),
        vec![format!("{BASE_URL}/?model=gpt-5-6-thinking")]
    );
    let menu = h.daemon.evals_of("menu_select");
    assert_eq!(menu.len(), 1);
    // FORK: the label table carries every spelling the picker has used, and
    // the ordinal is what the script actually navigates by.
    assert!(
        menu[0].contains("const TARGET = new RegExp(\"^Alto$|^Alta$|^High$\", 'i');"),
        "{}",
        menu[0]
    );
    assert!(menu[0].contains("const INDEX = 3;"), "{}", menu[0]);
    // with_activated_on: activate → menu → reload.
    let activations = h
        .daemon
        .calls_named("browser_tabs")
        .into_iter()
        .filter(|args| args["action"] == "activate")
        .count();
    assert_eq!(activations, 1);
    let reloads = h
        .daemon
        .calls_named("browser_navigate")
        .into_iter()
        .filter(|args| args["action"] == "reload")
        .count();
    assert_eq!(reloads, 1);
    // The picker reports what it selected, and neither the label "Instant" from
    // the fake composer nor any pill matches the requested level.
    assert_eq!(sent.notes.len(), 2, "{:?}", sent.notes);
    assert!(
        sent.notes[0].contains("effort level set through the picker"),
        "{}",
        sent.notes[0]
    );
    assert!(
        sent.notes[1].contains("Instant"),
        "{}",
        sent.notes[1]
    );
}

#[tokio::test]
async fn stop_clicks_until_the_stop_button_goes_away() {
    let h = harness();
    h.daemon.script(
        "click_stop",
        Ok(json!({"ok": false, "error": "no stop button — nothing generating in this tab"})),
    );
    let outcome = h.ops.stop(Some(FINISHED_ID)).await.expect("stop");
    assert_eq!(
        outcome,
        StopOutcome {
            ok: true,
            detail: "generation stopped".to_string()
        }
    );
    assert_eq!(h.daemon.evals_of("click_stop").len(), 2);
    assert_eq!(
        h.daemon.navigations(),
        vec![format!("{BASE_URL}/c/{FINISHED_ID}")]
    );
    assert_eq!(
        h.daemon.tab_url(500).as_deref(),
        Some(format!("{BASE_URL}/c/{FINISHED_ID}").as_str())
    );
}

#[tokio::test]
async fn stop_gives_up_after_the_window_with_the_last_error() {
    let h = harness();
    h.daemon.set_default(
        "click_stop",
        Ok(json!({"ok": false, "error": "no stop button — nothing generating in this tab"})),
    );
    let outcome = h.ops.stop(None).await.expect("stop");
    assert!(!outcome.ok);
    assert_eq!(
        outcome.detail,
        "no stop button — nothing generating in this tab"
    );
}

#[tokio::test]
async fn wait_reply_completes_on_a_finished_reply() {
    let h = harness();
    let waited = h
        .ops
        .wait_reply(
            FINISHED_ID,
            Duration::from_secs(5),
            Some("Resuma o arquivo anexo."),
        )
        .await
        .expect("wait");
    assert_eq!(waited.status, ReplyStatus::Complete);
    assert_eq!(
        waited.reply_text.as_deref(),
        Some("O arquivo contém três notas curtas sobre Rust.")
    );
    assert!(waited.conversation.is_some());
    assert_eq!(h.daemon.evals_of("api_conversation").len(), 1);
}

#[tokio::test]
async fn wait_reply_reports_generating_when_the_wait_runs_out() {
    let h = harness();
    h.daemon.set_default(
        "api_conversation",
        Ok(json!({"status": 200, "json": fixture("in_progress"), "text": null})),
    );
    let waited = h
        .ops
        .wait_reply("11111111-aaaa-4bbb-8ccc-000000000001", Duration::ZERO, None)
        .await
        .expect("wait");
    assert_eq!(waited.status, ReplyStatus::Generating);
    assert!(waited.reply_text.is_some_and(|t| t.starts_with("Um mutex")));
    assert!(waited.note.is_some());
}

#[tokio::test]
async fn resolve_model_fetches_the_model_list_through_a_read_tab() {
    let h = harness();
    let resolved = h
        .ops
        .resolve_model(Some(&ModelSpec::Pro))
        .await
        .expect("pro");
    assert_eq!(resolved.slug.as_deref(), Some("gpt-5-6-pro"));
    // (The process-wide models cache may already be warm from a sibling test,
    // so the fetch count is not asserted.) Auto never fetches:
    let before = h.daemon.evals_of("api_models").len();
    let auto = h
        .ops
        .resolve_model(Some(&ModelSpec::Auto))
        .await
        .expect("auto");
    assert_eq!(auto, ResolvedModel::default());
    assert_eq!(h.daemon.evals_of("api_models").len(), before);
}

// ---- live (chrome-mcp daemon + logged-in chatgpt.com) ------------------------------
//
// Run with:
//   RUST_MIN_STACK=8388608 cargo test -p codex-core --lib chatgpt_web::driver::ops -- --ignored --test-threads=1
// Every test hides what it created (`is_visible: false`) and closes its tab.

#[expect(
    clippy::print_stderr,
    reason = "live diagnostics (ids, timings, notes) are read from the terminal with --nocapture"
)]
mod live {
    use super::*;
    use crate::chatgpt_web::driver::daemon::DEFAULT_DAEMON_URL;
    use crate::chatgpt_web::driver::daemon::DaemonConfig;
    use crate::chatgpt_web::driver::tabs::default_registry_path;
    use codex_exec_server::HttpClient;

    struct Live {
        daemon: Arc<DaemonClient>,
        ops: ChatGptOps,
        created: StdMutex<Vec<String>>,
    }

    impl Live {
        async fn start() -> Self {
            use codex_exec_server::RouteAwareHttpClient;
            use codex_http_client::HttpClientFactory;
            use codex_http_client::OutboundProxyPolicy;

            let http_client: Arc<dyn HttpClient> = Arc::new(
                RouteAwareHttpClient::new(HttpClientFactory::new(
                    OutboundProxyPolicy::ReqwestDefault,
                ))
                .with_tls_backend_fallback(),
            );
            let daemon = Arc::new(DaemonClient::new(
                DaemonConfig::resolve(DEFAULT_DAEMON_URL, None),
                http_client,
            ));
            let health = daemon.health().await.expect("chrome-mcp daemon must be up");
            assert!(health.extension_connected, "Chrome extension not connected");
            let tabs = Arc::new(TabPool::new(
                Arc::clone(&daemon),
                TabPoolOptions {
                    max_tabs: 1,
                    idle_ms: 300_000,
                    registry_path: default_registry_path().expect("home dir"),
                    base_url: BASE_URL.to_string(),
                },
            ));
            let ops = ChatGptOps::new(Arc::clone(&daemon), tabs, BASE_URL);
            Self {
                daemon,
                ops,
                created: StdMutex::new(Vec::new()),
            }
        }

        fn track(&self, conversation_id: &str) {
            let mut created = self.created.lock().expect("created");
            if !created.iter().any(|id| id == conversation_id) {
                created.push(conversation_id.to_string());
            }
        }

        /// Hide every conversation the test created, close our tab, end the
        /// daemon session. Runs whether the test body passed or not.
        async fn finish(self) {
            let created = self.created.lock().expect("created").clone();
            for id in created {
                match self.ops.api_for(Some(&id)).await {
                    Ok(api) => {
                        if let Err(error) = api
                            .patch_conversation(&id, json!({"is_visible": false}))
                            .await
                        {
                            eprintln!("cleanup: hiding {id} failed: {error}");
                        }
                    }
                    Err(error) => eprintln!("cleanup: no tab to hide {id}: {error}"),
                }
            }
            self.ops.tabs().shutdown().await;
            self.daemon.shutdown().await;
        }
    }

    async fn send_and_wait(
        live: &Live,
        request: SendRequest,
        wait: Duration,
    ) -> Result<(Sent, ReplyWait), String> {
        let text = request.text.clone();
        let started = Instant::now();
        let sent = live.ops.send(request).await.map_err(|e| {
            format!(
                "send failed: {e} (kind {:?}, landed {:?})",
                e.kind, e.message_landed
            )
        })?;
        live.track(&sent.conversation_id);
        eprintln!(
            "[live] sent to {} in {:?} (label {:?}, notes {:?})",
            sent.conversation_id,
            started.elapsed(),
            sent.model_label,
            sent.notes
        );
        let waited = live
            .ops
            .wait_reply(&sent.conversation_id, wait, Some(&text))
            .await
            .map_err(|e| format!("wait_reply failed: {e}"))?;
        eprintln!(
            "[live] reply {:?} after {:?}: {:?}",
            waited.status,
            waited.elapsed,
            waited
                .reply_text
                .as_deref()
                .map(|t| t.chars().take(120).collect::<String>())
        );
        Ok((sent, waited))
    }

    #[tokio::test]
    #[ignore]
    async fn live_new_chat_instant_reply() {
        let live = Live::start().await;
        let outcome = async {
            let (sent, waited) = send_and_wait(
                &live,
                new_chat("Reply with the single word PONG.", Some(ModelSpec::Instant)),
                Duration::from_secs(120),
            )
            .await?;
            if waited.status != ReplyStatus::Complete {
                return Err(format!("reply did not complete: {waited:?}"));
            }
            let text = waited.reply_text.unwrap_or_default();
            if !text.to_uppercase().contains("PONG") {
                return Err(format!("reply does not contain PONG: {text}"));
            }
            let instant = level_spec("instant").expect("instant").loose();
            if let Some(label) = sent.model_label.as_deref()
                && !Regex::new(&format!("(?i){instant}"))
                    .expect("regex")
                    .is_match(label)
            {
                return Err(format!("composer label was {label}, expected Instant"));
            }
            Ok(())
        }
        .await;
        live.finish().await;
        outcome.expect("live_new_chat_instant_reply");
    }

    #[tokio::test]
    #[ignore]
    async fn live_continue_conversation() {
        let live = Live::start().await;
        let outcome = async {
            let (sent, waited) = send_and_wait(
                &live,
                new_chat(
                    "The code word is ORANGE. Reply with the single word OK.",
                    Some(ModelSpec::Instant),
                ),
                Duration::from_secs(120),
            )
            .await?;
            if waited.status != ReplyStatus::Complete {
                return Err(format!("first reply did not complete: {waited:?}"));
            }
            let (_, waited) = send_and_wait(
                &live,
                continuation(
                    &sent.conversation_id,
                    "What was the code word? Reply with that single word.",
                ),
                Duration::from_secs(120),
            )
            .await?;
            if waited.status != ReplyStatus::Complete {
                return Err(format!("second reply did not complete: {waited:?}"));
            }
            let text = waited.reply_text.unwrap_or_default();
            if !text.to_uppercase().contains("ORANGE") {
                return Err(format!("reply does not contain ORANGE: {text}"));
            }
            let conv = waited.conversation.expect("conversation");
            let users = conv.turns.iter().filter(|t| t.role == "user").count();
            if users != 2 {
                return Err(format!("expected 2 user turns, got {users}"));
            }
            Ok(())
        }
        .await;
        live.finish().await;
        outcome.expect("live_continue_conversation");
    }

    #[tokio::test]
    #[ignore]
    async fn live_upload_and_reply() {
        let live = Live::start().await;
        let dir = tempfile::tempdir().expect("tempdir");
        // ChatGPT dedupes uploads account-wide by content (a repeat opens the
        // "already uploaded" popup, which the port absorbs) — vary the shade
        // per run so the test exercises the plain path.
        let seed = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.subsec_millis() as u8)
            .unwrap_or(0);
        let png = dir.path().join("codex-live-red.png");
        image::RgbImage::from_pixel(48, 48, image::Rgb([200 + seed % 50, seed % 30, seed % 20]))
            .save(&png)
            .expect("write png");
        let outcome = async {
            let (sent, waited) = send_and_wait(
                &live,
                SendRequest {
                    conversation_id: None,
                    text: "What color is this image? One word.".to_string(),
                    model: Some(ModelSpec::Instant),
                    files: vec![png.clone()],
                    mention: None,
                    mention_strategy: MentionStrategy::default(),
                },
                Duration::from_secs(180),
            )
            .await?;
            if waited.status != ReplyStatus::Complete {
                return Err(format!("reply did not complete: {waited:?}"));
            }
            let text = waited.reply_text.unwrap_or_default().to_lowercase();
            if !(text.contains("red") || text.contains("vermelh")) {
                return Err(format!("reply does not name red: {text}"));
            }
            let conv = waited.conversation.expect("conversation");
            let user = &conv.turns[conv.last_user_turn_index().expect("user")];
            if user.assets.is_empty() {
                return Err(format!(
                    "the user turn carries no asset: {user:?} (notes {:?})",
                    sent.notes
                ));
            }
            Ok(())
        }
        .await;
        live.finish().await;
        outcome.expect("live_upload_and_reply");
    }

    #[tokio::test]
    #[ignore]
    async fn live_stop() {
        let live = Live::start().await;
        let outcome = async {
            let sent = live
                .ops
                .send(new_chat(
                    "Write a very long, detailed essay (at least 2000 words) about the history of the Roman Empire.",
                    Some(ModelSpec::Thinking),
                ))
                .await
                .map_err(|e| format!("send failed: {e}"))?;
            live.track(&sent.conversation_id);
            tokio::time::sleep(Duration::from_secs(3)).await;
            let stopped = live
                .ops
                .stop(Some(&sent.conversation_id))
                .await
                .map_err(|e| format!("stop failed: {e}"))?;
            eprintln!("[live] stop → {stopped:?}");
            // The API must go idle within a short while either way.
            let deadline = Instant::now() + Duration::from_secs(60);
            loop {
                match live.ops.read_conversation(&sent.conversation_id, false).await {
                    Ok(conv) if api_idle(&conv) => {
                        eprintln!(
                            "[live] idle after stop; reply so far: {:?}",
                            last_assistant_reply(&conv).map(|r| r.text.chars().take(80).collect::<String>())
                        );
                        return Ok(());
                    }
                    Ok(_) => {}
                    Err(error) => eprintln!("[live] read after stop failed: {error}"),
                }
                if Instant::now() > deadline {
                    return Err("conversation still generating 60s after stop".to_string());
                }
                tokio::time::sleep(Duration::from_millis(2500)).await;
            }
        }
        .await;
        live.finish().await;
        outcome.expect("live_stop");
    }

    #[tokio::test]
    #[ignore]
    async fn live_pro_resolves() {
        let live = Live::start().await;
        let outcome = async {
            let resolved = live
                .ops
                .resolve_model(Some(&ModelSpec::Pro))
                .await
                .map_err(|e| format!("resolve failed: {e}"))?;
            eprintln!("[live] pro → {resolved:?}");
            match resolved.slug.as_deref() {
                Some(slug) if slug.ends_with("-pro") => Ok(()),
                other => Err(format!("expected a -pro slug, got {other:?}")),
            }
        }
        .await;
        live.finish().await;
        outcome.expect("live_pro_resolves");
    }
}

// ---------------------------------------------------------------------------
// FORK: the effort picker is an ordinal, and the labels only report it back.

/// The picker's labels moved on 04/09 ("Leve", "Alta"), and every
/// `medium|high|extra-high` selection silently fell back to the account
/// default. The `<n> de 5` ordinal is the part that does not move.
#[test]
fn an_unknown_picker_label_is_reported_but_the_ordinal_selection_stands() {
    // What the page script answers when the slider landed on stop 3 but the
    // label there is not one Codex knows.
    let selection: MenuSelection = serde_json::from_value(serde_json::json!({
        "ok": true,
        "selected": "Alta",
        "index": 3,
        "total": 5,
        "labelMatched": false,
        "triggerLabel": "GPT-5.6 Alta",
        "slider": true,
    }))
    .expect("decodes");

    assert!(selection.ok, "the ordinal selection still succeeded");
    assert_eq!(selection.index, Some(3));
    assert_eq!(selection.total, Some(5));
    assert_eq!(selection.label_matched, Some(false));
    assert_eq!(selection.selected.as_deref(), Some("Alta"));
}

#[test]
fn a_slider_selection_decodes_its_position() {
    let selection: MenuSelection = serde_json::from_value(serde_json::json!({
        "ok": true,
        "selected": "Alta",
        "index": 3,
        "total": 5,
        "labelMatched": true,
    }))
    .expect("decodes");
    assert_eq!(
        (selection.index, selection.total, selection.label_matched),
        (Some(3), Some(5), Some(true))
    );

    // A failure carries what the walk saw, so the note can say what was there.
    let failed: MenuSelection = serde_json::from_value(serde_json::json!({
        "ok": false,
        "error": "option not found",
        "available": ["Leve (1)", "Média (2)", "Alta (3)"],
        "slider": true,
    }))
    .expect("decodes");
    assert!(!failed.ok);
    assert_eq!(failed.available.len(), 3);
    assert_eq!(failed.index, None);
}

/// FORK: the level moved into its own composer pill, so a check that only
/// looked at the model button reported a mismatch for a level that had in fact
/// been applied.
#[test]
fn the_level_is_verified_against_any_composer_pill() {
    let state: page_scripts::ComposerState = serde_json::from_value(serde_json::json!({
        "hasComposer": true,
        "url": "https://chatgpt.com/",
        "modelLabel": "ChatGPT 5.6 Thinking",
        "pills": ["Potência: Alta", "Busca"],
        "sendVisible": true,
        "sendEnabled": true,
        "generating": false,
        "attachments": 0,
    }))
    .expect("decodes");

    let expect = level_spec("high").expect("high").loose();
    let re = Regex::new(&format!("(?i){expect}")).expect("regex");
    assert!(
        !re.is_match(state.model_label.as_deref().unwrap_or("")),
        "the model button alone does not carry the level"
    );
    assert!(
        state.pills.iter().any(|pill| re.is_match(pill)),
        "the level is in a pill: {:?}",
        state.pills
    );
}
