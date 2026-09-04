//! FORK: port of `chatgpt-pro-mcp/src/ops.ts` (`class ChatGptOps`) — the
//! operations layer: model selection, the send phase machine, uploads,
//! reply polling, stop and asset downloads.
//!
//! Everything that touches a tab runs inside `TabPool::with_tab_for` (per-tab
//! FIFO, conversation affinity); everything that only reads the backend goes
//! through `ChatGptApi` on whichever tab `TabPool::eval_tab_id` hands out.
//!
//! Error contract (the plan's "Mapeamento de erros" table): every error that
//! leaves [`ChatGptOps::send`] carries the [`FailurePhase`] it happened in and
//! a `message_landed` verdict — `Some(false)` before the click, `Some(true)`
//! once the page confirmed the send, `None` only when a submit could not be
//! settled either way (then the kind is `SubmitAmbiguous`, never a blind
//! resend).

// TODO(M4/M5): `wait_reply`, `download_assets`, `discover_menu` and the model
// spec parser are consumed by the provider/attachments layers and the live
// tests; drop once everything is wired in.
#![allow(dead_code)]

use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Mutex as StdMutex;
use std::sync::OnceLock;
use std::sync::PoisonError;
use std::time::Duration;
use std::time::Instant;

use base64::Engine;
use futures::future::BoxFuture;
use regex_lite::Regex;
use serde::Deserialize;
use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::Value;
use serde_json::json;
use tracing::info;
use tracing::warn;

use super::DriverError;
use super::DriverErrorKind;
use super::DriverResult;
use super::FailurePhase;
use super::api::AssetKind;
use super::api::ChatGptApi;
use super::api::Conversation;
use super::api::ModelsInfo;
use super::api::NormalizeOptions;
use super::api::PageEval;
use super::api::Turn;
use super::api::fingerprint;
use super::api::normalize_with;
use super::daemon::DEFAULT_TOOL_TIMEOUT_MS;
use super::daemon::DaemonClient;
use super::page_scripts;
use super::page_scripts::ComposerState;
use super::page_scripts::DomProgress;
use super::page_scripts::MenuKind;
use super::tabs::TabDaemon;
use super::tabs::TabId;
use super::tabs::TabPool;

// ---------------------------------------------------------------------------
// Constants (`ops.ts:49-60`)
// ---------------------------------------------------------------------------

// Transport caps for daemon calls that can legitimately run long. Hidden pool
// tabs throttle page timers (down to one tick per MINUTE after ~5min occluded),
// so evals that wait on in-page setTimeout/setInterval need far more transport
// headroom than their nominal page-side deadline. The extension also keeps
// executing an eval after the daemon's cap fires, so a cap that is too tight
// yields "failed" sends that actually landed.
/// `setComposerText`: two page-side sleeps.
pub(crate) const EVAL_COMPOSE_TIMEOUT_MS: u64 = 150_000;
/// `clickSend`: 12s page-side poll.
pub(crate) const EVAL_SUBMIT_TIMEOUT_MS: u64 = 120_000;
/// `clickStop`: 8s page-side poll.
pub(crate) const EVAL_STOP_TIMEOUT_MS: u64 = 90_000;
/// `browser_upload`: extension reads + sets the files.
pub(crate) const UPLOAD_TIMEOUT_MS: u64 = 120_000;
/// In-page fetch of an asset.
pub(crate) const DOWNLOAD_STAGE_TIMEOUT_MS: u64 = 180_000;
/// 4MB base64 hop out of the page.
pub(crate) const DOWNLOAD_CHUNK_TIMEOUT_MS: u64 = 60_000;
/// `page.clickSend(12_000)`: page-side wait for the stop button / URL flip.
pub(crate) const CLICK_SEND_PAGE_TIMEOUT_MS: u64 = 12_000;
/// `readDownloadChunk` slice size (`const CHUNK = 4_000_000`).
const DOWNLOAD_CHUNK_B64: u64 = 4_000_000;

/// `waitReply` poll interval (`await sleep(2500)`).
pub(crate) const REPLY_POLL_INTERVAL: Duration = Duration::from_millis(2500);

/// FORK: one effort level as the composer's picker knows it.
///
/// The picker is a slider whose accessible text reads `"<label>, <n> de 5."`
/// (`"of 5"` in English). Its labels have already moved once — 04/09 the
/// levels rendered "Leve" and "Alta" rather than "Instantâneo" and "Alto", and
/// every `medium|high|extra-high` selection silently fell back to whatever the
/// account had last used. The **ordinal** is the stable part, so that is what
/// drives the selection and the labels are only how we recognise where the
/// slider currently sits.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct LevelSpec {
    /// The name Codex uses (`instant`, `medium`, …).
    pub(crate) key: &'static str,
    /// 1-based position on the slider.
    pub(crate) index: u32,
    /// Alternatives, `|`-separated and unanchored.
    pub(crate) aliases: &'static str,
}

impl LevelSpec {
    /// Anchored form, for *selecting*: "Alta" must not also match "Extra alta".
    pub(crate) fn anchored(self) -> String {
        self.aliases
            .split('|')
            .map(|alias| format!("^{alias}$"))
            .collect::<Vec<_>>()
            .join("|")
    }

    /// Unanchored form, for *verifying* a label we already have.
    pub(crate) fn loose(self) -> String {
        self.aliases.to_string()
    }
}

/// The slider, in order. `pro` is the fifth stop on the same control.
pub(crate) const LEVELS: [LevelSpec; 5] = [
    LevelSpec {
        key: "instant",
        index: 1,
        aliases: "Instant[âa]neo|Instant|Leve",
    },
    LevelSpec {
        key: "medium",
        index: 2,
        aliases: "M[ée]dio|M[ée]dia|Medium",
    },
    LevelSpec {
        key: "high",
        index: 3,
        aliases: "Alto|Alta|High",
    },
    LevelSpec {
        key: "extra-high",
        index: 4,
        aliases: "Extra alto|Extra alta|Extra high",
    },
    LevelSpec {
        key: "pro",
        index: 5,
        aliases: "Pro",
    },
];

pub(crate) fn level_spec(level: &str) -> Option<LevelSpec> {
    LEVELS.into_iter().find(|spec| spec.key == level)
}

/// The label regex used to *select* a level through the picker.
pub(crate) fn level_label(level: &str) -> Option<String> {
    level_spec(level).map(LevelSpec::anchored)
}

/// Timing knobs of the send/wait flows. Production values are the TS
/// literals; tests shrink them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct OpsTimings {
    /// `composerStateWithLabel(tabId, timeoutMs = 6000)` and its 700ms step.
    pub(crate) label_wait: Duration,
    pub(crate) label_poll: Duration,
    /// Precheck: how long a lingering stop button is tolerated (5s, 500ms step).
    pub(crate) generating_grace: Duration,
    pub(crate) generating_poll: Duration,
    /// `if (wasGenerating) await sleep(800)`.
    pub(crate) settle_after_generating: Duration,
    /// `await sleep(1200)` before the compose retry when the send button vanished.
    pub(crate) composer_reset_settle: Duration,
    /// `verifyTilesAttached`: 10s deadline, 500ms step, 800ms late-popup look.
    pub(crate) tiles_deadline: Duration,
    pub(crate) tiles_poll: Duration,
    pub(crate) late_popup_delay: Duration,
    /// `waitAttachmentsReady` step.
    pub(crate) attachments_poll: Duration,
    /// `confirmSubmitted(conversationId, message, 15_000)` and its 2.5s step.
    pub(crate) confirm_submit_wait: Duration,
    pub(crate) confirm_submit_poll: Duration,
    /// `waitReply` step.
    pub(crate) reply_poll: Duration,
    /// `stop`: keep clicking for 8s, 1s apart.
    pub(crate) stop_window: Duration,
    pub(crate) stop_poll: Duration,
}

impl Default for OpsTimings {
    fn default() -> Self {
        Self {
            label_wait: Duration::from_millis(6000),
            label_poll: Duration::from_millis(700),
            generating_grace: Duration::from_millis(5000),
            generating_poll: Duration::from_millis(500),
            settle_after_generating: Duration::from_millis(800),
            composer_reset_settle: Duration::from_millis(1200),
            tiles_deadline: Duration::from_millis(10_000),
            tiles_poll: Duration::from_millis(500),
            late_popup_delay: Duration::from_millis(800),
            attachments_poll: Duration::from_millis(1000),
            confirm_submit_wait: Duration::from_millis(15_000),
            confirm_submit_poll: REPLY_POLL_INTERVAL,
            reply_poll: REPLY_POLL_INTERVAL,
            stop_window: Duration::from_millis(8000),
            stop_poll: Duration::from_millis(1000),
        }
    }
}

// ---------------------------------------------------------------------------
// Model selection
// ---------------------------------------------------------------------------

/// Port of the TS `ModelSpec` union: a named level or an exact slug.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ModelSpec {
    Auto,
    Instant,
    Thinking,
    Medium,
    High,
    ExtraHigh,
    Pro,
    /// Anything else — must be an exact slug from `/backend-api/models`.
    Slug(String),
}

impl ModelSpec {
    /// `""`/`auto` → `Auto`; the level names; anything else is a slug.
    pub(crate) fn parse(spec: &str) -> Self {
        match spec.trim() {
            "" | "auto" => Self::Auto,
            "instant" => Self::Instant,
            "thinking" => Self::Thinking,
            "medium" => Self::Medium,
            "high" => Self::High,
            "extra-high" => Self::ExtraHigh,
            "pro" => Self::Pro,
            other => Self::Slug(other.to_string()),
        }
    }

    pub(crate) fn as_str(&self) -> &str {
        match self {
            Self::Auto => "auto",
            Self::Instant => "instant",
            Self::Thinking => "thinking",
            Self::Medium => "medium",
            Self::High => "high",
            Self::ExtraHigh => "extra-high",
            Self::Pro => "pro",
            Self::Slug(slug) => slug,
        }
    }
}

/// Port of the TS `ResolvedModel`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct ResolvedModel {
    /// Slug for the `?model=` URL parameter (`None` = leave account default).
    pub(crate) slug: Option<String>,
    /// When set, the exact level must additionally be picked via the UI menu.
    /// Anchored, so "Alta" does not also select "Extra alta".
    pub(crate) menu_level: Option<String>,
    /// FORK: the level's 1-based position on the slider. This is what actually
    /// drives the selection; the label only says where the slider sits now.
    pub(crate) menu_index: Option<u32>,
    /// Regex source the composer label should match afterwards (sanity check).
    /// Unanchored: the composer button carries the model name around the level.
    pub(crate) expect_label: Option<String>,
}

/// `(defaultSlug ?? "gpt-5-6").replace(/-(instant|thinking|pro|mini|t-mini)$/i, "")`.
pub(crate) fn model_family_base(default_slug: Option<&str>) -> String {
    static SUFFIX: OnceLock<Regex> = OnceLock::new();
    let suffix = SUFFIX.get_or_init(|| {
        #[expect(clippy::expect_used, reason = "the pattern is a compile-time literal")]
        Regex::new("(?i)-(instant|thinking|pro|mini|t-mini)$").expect("suffix regex must compile")
    });
    suffix
        .replace(default_slug.unwrap_or("gpt-5-6"), "")
        .into_owned()
}

/// FORK: an exact slug needs no fresh catalog. If the cached list already
/// names it, answer from the cache and skip the `GET /backend-api/models`
/// entirely; anything else falls through to the fetch.
fn resolved_from_cached_models(
    api: &ChatGptApi<'_>,
    spec: Option<&ModelSpec>,
) -> Option<ResolvedModel> {
    let ModelSpec::Slug(slug) = spec? else {
        return None;
    };
    let models = api.cached_models()?;
    models
        .models
        .iter()
        .any(|model| model.slug == *slug)
        .then(|| ResolvedModel {
            slug: Some(slug.clone()),
            ..ResolvedModel::default()
        })
}

/// Pure half of `resolveModel` (`ops.ts:110-148`), over an already fetched
/// model list. `None`/`Auto` leave the account default untouched.
pub(crate) fn resolve_model_with(
    spec: Option<&ModelSpec>,
    models: &ModelsInfo,
) -> DriverResult<ResolvedModel> {
    let Some(spec) = spec else {
        return Ok(ResolvedModel::default());
    };
    if *spec == ModelSpec::Auto {
        return Ok(ResolvedModel::default());
    }
    let slugs: Vec<&str> = models.models.iter().map(|m| m.slug.as_str()).collect();
    // `if (slugs.has(spec)) return { slug: spec, menuLevel: null, expectLabel: null };`
    if let ModelSpec::Slug(slug) = spec
        && slugs.contains(&slug.as_str())
    {
        return Ok(ResolvedModel {
            slug: Some(slug.clone()),
            ..ResolvedModel::default()
        });
    }

    // Derive the current family base (e.g. "gpt-5-6") from the default slug.
    let base = model_family_base(models.default_slug.as_deref());
    // `const pick = (suffix) => { const cand = `${base}-${suffix}`; if (slugs.has(cand)) return cand;
    //   const any = models.find((m) => m.slug.endsWith(`-${suffix}`)); return any ? any.slug : null; };`
    let pick = |suffix: &str| -> Option<String> {
        let candidate = format!("{base}-{suffix}");
        if slugs.contains(&candidate.as_str()) {
            return Some(candidate);
        }
        let ending = format!("-{suffix}");
        slugs
            .iter()
            .find(|slug| slug.ends_with(&ending))
            .map(|slug| (*slug).to_string())
    };

    // Selecting is anchored ("Alta" must not select "Extra alta"); verifying is
    // not, because the composer button reads "GPT-5.6 Alta", not "Alta".
    let select = |key: &str| level_spec(key).map(LevelSpec::anchored);
    let verify = |key: &str| level_spec(key).map(LevelSpec::loose);
    let index = |key: &str| level_spec(key).map(|spec| spec.index);
    let resolved = match spec {
        ModelSpec::Instant => ResolvedModel {
            slug: pick("instant"),
            menu_level: None,
            menu_index: None,
            expect_label: verify("instant"),
        },
        ModelSpec::Thinking => ResolvedModel {
            slug: pick("thinking"),
            menu_level: None,
            menu_index: None,
            expect_label: None,
        },
        ModelSpec::Pro => ResolvedModel {
            slug: pick("pro"),
            menu_level: None,
            menu_index: None,
            expect_label: verify("pro"),
        },
        ModelSpec::Medium | ModelSpec::High | ModelSpec::ExtraHigh => ResolvedModel {
            slug: pick("thinking"),
            menu_level: select(spec.as_str()),
            menu_index: index(spec.as_str()),
            expect_label: verify(spec.as_str()),
        },
        ModelSpec::Auto | ModelSpec::Slug(_) => {
            return Err(DriverError::other(format!(
                "Unknown model '{}'. Use auto|instant|thinking|medium|high|extra-high|pro or an exact slug. Known slugs: {}",
                spec.as_str(),
                slugs.join(", ")
            )));
        }
    };
    Ok(resolved)
}

// ---------------------------------------------------------------------------
// Send phase machine — public shapes
// ---------------------------------------------------------------------------

/// `PHASE_HINT` (`ops.ts:73-87`), reworded for a provider instead of an MCP
/// tool user (the TS names `chatgpt_read`/`chatgpt_list`/`chatgpt_response`).
fn phase_hint(phase: FailurePhase) -> &'static str {
    match phase {
        FailurePhase::Navigate => {
            "the message was NOT sent (failed opening the conversation) — retrying is safe"
        }
        FailurePhase::Model => {
            "the message was NOT sent (failed during model selection) — retrying is safe"
        }
        FailurePhase::Precheck => {
            "the message was NOT sent (composer was not ready) — retrying is safe"
        }
        FailurePhase::Upload => "the message was NOT sent (file attach failed) — retrying is safe",
        FailurePhase::Compose => {
            "the message was NOT sent (text never entered the composer) — retrying is safe"
        }
        FailurePhase::AttachmentsWait => {
            "the message was NOT sent (uploads never finished) — retrying is safe"
        }
        FailurePhase::Submit => {
            "AMBIGUOUS: if the error says the send button was not found/disabled nothing was clicked and retrying is safe; otherwise the click may have landed after the error — check that the message is not already the latest user turn of the conversation before resending"
        }
        FailurePhase::Confirm => {
            "the message WAS sent — do not resend; poll the conversation for the reply"
        }
    }
}

/// `if (!sent.ok && /send button not found/i.test(sent.error ?? ""))` plus the
/// "disabled" variant the hint names: neither clicks anything.
fn nothing_was_clicked(message: &str) -> bool {
    static PATTERN: OnceLock<Regex> = OnceLock::new();
    let regex = PATTERN.get_or_init(|| {
        #[expect(clippy::expect_used, reason = "the pattern is a compile-time literal")]
        Regex::new("(?i)send button (not found|disabled)").expect("send-button regex must compile")
    });
    regex.is_match(message)
}

/// Port of `phaseError` (`ops.ts:89-94`): tag an error with the phase it
/// happened in and the retry-safety verdict. `message_landed` already set by
/// the raising code (e.g. "nothing was clicked" at submit) is kept; otherwise
/// it follows the phase: known-not-sent before submit, unknown at submit,
/// known-sent at confirm. An unknown verdict at submit is `SubmitAmbiguous`.
fn phase_error(mut error: DriverError, phase: FailurePhase) -> DriverError {
    let landed = error.message_landed.or(match phase {
        FailurePhase::Submit => None,
        FailurePhase::Confirm => Some(true),
        _ => Some(false),
    });
    if phase == FailurePhase::Submit && landed.is_none() {
        error.kind = DriverErrorKind::SubmitAmbiguous;
    }
    error.message = format!(
        "[send phase: {phase}] {} | {}",
        error.message,
        phase_hint(phase)
    );
    error.phase = Some(phase);
    error.message_landed = landed;
    error
}

/// What to send.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct SendRequest {
    /// `None` = new chat (with the optional model spec).
    pub(crate) conversation_id: Option<String>,
    pub(crate) text: String,
    /// Ignored (with a note) when continuing an existing conversation.
    pub(crate) model: Option<ModelSpec>,
    /// Attached before the text.
    pub(crate) files: Vec<PathBuf>,
    /// FORK (connector mode): select this connector in the composer before
    /// typing. When set, the text is appended after the connector pill instead
    /// of replacing the whole composer (which would wipe the pill).
    pub(crate) mention: Option<String>,
    /// FORK (connector mode): whether the mention may activate the tab.
    pub(crate) mention_strategy: MentionStrategy,
}

/// FORK: how the connector @mention deals with the popover that Radix only
/// mounts reliably in a focused tab (spike S2: it usually mounts in a hidden
/// tab, but a fresh chat sometimes never shows the connector row).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) enum MentionStrategy {
    /// Background first; activate the tab (no reload) when the menu row never
    /// mounts.
    #[default]
    Auto,
    /// Never steal focus; fail when the menu does not mount.
    BackgroundOnly,
    /// Always activate the tab for the mention.
    Activate,
}

/// Mention failures that a focused tab is known to fix (spike S2).
fn mention_needs_focus(error: &str) -> bool {
    let error = error.to_ascii_lowercase();
    error.contains("mention menu")
        || error.contains("highlight the connector row")
        || error.contains("menu closed")
        || error.contains("pill did not appear")
}

/// A message that reached the conversation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Sent {
    pub(crate) conversation_id: String,
    /// Always [`FailurePhase::Confirm`] on success; kept so callers can log the
    /// same vocabulary as the errors.
    pub(crate) phase_reached: FailurePhase,
    /// Composer model label read right after the click (`None` when the read failed).
    pub(crate) model_label: Option<String>,
    /// Non-fatal observations (ignored model spec, renamed uploads, ...).
    pub(crate) notes: Vec<String>,
}

/// Tracks the phase the send is in, readable after the tab closure failed.
struct PhaseCell(StdMutex<FailurePhase>);

impl PhaseCell {
    fn new(phase: FailurePhase) -> Self {
        Self(StdMutex::new(phase))
    }

    fn set(&self, phase: FailurePhase) {
        *self.0.lock().unwrap_or_else(PoisonError::into_inner) = phase;
    }

    fn get(&self) -> FailurePhase {
        *self.0.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

struct Prepared {
    conversation_id: Option<String>,
    model_label: Option<String>,
    notes: Vec<String>,
}

// ---------------------------------------------------------------------------
// Page script result shapes
// ---------------------------------------------------------------------------

/// `setComposerText` / `clearComposer` result.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
struct OkResult {
    ok: bool,
    error: Option<String>,
}

/// `clickSend` result.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, rename_all = "camelCase")]
struct ClickSendResult {
    ok: bool,
    conversation_id: Option<String>,
    generating: bool,
    error: Option<String>,
    url: Option<String>,
}

/// `clickStop` result.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, rename_all = "camelCase")]
struct ClickStopResult {
    ok: bool,
    error: Option<String>,
}

/// `attachmentTiles` result.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
struct Tiles {
    tiles: Vec<String>,
}

/// `dismissUploadDialog` result.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
struct DialogResult {
    found: bool,
    text: Option<String>,
    dismissed: Option<bool>,
    buttons: Vec<String>,
}

/// `menuSelect` result (port of `{ ok, selected?, error? }`).
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(default, rename_all = "camelCase")]
pub(crate) struct MenuSelection {
    pub(crate) ok: bool,
    pub(crate) selected: Option<String>,
    pub(crate) error: Option<String>,
    pub(crate) trigger_label: Option<String>,
    pub(crate) available: Vec<String>,
    /// FORK: where the slider ended up, 1-based, and how many stops it has.
    pub(crate) index: Option<u32>,
    pub(crate) total: Option<u32>,
    /// FORK: whether the label at that stop matched what we expected. A `false`
    /// here is worth reporting but is not a failure: the ordinal is what we
    /// asked for, and the labels are the part that keeps moving.
    pub(crate) label_matched: Option<bool>,
}

/// `stageDownload` result.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
struct StagedDownload {
    ok: bool,
    error: Option<String>,
    b64len: Option<u64>,
    mime: Option<String>,
    cd: Option<String>,
}

/// `readDownloadChunk` result.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
struct DownloadChunk {
    ok: bool,
    chunk: Option<String>,
    error: Option<String>,
}

/// [`page_error_probe`] result.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
struct PageErrors {
    texts: Vec<String>,
}

/// Read-only probe for error surfaces the TS never classified: blocking
/// dialogs, alerts and toasts ("Too many requests", "message too long",
/// "Something went wrong", ...). Nothing is clicked. Lives here rather than in
/// `page_scripts.rs` because it is an addition of the port, not of the TS.
fn page_error_probe() -> String {
    r#"() => {
    const pageErrorProbe = [];
    const push = (el) => {
      const t = (el.innerText || el.textContent || '').replace(/\s+/g, ' ').trim();
      if (t) pageErrorProbe.push(t.slice(0, 300));
    };
    document.querySelectorAll(
      'dialog, [role="dialog"], [role="alertdialog"], [role="alert"], [data-testid*="toast"], [class*="toast"]'
    ).forEach(push);
    const ed = document.querySelector('#prompt-textarea');
    const form = ed ? ed.closest('form') : null;
    if (form) form.querySelectorAll('[class*="text-error"], [class*="text-token-text-error"]').forEach(push);
    return JSON.stringify({ texts: pageErrorProbe });
  }"#
    .to_string()
}

/// Classify page error texts (PT/EN) onto the error table. `None` = nothing
/// recognizable (benign dialogs are ignored).
pub(crate) fn classify_page_error(texts: &[String]) -> Option<(DriverErrorKind, String)> {
    static TABLE: OnceLock<Vec<(DriverErrorKind, Regex)>> = OnceLock::new();
    let table = TABLE.get_or_init(|| {
        let compile = |source: &str| {
            #[expect(clippy::expect_used, reason = "the patterns are compile-time literals")]
            Regex::new(source).expect("page error regex must compile")
        };
        vec![
            (
                DriverErrorKind::RateLimited,
                compile("(?i)too many requests|muitas solicita|rate limit|limite de (uso|mensagens)|reached (your|the) [^.]*limit|atingiu o limite"),
            ),
            (
                DriverErrorKind::MessageTooLong,
                compile("(?i)(message|mensagem)[^.]*(too long|muito long)|(too long|muito long)[^.]*(message|mensagem)|exceeds the (maximum|character)|excede o (limite|tamanho)|maximum length"),
            ),
            (
                DriverErrorKind::LoginRequired,
                compile("(?i)session has expired|sess[ãa]o expirou|log in again|fa[çc]a login novamente"),
            ),
            (
                DriverErrorKind::Upstream,
                compile("(?i)something went wrong|algo deu errado|error in message stream|erro ao gerar|network error|erro de rede"),
            ),
        ]
    });
    for text in texts {
        for (kind, regex) in table {
            if regex.is_match(text) {
                return Some((kind.clone(), text.clone()));
            }
        }
    }
    None
}

/// `location.pathname.match(/\/c\/([0-9a-f-]{20,})/i)`.
pub(crate) fn conversation_id_from_url(url: &str) -> Option<String> {
    static PATTERN: OnceLock<Regex> = OnceLock::new();
    let regex = PATTERN.get_or_init(|| {
        #[expect(clippy::expect_used, reason = "the pattern is a compile-time literal")]
        Regex::new("(?i)/c/([0-9a-f-]{20,})").expect("conversation url regex must compile")
    });
    regex
        .captures(url)
        .and_then(|caps| caps.get(1))
        .map(|m| m.as_str().to_string())
}

/// `encodeURIComponent` for the `?model=` slug.
fn encode_uri_component(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for byte in value.bytes() {
        match byte {
            b'A'..=b'Z'
            | b'a'..=b'z'
            | b'0'..=b'9'
            | b'-'
            | b'_'
            | b'.'
            | b'!'
            | b'~'
            | b'*'
            | b'\''
            | b'('
            | b')' => out.push(byte as char),
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

/// `message.trim().slice(0, 120)` — the anchor compared against the latest
/// user turn.
fn anchor_of(text: &str) -> String {
    text.trim().chars().take(120).collect()
}

/// Latest user turn's text matches `anchor` (`confirmSubmitted` / `waitReply`).
fn last_user_matches(conv: &Conversation, anchor: &str) -> bool {
    conv.turns
        .iter()
        .rev()
        .find(|t| t.role == "user")
        .is_some_and(|t| anchor_of(&t.text) == anchor)
}

/// Port of `lastAssistantReply` (`ops.ts:900-909`): walking back from the end,
/// the first assistant turn with text/assets or tool turn with assets, stopping
/// at the last user turn.
pub(crate) fn last_assistant_reply(conv: &Conversation) -> Option<&Turn> {
    for turn in conv.turns.iter().rev() {
        if turn.role == "user" {
            break;
        }
        if turn.role == "assistant" && (!turn.text.is_empty() || !turn.assets.is_empty()) {
            return Some(turn);
        }
        if turn.role == "tool" && !turn.assets.is_empty() {
            return Some(turn);
        }
    }
    None
}

/// `asyncStatus !== null && asyncStatus !== 0`: a server-side async run (deep
/// research, long Pro runs) that keeps working after an early acknowledgment.
pub(crate) fn async_active(conv: &Conversation) -> bool {
    conv.async_status.is_some_and(|status| status != 0)
}

/// `!c.isGenerating && (c.asyncStatus === null || c.asyncStatus === 0)`.
pub(crate) fn api_idle(conv: &Conversation) -> bool {
    !conv.is_generating && !async_active(conv)
}

/// One `waitReply` poll's verdict (`ops.ts:751-776`), kept pure so the
/// completion rule is unit-testable over fixtures.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct ReplyCheck {
    /// The reply being watched (`None` when the anchor does not match yet).
    pub(crate) reply: Option<Turn>,
    pub(crate) idle: bool,
    /// Fingerprint of this poll, to feed the next check.
    pub(crate) fingerprint: u64,
    pub(crate) done: bool,
}

/// `done = reply && idle && (reply.endTurn === true || stable)`, with
/// `stable` = same fingerprint as the previous poll. The fingerprint is
/// [`fingerprint`] over the whole conversation rather than the TS
/// `messageId:text.length:turns.length`, so a multi-image run that grows its
/// asset list under one message id is not mistaken for stable.
pub(crate) fn check_reply(
    conv: &Conversation,
    anchor: Option<&str>,
    prev_fingerprint: Option<u64>,
) -> ReplyCheck {
    // When we just sent a message, the previous (already finished) reply must
    // not be mistaken for the new one: require the API to show OUR user
    // message as the latest user turn before trusting completion.
    // `anchorUserText?.trim().slice(0, 120)` — normalized here so callers may
    // pass the raw message.
    let anchored = anchor.is_none_or(|anchor| last_user_matches(conv, &anchor_of(anchor)));
    let reply = if anchored {
        last_assistant_reply(conv).cloned()
    } else {
        None
    };
    let idle = !conv.is_generating
        && !async_active(conv)
        && reply.as_ref().is_none_or(|r| r.status != "in_progress");
    let current = fingerprint(conv);
    let stable = reply.is_some() && prev_fingerprint == Some(current);
    // Asset replies intentionally have no fast path: a multi-image run can
    // land its first image while more are coming, so assets only complete
    // through the stability check (one extra poll, ~2.5s).
    let done = reply.is_some()
        && idle
        && (reply.as_ref().and_then(|r| r.end_turn) == Some(true) || stable);
    ReplyCheck {
        reply,
        idle,
        fingerprint: current,
        done,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ReplyStatus {
    Complete,
    Generating,
}

/// Port of the TS `SendResult` for `waitReply`.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct ReplyWait {
    pub(crate) conversation_id: String,
    pub(crate) status: ReplyStatus,
    pub(crate) reply: Option<Turn>,
    pub(crate) reply_text: Option<String>,
    pub(crate) elapsed: Duration,
    /// Last successful read, for callers that want the whole thread.
    pub(crate) conversation: Option<Conversation>,
    pub(crate) note: Option<String>,
}

/// `stop()` result.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct StopOutcome {
    pub(crate) ok: bool,
    pub(crate) detail: String,
}

/// One file written by [`ChatGptOps::download_assets`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SavedAsset {
    pub(crate) file: PathBuf,
    pub(crate) bytes: u64,
    pub(crate) file_id: String,
    pub(crate) kind: AssetKind,
}

/// `attachFiles` / `verifyTilesAttached` result.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct Attached {
    notes: Vec<String>,
    expected_tiles: u64,
}

/// `splitAlreadyUploaded` result.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct UploadSplit {
    pub(crate) fresh: Vec<PathBuf>,
    pub(crate) notes: Vec<String>,
}

/// Pure half of `splitAlreadyUploaded` (`ops.ts:452-490`): files whose
/// name+size match an upload already in this conversation are skipped.
pub(crate) fn split_already_uploaded_with(
    conv: &Conversation,
    files: &[PathBuf],
) -> DriverResult<UploadSplit> {
    let mut uploaded: Vec<(&str, u64)> = Vec::new();
    for turn in &conv.turns {
        if turn.role != "user" {
            continue;
        }
        for asset in &turn.assets {
            if let (Some(name), Some(size)) = (asset.name.as_deref(), asset.size_bytes) {
                uploaded.push((name, size));
            }
        }
    }
    let mut fresh = Vec::new();
    let mut skipped = Vec::new();
    for file in files {
        let meta = std::fs::metadata(file)
            .map_err(|_| DriverError::other(format!("file not found: {}", file.display())))?;
        let name = file_name_of(file);
        if uploaded
            .iter()
            .any(|(n, size)| *n == name && *size == meta.len())
        {
            skipped.push(name);
        } else {
            fresh.push(file.clone());
        }
    }
    let mut notes = Vec::new();
    if !skipped.is_empty() {
        let list = skipped
            .iter()
            .map(|s| format!("'{s}'"))
            .collect::<Vec<_>>()
            .join(", ");
        notes.push(format!(
            "{list} not re-uploaded: an identical file (same name and size) was already sent in this conversation and remains readable by the model — referencing the existing copy. If the content did change, rename the file locally and send again."
        ));
    }
    Ok(UploadSplit { fresh, notes })
}

fn file_name_of(path: &Path) -> String {
    path.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.display().to_string())
}

/// `browser_upload` selectors: the plan's image input first for images, then
/// the TS list (`ops.ts:501-505`, "the composer keeps a hidden multi-file
/// input inside the form; setting files on it directly skips the menu").
const IMAGE_UPLOAD_SELECTORS: [&str; 4] = [
    r#"input[data-testid="upload-photos-input"]"#,
    r#"form input[type="file"]:not([accept*="image"])"#,
    r#"form input[type="file"]"#,
    r#"input[type="file"]"#,
];
const FILE_UPLOAD_SELECTORS: [&str; 3] = [
    r#"form input[type="file"]:not([accept*="image"])"#,
    r#"form input[type="file"]"#,
    r#"input[type="file"]"#,
];

fn is_image_path(path: &Path) -> bool {
    path.extension()
        .and_then(|ext| ext.to_str())
        .is_some_and(|ext| {
            matches!(
                ext.to_ascii_lowercase().as_str(),
                "png" | "jpg" | "jpeg" | "gif" | "webp" | "bmp" | "heic" | "heif"
            )
        })
}

/// `${escapeRe(stem)}\(\d+\)${escapeRe(ext)}`: ChatGPT's "name(N)" rename.
fn rename_regex(base: &str) -> Option<Regex> {
    let (stem, ext) = match base.rfind('.') {
        Some(dot) if dot > 0 => (&base[..dot], &base[dot..]),
        _ => (base, ""),
    };
    Regex::new(&format!(
        "^{}\\(\\d+\\){}$",
        regex_lite::escape(stem),
        regex_lite::escape(ext)
    ))
    .ok()
}

/// Port of `pickFileName` (`ops.ts:911-917`).
fn pick_file_name(asset_name: Option<&str>, file_id: &str, ctype: &str, cd: &str) -> String {
    if let Some(name) = asset_name.filter(|n| !n.is_empty()) {
        return sanitize_file_name(name);
    }
    static FILENAME: OnceLock<Regex> = OnceLock::new();
    let regex = FILENAME.get_or_init(|| {
        #[expect(clippy::expect_used, reason = "the pattern is a compile-time literal")]
        Regex::new("(?i)filename\\*?=(?:UTF-8''|\")?([^\";]+)")
            .expect("filename regex must compile")
    });
    if let Some(m) = regex.captures(cd).and_then(|caps| caps.get(1)) {
        let decoded = percent_decode(m.as_str());
        return sanitize_file_name(&decoded);
    }
    let ext = if ctype.contains("png") {
        ".png"
    } else if ctype.contains("jpeg") {
        ".jpg"
    } else if ctype.contains("webp") {
        ".webp"
    } else {
        ""
    };
    format!("{}{ext}", sanitize_file_name(file_id))
}

/// `decodeURIComponent` for the content-disposition filename (lenient).
fn percent_decode(value: &str) -> String {
    let bytes = value.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            let hex = &value[i + 1..i + 3];
            if let Ok(byte) = u8::from_str_radix(hex, 16) {
                out.push(byte);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// `name.replace(/[^\w.\-]+/g, "_").slice(0, 120)`.
fn sanitize_file_name(name: &str) -> String {
    let mut out = String::new();
    let mut last_was_sep = false;
    for ch in name.chars() {
        if ch.is_alphanumeric() || ch == '_' || ch == '.' || ch == '-' {
            out.push(ch);
            last_was_sep = false;
        } else if !last_was_sep {
            out.push('_');
            last_was_sep = true;
        }
    }
    out.chars().take(120).collect()
}

/// Port of `uniquePath`: never silently overwrite — suffix -1, -2, … when the
/// name is taken.
fn unique_path(dir: &Path, name: &str) -> PathBuf {
    let (stem, ext) = match name.rfind('.') {
        Some(dot) if dot > 0 => (&name[..dot], &name[dot..]),
        _ => (name, ""),
    };
    let mut candidate = dir.join(name);
    let mut i = 1;
    while candidate.exists() {
        candidate = dir.join(format!("{stem}-{i}{ext}"));
        i += 1;
    }
    candidate
}

// ---------------------------------------------------------------------------
// The ops object
// ---------------------------------------------------------------------------

/// `PageEval` over the pool's daemon so `ChatGptApi` can be built on any tab.
struct DaemonEval(Arc<dyn TabDaemon>);

impl PageEval for DaemonEval {
    fn eval<'a>(
        &'a self,
        tab_id: TabId,
        expression: String,
        timeout_ms: u64,
    ) -> BoxFuture<'a, DriverResult<Value>> {
        self.0.eval_in(tab_id, expression, timeout_ms)
    }
}

/// Port of `class ChatGptOps`.
pub(crate) struct ChatGptOps {
    eval: DaemonEval,
    tabs: Arc<TabPool>,
    /// ChatGPT origin, no trailing slash.
    base_url: String,
    timings: OpsTimings,
}

impl ChatGptOps {
    pub(crate) fn new(
        daemon: Arc<DaemonClient>,
        tabs: Arc<TabPool>,
        base_url: impl Into<String>,
    ) -> Self {
        Self::with_daemon(daemon, tabs, base_url)
    }

    /// Same as [`Self::new`] over any [`TabDaemon`] (tests use a fake).
    pub(crate) fn with_daemon(
        daemon: Arc<dyn TabDaemon>,
        tabs: Arc<TabPool>,
        base_url: impl Into<String>,
    ) -> Self {
        Self {
            eval: DaemonEval(daemon),
            tabs,
            base_url: base_url.into().trim_end_matches('/').to_string(),
            timings: OpsTimings::default(),
        }
    }

    pub(crate) fn with_timings(mut self, timings: OpsTimings) -> Self {
        self.timings = timings;
        self
    }

    pub(crate) fn base_url(&self) -> &str {
        &self.base_url
    }

    pub(crate) fn tabs(&self) -> &Arc<TabPool> {
        &self.tabs
    }

    fn daemon(&self) -> &dyn TabDaemon {
        self.eval.0.as_ref()
    }

    /// The backend API bound to `tab_id` (page-side fetches with the browser's
    /// cookies; relative paths, as the TS does).
    pub(crate) fn api_on(&self, tab_id: TabId) -> ChatGptApi<'_> {
        // FORK: every backend call of the driver is paced process-wide; see
        // `api::BackendLimiter`.
        ChatGptApi::new(&self.eval, tab_id, "").with_backend_limiter()
    }

    /// FORK: reads the reply's progress from the DOM of the tab bound to
    /// `conversation_id` — no backend request. `None` when no tab of this
    /// pool shows that conversation (the reader then falls back to the API).
    pub(crate) async fn dom_progress(
        &self,
        conversation_id: &str,
    ) -> DriverResult<Option<DomProgress>> {
        let Some(tab_id) = self.tabs.bound_tab_id(conversation_id) else {
            return Ok(None);
        };
        let progress: DomProgress = self
            .eval_as(
                tab_id,
                page_scripts::dom_progress(),
                DEFAULT_TOOL_TIMEOUT_MS,
            )
            .await?;
        let on_page = conversation_id_from_url(&progress.url).as_deref() == Some(conversation_id);
        Ok(on_page.then_some(progress))
    }

    /// The backend API on the best read tab for `conversation_id`.
    pub(crate) async fn api_for(
        &self,
        conversation_id: Option<&str>,
    ) -> DriverResult<ChatGptApi<'_>> {
        let tab_id = self.tabs.eval_tab_id(conversation_id).await?;
        Ok(self.api_on(tab_id))
    }

    /// Evaluate a page script and decode its JSON result into `T`.
    async fn eval_as<T: DeserializeOwned>(
        &self,
        tab_id: TabId,
        script: String,
        timeout_ms: u64,
    ) -> DriverResult<T> {
        let value = self.daemon().eval_in(tab_id, script, timeout_ms).await?;
        Ok(serde_json::from_value(value)?)
    }

    // ---- model selection ----------------------------------------------------

    /// Port of `resolveModel`: fetches the model list on a read tab.
    pub(crate) async fn resolve_model(
        &self,
        spec: Option<&ModelSpec>,
    ) -> DriverResult<ResolvedModel> {
        if spec.is_none_or(|spec| *spec == ModelSpec::Auto) {
            return Ok(ResolvedModel::default());
        }
        let api = self.api_for(None).await?;
        if let Some(resolved) = resolved_from_cached_models(&api, spec) {
            return Ok(resolved);
        }
        let models = api.models().await?;
        resolve_model_with(spec, &models)
    }

    /// `resolveModel` on a tab the caller already holds.
    async fn resolve_model_on(
        &self,
        tab_id: TabId,
        spec: Option<&ModelSpec>,
    ) -> DriverResult<ResolvedModel> {
        if spec.is_none_or(|spec| *spec == ModelSpec::Auto) {
            return Ok(ResolvedModel::default());
        }
        let api = self.api_on(tab_id);
        if let Some(resolved) = resolved_from_cached_models(&api, spec) {
            return Ok(resolved);
        }
        let models = api.models().await?;
        resolve_model_with(spec, &models)
    }

    /// Set the reasoning level via the composer menu (activates a tab briefly).
    pub(crate) async fn set_level_via_menu(
        &self,
        level_regex: &str,
        level_index: Option<u32>,
    ) -> DriverResult<MenuSelection> {
        self.tabs
            .with_tab_for(None, |tab_id| {
                self.set_level_via_menu_on(tab_id, level_regex, level_index)
            })
            .await
    }

    /// Internal variant for callers that already hold the tab lock (`prepareAndSend`).
    async fn set_level_via_menu_on(
        &self,
        tab_id: TabId,
        level_regex: &str,
        level_index: Option<u32>,
    ) -> DriverResult<MenuSelection> {
        // Validate here so a bad pattern fails with a clear message instead of
        // exploding inside the page script (which would bypass its JSON contract).
        if let Err(error) = Regex::new(&format!("(?i){level_regex}")) {
            return Ok(MenuSelection {
                ok: false,
                error: Some(format!("invalid level regex '{level_regex}': {error}")),
                ..MenuSelection::default()
            });
        }
        self.tabs
            .with_activated_on(tab_id, |id| {
                self.eval_as::<MenuSelection>(
                    id,
                    page_scripts::menu_select(MenuKind::Level, level_regex, level_index),
                    DEFAULT_TOOL_TIMEOUT_MS,
                )
            })
            .await
    }

    /// Port of `discoverMenu`: dump the composer model/level menu (activates a tab).
    pub(crate) async fn discover_menu(&self) -> DriverResult<Value> {
        self.tabs
            .with_tab_for(None, |tab_id| {
                self.tabs.with_activated_on(tab_id, |id| {
                    self.daemon().eval_in(
                        id,
                        page_scripts::menu_discover(),
                        DEFAULT_TOOL_TIMEOUT_MS,
                    )
                })
            })
            .await
    }

    /// Port of `composerState(tabId?)`: the primary tab when none is given.
    pub(crate) async fn composer_state(
        &self,
        tab_id: Option<TabId>,
    ) -> DriverResult<ComposerState> {
        let id = match tab_id {
            Some(id) => id,
            None => self.tabs.ensure().await?,
        };
        self.composer_state_on(id).await
    }

    async fn composer_state_on(&self, tab_id: TabId) -> DriverResult<ComposerState> {
        self.eval_as(
            tab_id,
            page_scripts::composer_state(),
            DEFAULT_TOOL_TIMEOUT_MS,
        )
        .await
    }

    /// `composerState`, retrying briefly until the model label renders — the
    /// model button mounts a beat after the composer on fresh navigations.
    pub(crate) async fn composer_state_with_label(
        &self,
        tab_id: Option<TabId>,
    ) -> DriverResult<ComposerState> {
        let id = match tab_id {
            Some(id) => id,
            None => self.tabs.ensure().await?,
        };
        self.composer_state_with_label_on(id).await
    }

    async fn composer_state_with_label_on(&self, tab_id: TabId) -> DriverResult<ComposerState> {
        let started = Instant::now();
        loop {
            let state = self.composer_state_on(tab_id).await?;
            if state.model_label.is_some() || started.elapsed() > self.timings.label_wait {
                return Ok(state);
            }
            tokio::time::sleep(self.timings.label_poll).await;
        }
    }

    // ---- sending ------------------------------------------------------------

    /// Port of `send` (`ops.ts:199-281`) minus the reply wait: prepare the tab,
    /// attach files, type, click, confirm. `conversation_id: None` = new chat.
    pub(crate) async fn send(&self, request: SendRequest) -> DriverResult<Sent> {
        // Conversation-affine tab: sends to the same conversation serialize on
        // its bound tab; sends to different conversations run in parallel.
        let phase = PhaseCell::new(FailurePhase::Navigate);
        let request = &request;
        let phase = &phase;
        let outcome = self
            .tabs
            .with_tab_for(request.conversation_id.as_deref(), |tab_id| async move {
                let prepared = self.prepare_and_send(tab_id, request, phase).await?;
                self.tabs.bind(
                    tab_id,
                    prepared
                        .conversation_id
                        .as_deref()
                        .or(request.conversation_id.as_deref()),
                );
                Ok(prepared)
            })
            .await;

        let prepared = match outcome {
            Ok(prepared) => prepared,
            Err(error) => {
                let at = phase.get();
                // Submit-phase failures are the dangerous ones: the extension
                // keeps executing the eval after the daemon's transport cap
                // fires, so the click may have landed anyway. For an existing
                // conversation the API can settle it — never make the caller
                // guess (or double-send).
                let ambiguous = at == FailurePhase::Submit && error.message_landed.is_none();
                match (ambiguous, request.conversation_id.as_deref()) {
                    (true, Some(conversation_id)) => {
                        match self
                            .confirm_submitted(
                                conversation_id,
                                &request.text,
                                self.timings.confirm_submit_wait,
                            )
                            .await
                        {
                            Some(true) => {
                                info!(
                                    "[chatgpt_web send] submit step errored but the API shows the message landed — continuing"
                                );
                                Prepared {
                                    conversation_id: Some(conversation_id.to_string()),
                                    model_label: None,
                                    notes: vec![
                                        "the submit step errored but the message WAS confirmed sent (the API shows it as the latest user turn)"
                                            .to_string(),
                                    ],
                                }
                            }
                            Some(false) => {
                                let mut error = phase_error(error.landed(Some(false)), at);
                                error.message.push_str(
                                    " | API check: the message is NOT in the conversation — it was not sent; retrying is safe",
                                );
                                return Err(error);
                            }
                            // API unreachable — verdict unknown.
                            None => return Err(phase_error(error, at)),
                        }
                    }
                    _ => return Err(phase_error(error, at)),
                }
            }
        };

        let Some(conversation_id) = prepared.conversation_id else {
            // URL did not flip (rare) — the send may still have landed; the TS
            // reports it and tells the user to look the chat up. A provider
            // cannot poll without the id, so this is a (landed) error.
            return Err(DriverError::ui_changed(format!(
                "conversation id not captured from the URL after the send{}",
                if prepared.notes.is_empty() {
                    String::new()
                } else {
                    format!(" ({})", prepared.notes.join(" | "))
                }
            ))
            .with_phase(FailurePhase::Confirm)
            .landed(Some(true)));
        };
        Ok(Sent {
            conversation_id,
            phase_reached: FailurePhase::Confirm,
            model_label: prepared.model_label,
            notes: prepared.notes,
        })
    }

    /// Port of `prepareAndSend` (`ops.ts:283-445`).
    async fn prepare_and_send(
        &self,
        tab_id: TabId,
        request: &SendRequest,
        phase: &PhaseCell,
    ) -> DriverResult<Prepared> {
        let mut notes: Vec<String> = Vec::new();
        let api = self.api_on(tab_id);

        if let Some(conversation_id) = request.conversation_id.as_deref() {
            self.tabs
                .show_conversation_on(tab_id, Some(conversation_id))
                .await?;
            if request.model.is_some() {
                notes.push(
                    "model spec is ignored when continuing an existing conversation".to_string(),
                );
            }
        } else {
            phase.set(FailurePhase::Model);
            let resolved = self
                .resolve_model_on(tab_id, request.model.as_ref())
                .await?;
            let url = match resolved.slug.as_deref() {
                Some(slug) => format!("{}/?model={}", self.base_url, encode_uri_component(slug)),
                None => format!("{}/", self.base_url),
            };
            self.tabs.goto_on(tab_id, &url).await?;
            if let Some(level) = resolved.menu_level.as_deref() {
                let selection = self
                    .set_level_via_menu_on(tab_id, level, resolved.menu_index)
                    .await?;
                if selection.ok {
                    // FORK: report the ordinal alongside the label. The labels
                    // move (04/09 they read "Leve"/"Alta"), the ordinal does
                    // not, so "Alta (3/5)" is the part worth reading.
                    let position = match (selection.index, selection.total) {
                        (Some(index), Some(total)) => format!(" ({index}/{total})"),
                        _ => String::new(),
                    };
                    notes.push(format!(
                        "effort level set through the picker: {}{position}",
                        selection.selected.as_deref().unwrap_or("?")
                    ));
                    if selection.label_matched == Some(false) {
                        notes.push(format!(
                            "the picker labelled that stop '{}', which is not a name Codex knows — the ordinal selection stands, but the label table is out of date",
                            selection.selected.as_deref().unwrap_or("?")
                        ));
                    }
                } else {
                    let available = if selection.available.is_empty() {
                        String::new()
                    } else {
                        format!("; available: {}", selection.available.join(", "))
                    };
                    notes.push(format!(
                        "exact level selection via menu failed ({}{available}); continuing with the model slug default",
                        selection.error.as_deref().unwrap_or("unknown")
                    ));
                }
                self.tabs
                    .wait_ready_on(tab_id, Duration::from_secs(20))
                    .await?;
            }
            if let Some(expect) = resolved.expect_label.as_deref() {
                let state = self.composer_state_with_label_on(tab_id).await?;
                // FORK: check every composer pill, not only the model button.
                // The effort level moved into a pill of its own, so matching
                // `model_label` alone reported a mismatch for a level that had
                // in fact been applied.
                if let Ok(expect) = Regex::new(&format!("(?i){expect}")) {
                    let candidates: Vec<&str> = state
                        .model_label
                        .as_deref()
                        .into_iter()
                        .chain(state.pills.iter().map(String::as_str))
                        .collect();
                    if !candidates.is_empty()
                        && !candidates.iter().any(|label| expect.is_match(label))
                    {
                        notes.push(format!(
                            "composer shows {:?}, none of which matches the requested model — the picker UI may have changed",
                            candidates
                        ));
                    }
                }
            }
        }

        // Block sending into a conversation that is still generating. The DOM
        // lags the API by a beat after a reply completes (the stop button
        // lingers), so give it a short grace before failing — otherwise a send
        // fired right after a completed reply flakes here.
        phase.set(FailurePhase::Precheck);
        let mut pre = self.composer_state_on(tab_id).await?;
        let was_generating = pre.generating;
        if pre.generating {
            let started = Instant::now();
            while pre.generating && started.elapsed() < self.timings.generating_grace {
                tokio::time::sleep(self.timings.generating_poll).await;
                pre = self.composer_state_on(tab_id).await?;
            }
        }
        if pre.generating
            && let Some(conversation_id) = request.conversation_id.as_deref()
        {
            // Background-tab throttling can freeze the streaming UI (stop
            // button stuck) long after the reply actually finished — the API is
            // the source of truth. If it says idle, a reload unsticks the page.
            let idle = api
                .read_conversation(conversation_id)
                .await
                .map(|conv| api_idle(&conv))
                .unwrap_or(false);
            if idle {
                self.tabs
                    .goto_on(tab_id, &format!("{}/c/{conversation_id}", self.base_url))
                    .await?;
                pre = self.composer_state_on(tab_id).await?;
            }
        }
        if pre.generating {
            return Err(DriverError::busy(
                "This conversation is still generating a reply. Wait for it or stop it first.",
            ));
        }
        // Right after a reply finishes streaming React re-renders the composer,
        // which can wipe text typed into it during the transition — let it settle.
        if was_generating {
            tokio::time::sleep(self.timings.settle_after_generating).await;
        }

        // Re-uploading a file ChatGPT already has triggers a blocking "you
        // already uploaded this file" popup (and a rename to "name(1)"). When
        // the identical file is already in THIS conversation the model can
        // still read it, so we reference the existing copy instead.
        phase.set(FailurePhase::Upload);
        let mut files = request.files.clone();
        if !files.is_empty()
            && let Some(conversation_id) = request.conversation_id.as_deref()
        {
            let split = self
                .split_already_uploaded(&api, conversation_id, &files)
                .await;
            files = split.fresh;
            notes.extend(split.notes);
        }
        let mut expected_tiles = 0;
        if !files.is_empty() {
            let attached = self.attach_files(tab_id, &files).await?;
            notes.extend(attached.notes);
            expected_tiles = attached.expected_tiles;
        }

        phase.set(FailurePhase::Compose);
        // FORK: in connector mode the message must be typed AFTER the connector
        // pill, so a select-all-and-replace would wipe it; the combined script
        // selects the connector (idempotently) and appends the text.
        let compose_script = || match request.mention.as_deref() {
            Some(name) => {
                crate::chatgpt_web::connector::connector_attach::mention_and_compose_script(
                    name,
                    &request.text,
                )
            }
            None => page_scripts::set_composer_text(&request.text),
        };
        let mut set: OkResult = match (request.mention.as_deref(), request.mention_strategy) {
            (Some(_), MentionStrategy::Activate) => {
                self.tabs
                    .with_activated_on_keep(tab_id, |id| async move {
                        self.eval_as(id, compose_script(), EVAL_COMPOSE_TIMEOUT_MS)
                            .await
                    })
                    .await?
            }
            _ => {
                self.eval_as(tab_id, compose_script(), EVAL_COMPOSE_TIMEOUT_MS)
                    .await?
            }
        };
        // FORK: the mention popover sometimes never mounts in a hidden tab
        // (spike S2); a focused tab fixes it. Retry once with the tab
        // activated — and no reload afterwards, which would drop the pill.
        if !set.ok
            && request.mention.is_some()
            && request.mention_strategy == MentionStrategy::Auto
            && set.error.as_deref().is_some_and(mention_needs_focus)
        {
            let first_error = set.error.clone().unwrap_or_default();
            notes.push(format!(
                "mention fell back to activating the tab: {first_error}"
            ));
            set = self
                .tabs
                .with_activated_on_keep(tab_id, |id| async move {
                    self.eval_as(id, compose_script(), EVAL_COMPOSE_TIMEOUT_MS)
                        .await
                })
                .await?;
        }
        if !set.ok {
            return Err(DriverError::ui_changed(format!(
                "could not put the message into the composer: {}. The composer UI may have changed.",
                set.error.as_deref().unwrap_or("unknown")
            )));
        }

        if !files.is_empty() {
            phase.set(FailurePhase::AttachmentsWait);
            notes.extend(
                self.wait_attachments_ready(tab_id, expected_tiles, UPLOAD_TIMEOUT_MS)
                    .await?,
            );
        }

        phase.set(FailurePhase::Submit);
        let mut sent: ClickSendResult = self
            .eval_as(
                tab_id,
                page_scripts::click_send(CLICK_SEND_PAGE_TIMEOUT_MS),
                EVAL_SUBMIT_TIMEOUT_MS,
            )
            .await?;
        if !sent.ok
            && sent
                .error
                .as_deref()
                .is_some_and(|e| e.to_lowercase().contains("send button not found"))
        {
            // The composer remount race (see above) can also swallow the text
            // AFTER we set it, leaving an empty composer with no send button.
            // No click has happened in that case, so retyping and retrying
            // once is safe — unless attachments were wiped too, which must
            // fail instead of sending textless.
            tokio::time::sleep(self.timings.composer_reset_settle).await;
            if expected_tiles > 0 {
                let state = self.composer_state_on(tab_id).await?;
                if state.attachments < expected_tiles {
                    return Err(DriverError::other(format!(
                        "the composer was reset mid-send and the attachments were lost ({}/{expected_tiles} tiles) — retry the call",
                        state.attachments
                    ))
                    .landed(Some(false)));
                }
            }
            phase.set(FailurePhase::Compose);
            let retyped: OkResult = self
                .eval_as(tab_id, compose_script(), EVAL_COMPOSE_TIMEOUT_MS)
                .await?;
            if retyped.ok {
                phase.set(FailurePhase::Submit);
                sent = self
                    .eval_as(
                        tab_id,
                        page_scripts::click_send(CLICK_SEND_PAGE_TIMEOUT_MS),
                        EVAL_SUBMIT_TIMEOUT_MS,
                    )
                    .await?;
            }
        }
        // FORK (verified live): right after the effort-picker reload the first
        // synthetic click on an enabled send button can be swallowed while the
        // composer finishes hydrating — no stop button, no URL flip, and the
        // text still sits in the composer. That state is provably "nothing
        // sent" for a new chat, so clicking again is safe.
        // On an existing conversation the URL already has `/c/`, so `ok` is
        // true even when nothing was sent; the composer still holding the
        // text is the reliable signal there (a sent message empties it).
        let mut resend_attempts = 0u8;
        while !(sent.ok && sent.generating) && sent.error.is_none() && resend_attempts < 2 {
            let state = self.composer_state_on(tab_id).await?;
            let nothing_sent = state.has_composer
                && !state.generating
                && (request.conversation_id.is_some()
                    || conversation_id_from_url(&state.url).is_none())
                && state
                    .text
                    .as_deref()
                    .is_some_and(|text| !text.trim().is_empty());
            if !nothing_sent {
                break;
            }
            resend_attempts += 1;
            warn!(
                "[chatgpt_web ops] the send click was swallowed (composer still full); clicking again ({resend_attempts}/2)"
            );
            tokio::time::sleep(self.timings.composer_reset_settle).await;
            sent = self
                .eval_as(
                    tab_id,
                    page_scripts::click_send(CLICK_SEND_PAGE_TIMEOUT_MS),
                    EVAL_SUBMIT_TIMEOUT_MS,
                )
                .await?;
        }
        if !sent.ok {
            let detail = sent
                .error
                .clone()
                .unwrap_or_else(|| "send failed".to_string());
            if nothing_was_clicked(&detail) {
                return Err(DriverError::ui_changed(format!(
                    "{detail}. If the UI changed, the send control must be rediscovered."
                ))
                .landed(Some(false)));
            }
            // The click went out but neither the stop button nor the URL flip
            // showed up: look for an error surface before calling it ambiguous.
            if let Some((kind, text)) = self.probe_page_errors(tab_id).await {
                return Err(page_error(kind, &text));
            }
            return Err(DriverError::other(format!(
                "{detail}. If the UI changed, the send control must be rediscovered."
            )));
        }

        phase.set(FailurePhase::Confirm);
        let state = self.composer_state_on(tab_id).await.ok();
        let mut conversation_id = sent
            .conversation_id
            .clone()
            .or_else(|| request.conversation_id.clone());
        if conversation_id.is_none() {
            conversation_id = state
                .as_ref()
                .and_then(|s| conversation_id_from_url(&s.url))
                .or_else(|| sent.url.as_deref().and_then(conversation_id_from_url));
        }
        if !sent.generating {
            // Existing conversation, URL already had `/c/`, and the stop button
            // never showed within 12s: either the reply was very fast or the
            // page refused the message (rate limit dialog, too long, ...).
            if let Some((kind, text)) = self.probe_page_errors(tab_id).await {
                return Err(page_error(kind, &text));
            }
            if let Some(conversation_id) = request.conversation_id.as_deref() {
                match self
                    .confirm_submitted(
                        conversation_id,
                        &request.text,
                        self.timings.confirm_submit_wait,
                    )
                    .await
                {
                    Some(true) => {}
                    Some(false) => {
                        return Err(DriverError::ui_changed(
                            "the send click did not land: the stop button never appeared and the API does not show the message as the latest user turn",
                        )
                        .landed(Some(false)));
                    }
                    None => {
                        return Err(DriverError::new(
                            DriverErrorKind::SubmitAmbiguous,
                            "the stop button never appeared after the click and the API could not be read to confirm the send",
                        ));
                    }
                }
            } else {
                notes.push(
                    "the stop button was not observed after the click; the reply poll settles whether generation ran"
                        .to_string(),
                );
            }
        }
        Ok(Prepared {
            conversation_id,
            model_label: state.and_then(|s| s.model_label),
            notes,
        })
    }

    /// Run [`page_error_probe`] and classify; read failures are ignored (the
    /// caller already has a more specific error to report).
    async fn probe_page_errors(&self, tab_id: TabId) -> Option<(DriverErrorKind, String)> {
        match self
            .eval_as::<PageErrors>(tab_id, page_error_probe(), DEFAULT_TOOL_TIMEOUT_MS)
            .await
        {
            Ok(errors) => classify_page_error(&errors.texts),
            Err(error) => {
                warn!("[chatgpt_web send] page error probe failed: {error}");
                None
            }
        }
    }

    /// `splitAlreadyUploaded` (`ops.ts:452-490`): any API failure falls back
    /// to uploading everything (the popup handling in `attach_files` still
    /// covers that path).
    async fn split_already_uploaded(
        &self,
        api: &ChatGptApi<'_>,
        conversation_id: &str,
        files: &[PathBuf],
    ) -> UploadSplit {
        let outcome = match api.read_conversation(conversation_id).await {
            Ok(conv) => split_already_uploaded_with(&conv, files),
            Err(error) => Err(error),
        };
        match outcome {
            Ok(split) => split,
            Err(error) => {
                info!(
                    "[chatgpt_web send] duplicate-upload pre-check failed (uploading everything): {error}"
                );
                UploadSplit {
                    fresh: files.to_vec(),
                    notes: Vec::new(),
                }
            }
        }
    }

    /// Port of `attachFiles` (`ops.ts:492-527`): `browser_upload` onto the
    /// composer's hidden file input(s), then verify one tile per file.
    async fn attach_files(&self, tab_id: TabId, files: &[PathBuf]) -> DriverResult<Attached> {
        let mut absolute: Vec<(PathBuf, bool)> = Vec::with_capacity(files.len());
        for file in files {
            if !file.exists() {
                return Err(DriverError::other(format!(
                    "file not found: {}",
                    file.display()
                )));
            }
            let path = std::path::absolute(file).unwrap_or_else(|_| file.clone());
            absolute.push((path, is_image_path(file)));
        }
        let before = self.read_tiles(tab_id).await;
        let images: Vec<&Path> = absolute
            .iter()
            .filter(|(_, image)| *image)
            .map(|(path, _)| path.as_path())
            .collect();
        let others: Vec<&Path> = absolute
            .iter()
            .filter(|(_, image)| !*image)
            .map(|(path, _)| path.as_path())
            .collect();
        if !images.is_empty() {
            self.upload_batch(tab_id, &images, &IMAGE_UPLOAD_SELECTORS)
                .await?;
        }
        if !others.is_empty() {
            self.upload_batch(tab_id, &others, &FILE_UPLOAD_SELECTORS)
                .await?;
        }
        self.verify_tiles_attached(tab_id, &before.tiles, files)
            .await
    }

    /// One `browser_upload` per selector until one succeeds.
    async fn upload_batch(
        &self,
        tab_id: TabId,
        files: &[&Path],
        selectors: &[&str],
    ) -> DriverResult<()> {
        let paths: Vec<String> = files
            .iter()
            .map(|p| p.to_string_lossy().into_owned())
            .collect();
        let mut last_error: Option<DriverError> = None;
        for selector in selectors {
            match self
                .daemon()
                .call(
                    "browser_upload",
                    json!({
                        "tabId": tab_id,
                        "selector": selector,
                        "filePaths": paths,
                        "timeoutMs": UPLOAD_TIMEOUT_MS,
                    }),
                    UPLOAD_TIMEOUT_MS,
                )
                .await
            {
                Ok(_) => return Ok(()),
                Err(error) => last_error = Some(error),
            }
        }
        Err(DriverError::ui_changed(format!(
            "file attach failed on all known selectors: {}. The composer's file input must be rediscovered.",
            last_error.map(|e| e.message).unwrap_or_default()
        )))
    }

    async fn read_tiles(&self, tab_id: TabId) -> Tiles {
        self.eval_as::<Tiles>(
            tab_id,
            page_scripts::attachment_tiles(),
            DEFAULT_TOOL_TIMEOUT_MS,
        )
        .await
        .unwrap_or_default()
    }

    /// Port of `verifyTilesAttached` (`ops.ts:546-640`): confirm one tile per
    /// file showed up (accepting ChatGPT's "name(N)" duplicate rename), and
    /// absorb the blocking "you already uploaded this file" popup. A file
    /// that never gets a tile was silently dropped (ChatGPT hard-blocks
    /// content already pending in the composer), which must fail loudly.
    async fn verify_tiles_attached(
        &self,
        tab_id: TabId,
        before_tiles: &[String],
        files: &[PathBuf],
    ) -> DriverResult<Attached> {
        let mut notes = Vec::new();
        let deadline = Instant::now() + self.timings.tiles_deadline;
        let mut popup_text: Option<String> = None;
        let mut tiles: Vec<String>;
        loop {
            let dialog: DialogResult = self
                .eval_as(
                    tab_id,
                    page_scripts::dismiss_upload_dialog(),
                    DEFAULT_TOOL_TIMEOUT_MS,
                )
                .await?;
            if dialog.found {
                if dialog.text.is_some() {
                    popup_text = dialog.text.clone();
                }
                if dialog.dismissed == Some(false) {
                    return Err(DriverError::ui_changed(format!(
                        "a popup is blocking the composer and was not auto-dismissed (multiple buttons): \"{}\" [{}]",
                        dialog.text.as_deref().unwrap_or_default(),
                        dialog.buttons.join(" | ")
                    )));
                }
            }
            tiles = self.read_tiles(tab_id).await.tiles;
            if tiles.len() >= before_tiles.len() + files.len() {
                // The duplicate-upload popup can mount a beat AFTER the tiles
                // do — give it one more look so it never lingers into the send.
                tokio::time::sleep(self.timings.late_popup_delay).await;
                let late: DialogResult = self
                    .eval_as(
                        tab_id,
                        page_scripts::dismiss_upload_dialog(),
                        DEFAULT_TOOL_TIMEOUT_MS,
                    )
                    .await?;
                if late.found && late.text.is_some() {
                    popup_text = late.text;
                }
                break;
            }
            if Instant::now() > deadline {
                break;
            }
            tokio::time::sleep(self.timings.tiles_poll).await;
        }

        // Tile selector drifted with the UI? Fall back to the send-button signal.
        if tiles.is_empty() && before_tiles.is_empty() && popup_text.is_none() {
            notes.push(
                "attachment tiles were not detected (chip UI may have changed) — relying on the send-button state only; verify the files landed by reading the conversation"
                    .to_string(),
            );
            return Ok(Attached {
                notes,
                expected_tiles: 0,
            });
        }

        let mut new_tiles = tiles.clone();
        for tile in before_tiles {
            if let Some(i) = new_tiles.iter().position(|t| t == tile) {
                new_tiles.remove(i);
            }
        }
        let mut missing: Vec<String> = Vec::new();
        let mut renamed: Vec<String> = Vec::new();
        for file in files {
            let base = file_name_of(file);
            if let Some(i) = new_tiles.iter().position(|t| *t == base) {
                new_tiles.remove(i);
                continue;
            }
            let rename = rename_regex(&base);
            if let Some(i) = new_tiles
                .iter()
                .position(|t| rename.as_ref().is_some_and(|re| re.is_match(t)))
            {
                renamed.push(format!("'{base}' → '{}'", new_tiles[i]));
                new_tiles.remove(i);
                continue;
            }
            missing.push(base);
        }

        if !missing.is_empty() {
            let list = missing
                .iter()
                .map(|m| format!("'{m}'"))
                .collect::<Vec<_>>()
                .join(", ");
            let blocked = popup_text
                .as_deref()
                .map(|t| format!(" — ChatGPT blocked it with: \"{t}\""))
                .unwrap_or_default();
            return Err(DriverError::other(format!(
                "{list} did not attach{blocked}. ChatGPT silently drops a file whose identical content is already pending in the composer; nothing was sent. Retry without the duplicate, or rename the file if it must go again."
            )));
        }
        if let Some(text) = popup_text {
            let renamed_note = if renamed.is_empty() {
                String::new()
            } else {
                format!(" (renamed by ChatGPT: {})", renamed.join(", "))
            };
            notes.push(format!(
                "ChatGPT flagged previously-uploaded file(s) (\"{text}\"); popup auto-dismissed and the files were attached anyway{renamed_note}"
            ));
        } else if !renamed.is_empty() {
            notes.push(format!(
                "ChatGPT renamed duplicate file name(s): {}",
                renamed.join(", ")
            ));
        }
        Ok(Attached {
            notes,
            expected_tiles: tiles.len() as u64,
        })
    }

    /// Port of `waitAttachmentsReady` (`ops.ts:649-684`): wait for in-flight
    /// uploads to finish (ChatGPT disables Send while a file is uploading).
    /// `min_tiles` = composer tile total expected at send time (0 = tile
    /// selector unavailable, trust the send button alone). A popup or a tile
    /// count below the target means an upload was rejected after attach —
    /// fail rather than send a message missing its file.
    async fn wait_attachments_ready(
        &self,
        tab_id: TabId,
        min_tiles: u64,
        timeout_ms: u64,
    ) -> DriverResult<Vec<String>> {
        static BENIGN: OnceLock<Regex> = OnceLock::new();
        let benign = BENIGN.get_or_init(|| {
            #[expect(clippy::expect_used, reason = "the pattern is a compile-time literal")]
            Regex::new("(?i)j[áa] carregou|already uploaded").expect("benign regex must compile")
        });
        let started = Instant::now();
        let timeout = Duration::from_millis(timeout_ms);
        loop {
            let state = self.composer_state_on(tab_id).await?;
            let dialog: DialogResult = self
                .eval_as(
                    tab_id,
                    page_scripts::dismiss_upload_dialog(),
                    DEFAULT_TOOL_TIMEOUT_MS,
                )
                .await?;
            if dialog.found && !benign.is_match(dialog.text.as_deref().unwrap_or_default()) {
                // The known duplicate-upload warning is benign (file attaches
                // anyway, dismissUploadDialog already clicked OK); anything
                // else mid-upload is an upload failure.
                return Err(DriverError::other(format!(
                    "a popup appeared while waiting for uploads to finish: \"{}\"{} — treating the upload as failed so the message is not sent without its file.",
                    dialog.text.as_deref().unwrap_or_default(),
                    if dialog.dismissed == Some(true) {
                        " (auto-dismissed)"
                    } else {
                        ""
                    }
                )));
            }
            if state.send_visible && state.send_enabled && state.attachments >= min_tiles {
                // Grace note for the selector-drift path: chips undetectable but send ready.
                return Ok(if min_tiles == 0 && state.attachments == 0 {
                    vec![
                        "attachment chips could not be verified before sending — confirm by reading the conversation that the files arrived"
                            .to_string(),
                    ]
                } else {
                    Vec::new()
                });
            }
            if started.elapsed() > timeout {
                return Err(DriverError::timeout(format!(
                    "attachments not ready after {timeout_ms}ms (tiles seen: {}/{min_tiles}, sendEnabled: {})",
                    state.attachments, state.send_enabled
                )));
            }
            tokio::time::sleep(self.timings.attachments_poll).await;
        }
    }

    /// Port of `confirmSubmitted` (`ops.ts:694-713`): after an ambiguous
    /// submit failure, did our message actually land? Polls the API for up to
    /// `wait` (the backend can lag a submit by a few seconds). `Some(true)` =
    /// it is the latest user turn; `Some(false)` = a successful read says it
    /// is not there; `None` = the API was unreadable (verdict unknown). If the
    /// previous user turn already had identical text, this reads as "landed"
    /// — erring toward not duplicating the send.
    pub(crate) async fn confirm_submitted(
        &self,
        conversation_id: &str,
        message: &str,
        wait: Duration,
    ) -> Option<bool> {
        let anchor = anchor_of(message);
        let deadline = Instant::now() + wait;
        let mut saw_read = false;
        loop {
            match self.read_conversation(conversation_id, false).await {
                Ok(conv) => {
                    saw_read = true;
                    if last_user_matches(&conv, &anchor) {
                        return Some(true);
                    }
                }
                Err(error) => {
                    info!("[chatgpt_web send] post-submit verification read failed: {error}");
                }
            }
            if Instant::now() >= deadline {
                return if saw_read { Some(false) } else { None };
            }
            tokio::time::sleep(self.timings.confirm_submit_poll).await;
        }
    }

    // ---- reading / waiting --------------------------------------------------

    /// `GET /backend-api/conversation/<id>` normalized, on the best read tab.
    pub(crate) async fn read_conversation(
        &self,
        conversation_id: &str,
        include_thoughts: bool,
    ) -> DriverResult<Conversation> {
        // FORK (verified live): this is the poll read. Retrying a 429 three
        // times here (2/5/10 s) turned one throttled poll into a burst of four
        // requests that kept the account throttled; surface the 429 at once
        // and let the poll loop back off instead.
        let api = self
            .api_for(Some(conversation_id))
            .await?
            .with_backoff(Vec::new());
        let raw = api.get_conversation(conversation_id).await?;
        Ok(normalize_with(&raw, NormalizeOptions { include_thoughts }))
    }

    /// Port of `waitReply` (`ops.ts:725-807`): poll the backend until the last
    /// assistant reply is complete (or `wait` runs out). DOM is only a hint;
    /// the API is the source of truth, so this works even when the tab has
    /// navigated elsewhere (Pro runs continue server-side).
    ///
    /// The provider's poll loop (M4) streams deltas instead; this stays as the
    /// documented completion rule and for the live tests.
    pub(crate) async fn wait_reply(
        &self,
        conversation_id: &str,
        wait: Duration,
        anchor_user_text: Option<&str>,
    ) -> DriverResult<ReplyWait> {
        let started = Instant::now();
        let deadline = started + wait;
        let anchor = anchor_user_text.map(anchor_of);
        let mut conv: Option<Conversation> = None;
        let mut prev_fingerprint: Option<u64> = None;
        let mut not_found_logged = false;
        loop {
            match self.read_conversation(conversation_id, true).await {
                Ok(read) => conv = Some(read),
                Err(error) => {
                    // Right after sending, the conversation may 404
                    // ("inaccessible") for a few seconds until the backend
                    // commits it — that is normal, not fatal.
                    let transient = error.kind == DriverErrorKind::ConversationNotFound
                        || error.message.contains("conversation_inaccessible");
                    if !transient || !not_found_logged {
                        info!(
                            "[chatgpt_web wait] read failed{}: {error}",
                            if transient { " (transient)" } else { "" }
                        );
                    }
                    if transient {
                        not_found_logged = true;
                    }
                }
            }
            if let Some(current) = conv.as_ref() {
                let check = check_reply(current, anchor.as_deref(), prev_fingerprint);
                prev_fingerprint = Some(check.fingerprint);
                if check.done {
                    let reply_text = check
                        .reply
                        .as_ref()
                        .map(|r| r.text.clone())
                        .filter(|t| !t.is_empty());
                    return Ok(ReplyWait {
                        conversation_id: conversation_id.to_string(),
                        status: ReplyStatus::Complete,
                        reply: check.reply,
                        reply_text,
                        elapsed: started.elapsed(),
                        conversation: conv,
                        note: None,
                    });
                }
            }
            if Instant::now() >= deadline {
                let reply = conv.as_ref().and_then(last_assistant_reply).cloned();
                let reply_text = reply
                    .as_ref()
                    .map(|r| r.text.clone())
                    .filter(|t| !t.is_empty());
                return Ok(ReplyWait {
                    conversation_id: conversation_id.to_string(),
                    status: ReplyStatus::Generating,
                    reply,
                    reply_text,
                    elapsed: started.elapsed(),
                    conversation: conv,
                    note: Some(
                        "still generating — keep waiting (Pro runs can take many minutes)"
                            .to_string(),
                    ),
                });
            }
            tokio::time::sleep(self.timings.reply_poll).await;
        }
    }

    // ---- stop ---------------------------------------------------------------

    /// Port of `stop` (`ops.ts:811-838`): click the stop button on the
    /// conversation's tab, retrying for 8s (the control only exists while the
    /// client streams; after a navigation the page re-attaches to a live run
    /// within a couple of seconds).
    pub(crate) async fn stop(&self, conversation_id: Option<&str>) -> DriverResult<StopOutcome> {
        self.tabs
            .with_tab_for(conversation_id, |tab_id| async move {
                if let Some(conversation_id) = conversation_id {
                    self.tabs
                        .show_conversation_on(tab_id, Some(conversation_id))
                        .await?;
                    self.tabs.bind(tab_id, Some(conversation_id));
                }
                let started = Instant::now();
                loop {
                    let result: ClickStopResult = self
                        .eval_as(tab_id, page_scripts::click_stop(), EVAL_STOP_TIMEOUT_MS)
                        .await?;
                    if result.ok {
                        return Ok(StopOutcome {
                            ok: true,
                            detail: "generation stopped".to_string(),
                        });
                    }
                    if started.elapsed() > self.timings.stop_window {
                        return Ok(StopOutcome {
                            ok: false,
                            detail: result.error.unwrap_or_else(|| {
                                "no active generation found in the tab — it may have already finished"
                                    .to_string()
                            }),
                        });
                    }
                    tokio::time::sleep(self.timings.stop_poll).await;
                }
            })
            .await
    }

    // ---- downloads ----------------------------------------------------------

    /// Port of `downloadAssets` (`ops.ts:842-897`): fetch every asset of the
    /// conversation (optionally of one message) inside the page and write it
    /// under `dir`. Runs on an exclusively locked idle tab: the chunked page
    /// pickup must not interleave with other work on the same tab.
    pub(crate) async fn download_assets(
        &self,
        conversation_id: &str,
        message_id: Option<&str>,
        dir: &Path,
    ) -> DriverResult<Vec<SavedAsset>> {
        self.tabs
            .with_tab_for(None, |tab_id| {
                self.download_assets_unlocked(tab_id, conversation_id, message_id, dir)
            })
            .await
    }

    async fn download_assets_unlocked(
        &self,
        tab_id: TabId,
        conversation_id: &str,
        message_id: Option<&str>,
        dir: &Path,
    ) -> DriverResult<Vec<SavedAsset>> {
        let conv = self.read_conversation(conversation_id, false).await?;
        let assets: Vec<_> = conv
            .turns
            .iter()
            .filter(|t| !t.assets.is_empty())
            .filter(|t| message_id.is_none_or(|id| t.message_id == id))
            .flat_map(|t| t.assets.iter().cloned())
            .collect();
        if assets.is_empty() {
            return Err(DriverError::other(format!(
                "no downloadable assets found in conversation {conversation_id}{}. Generated images/files appear as assets on assistant/tool turns.",
                message_id
                    .map(|id| format!(" for message {id}"))
                    .unwrap_or_default()
            )));
        }
        std::fs::create_dir_all(dir).map_err(|error| {
            DriverError::other(format!(
                "could not create download dir {}: {error}",
                dir.display()
            ))
        })?;

        let mut saved = Vec::with_capacity(assets.len());
        for asset in assets {
            // The download URL lives on chatgpt.com and requires the browser's
            // cookies, so the file is fetched inside the page and shipped out
            // as base64 chunks.
            let staged: StagedDownload = self
                .eval_as(
                    tab_id,
                    page_scripts::stage_download(&asset.file_id),
                    DOWNLOAD_STAGE_TIMEOUT_MS,
                )
                .await?;
            if !staged.ok {
                return Err(DriverError::other(format!(
                    "download of {} failed in page: {}. If this asset is old its link may have expired.",
                    asset.file_id,
                    staged.error.as_deref().unwrap_or("unknown")
                )));
            }
            let total = staged.b64len.unwrap_or(0);
            let mut b64 = String::with_capacity(total as usize);
            let mut offset = 0;
            while offset < total {
                let is_last = offset + DOWNLOAD_CHUNK_B64 >= total;
                let part: DownloadChunk = self
                    .eval_as(
                        tab_id,
                        page_scripts::read_download_chunk(
                            &asset.file_id,
                            offset,
                            DOWNLOAD_CHUNK_B64,
                            is_last,
                        ),
                        DOWNLOAD_CHUNK_TIMEOUT_MS,
                    )
                    .await?;
                match part.chunk {
                    Some(chunk) if part.ok => b64.push_str(&chunk),
                    _ => {
                        return Err(DriverError::other(format!(
                            "chunked read of {} failed at {offset}: {}",
                            asset.file_id,
                            part.error.as_deref().unwrap_or("unknown")
                        )));
                    }
                }
                offset += DOWNLOAD_CHUNK_B64;
            }
            let bytes = base64::prelude::BASE64_STANDARD
                .decode(b64.as_bytes())
                .map_err(|error| {
                    DriverError::other(format!(
                        "asset {} is not valid base64: {error}",
                        asset.file_id
                    ))
                })?;
            let name = pick_file_name(
                asset.name.as_deref(),
                &asset.file_id,
                staged.mime.as_deref().unwrap_or_default(),
                staged.cd.as_deref().unwrap_or_default(),
            );
            let file = unique_path(dir, &name);
            std::fs::write(&file, &bytes).map_err(|error| {
                DriverError::other(format!("could not write {}: {error}", file.display()))
            })?;
            saved.push(SavedAsset {
                file,
                bytes: bytes.len() as u64,
                file_id: asset.file_id.clone(),
                kind: asset.kind,
            });
        }
        Ok(saved)
    }
}

/// An error surfaced by the page itself (dialog/toast), with the retry verdict
/// the table assigns: a refused message did not land; an upstream failure
/// may have.
fn page_error(kind: DriverErrorKind, text: &str) -> DriverError {
    let landed = match kind {
        DriverErrorKind::Upstream => None,
        _ => Some(false),
    };
    let message = match kind {
        DriverErrorKind::RateLimited => {
            format!("ChatGPT refused the message: \"{text}\" (rate limited)")
        }
        DriverErrorKind::MessageTooLong => {
            format!("ChatGPT rejected the message as too long: \"{text}\"")
        }
        DriverErrorKind::LoginRequired => format!("ChatGPT asks to log in again: \"{text}\""),
        _ => format!("ChatGPT reported an error after the send: \"{text}\""),
    };
    DriverError::new(kind, message).landed(landed)
}

impl std::fmt::Debug for ChatGptOps {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ChatGptOps")
            .field("base_url", &self.base_url)
            .field("timings", &self.timings)
            .finish_non_exhaustive()
    }
}

#[cfg(test)]
#[path = "ops_tests.rs"]
mod tests;
