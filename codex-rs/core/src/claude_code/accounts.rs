//! FORK: account pinning and failover for the `claude_code` provider.
//!
//! The provider historically inherited `CLAUDE_CONFIG_DIR` from the ambient
//! environment, so the Claude account backing a Codex thread was decided by
//! whatever shell happened to launch Codex. With `[claude_code].account_dirs`
//! configured, every spawn is pinned to one directory of that list, and
//! account-level failures (usage limit, expired login) fail over to the next.
//!
//! Health state is persisted in a small JSON file in `CODEX_HOME`, which also
//! mirrors the configured directory list and carries a user-selected preferred
//! account. (It was once shared with an external `claude_agents` MCP bridge;
//! that path is retired.)

use codex_config::config_toml::ClaudeCodeAccountSelection;
use serde::Deserialize;
use serde::Serialize;
use std::collections::BTreeMap;
use std::path::Path;
use std::path::PathBuf;
use std::time::SystemTime;
use std::time::UNIX_EPOCH;
use tracing::warn;

/// File name under `CODEX_HOME` holding per-account health and preference.
pub(crate) const ACCOUNTS_STATE_FILE_NAME: &str = "claude_code_accounts.json";

/// How long an account sits out after a usage-limit failure. Deliberately
/// short: a wrong guess only costs one fast failed spawn, while the real reset
/// time is kept as a display hint.
const USAGE_LIMIT_COOLDOWN_MS: u64 = 15 * 60 * 1000;

/// How long an account sits out after an auth failure. The cooldown also lifts
/// early when `.credentials.json` changes, i.e. when the user logs in again.
const AUTH_COOLDOWN_MS: u64 = 5 * 60 * 1000;

/// How long a fetched usage snapshot stays fresh.
const USAGE_TTL_MS: u64 = 5 * 60 * 1000;

/// Minimum gap between usage-fetch attempts for one account, so a dead network
/// cannot stall every turn on the fetch timeout.
const USAGE_RETRY_MS: u64 = 60 * 1000;

const USAGE_FETCH_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(4);

/// The endpoint behind the CLI's `/usage` screen. Read-only; the account's
/// OAuth token is used for nothing but this lookup.
const USAGE_ENDPOINT: &str = "https://api.anthropic.com/api/oauth/usage";

/// Why an attempt against one account failed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum FailureClass {
    /// The account ran into a usage limit (5-hour window, weekly, …).
    UsageLimit { reset_hint: Option<String> },
    /// The account needs a fresh login (expired/revoked OAuth, logged out).
    Auth,
    /// FORK: the CLI ended the turn for its own reasons — an interrupt, a
    /// tool-use cap. Not an account problem, and explicitly *not* worth failing
    /// over: another account would hit the same wall.
    Transient,
    /// FORK: Anthropic itself failed — a 529 overload, a 5xx, a dropped
    /// connection. Not an account problem either, so failing over buys nothing,
    /// but unlike everything else here it is worth trying again on the same
    /// account after a pause.
    ServerError,
    /// Anything else — not an account problem, so not worth failing over.
    Other,
}

impl FailureClass {
    pub(crate) fn is_account_level(&self) -> bool {
        matches!(self, FailureClass::UsageLimit { .. } | FailureClass::Auth)
    }

    /// FORK: whether the same account is worth another attempt after a pause.
    pub(crate) fn is_retryable_in_place(&self) -> bool {
        matches!(self, FailureClass::ServerError)
    }

    fn reason(&self) -> &'static str {
        match self {
            FailureClass::UsageLimit { .. } => "usage_limit",
            FailureClass::Auth => "auth",
            FailureClass::Transient => "transient",
            FailureClass::ServerError => "server_error",
            FailureClass::Other => "other",
        }
    }
}

/// FORK: classifies a CLI failure from the structured fields of its `result`
/// frame, falling back to the error text.
///
/// The CLI states the shape of the failure in `subtype` (and sometimes a nested
/// `error.type`); reading that first avoids the guesswork of substring matching,
/// which cannot tell "the model mentioned a rate limit" from "this account hit
/// one". The text fallback stays for CLI versions that say nothing structured.
pub(crate) fn classify_result_failure(
    frame: &serde_json::Value,
    api_error: Option<&str>,
    text: &str,
) -> FailureClass {
    // FORK: the CLI names the API failure on the `assistant` frame it emits
    // before the `result` (`isApiErrorMessage`), and that name is the only
    // reliable way to tell "Anthropic is overloaded" from "this account is
    // spent". The `result` subtype for both is `error_during_execution`.
    if let Some(api_error) = api_error {
        match api_error {
            "overloaded" | "server_error" => return FailureClass::ServerError,
            "authentication_failed" | "billing_error" => return FailureClass::Auth,
            // The CLI reports both a plan limit and a raw 429 as `rate_limit`;
            // only the text says which. A limit message names the window and
            // its reset, anything else is Anthropic pushing back.
            "rate_limit" => {
                return match classify_failure(text) {
                    limit @ FailureClass::UsageLimit { .. } => limit,
                    _ => FailureClass::ServerError,
                };
            }
            _ => {}
        }
    }
    // The error text lives in `errors[]` on an `error_during_execution` result,
    // not in `result`.
    let fallback = frame
        .get("errors")
        .and_then(serde_json::Value::as_array)
        .and_then(|errors| errors.first())
        .and_then(|error| {
            error.as_str().map(str::to_string).or_else(|| {
                error
                    .get("message")
                    .and_then(serde_json::Value::as_str)
                    .map(str::to_string)
            })
        });
    let text = if text.trim().is_empty() {
        fallback.as_deref().unwrap_or(text)
    } else {
        text
    };
    let structured = frame
        .get("subtype")
        .and_then(serde_json::Value::as_str)
        .or_else(|| {
            frame
                .get("error")
                .and_then(|error| error.get("type"))
                .and_then(serde_json::Value::as_str)
        });
    if let Some(structured) = structured {
        let structured = structured.to_lowercase();
        if structured.contains("usage_limit") || structured.contains("rate_limit") {
            return FailureClass::UsageLimit {
                reset_hint: extract_reset_hint(text),
            };
        }
        if structured.contains("auth") || structured.contains("login") {
            return FailureClass::Auth;
        }
        // A turn the CLI ended for its own reasons — an interrupt, a tool-use
        // cap — says nothing about the account, so failing over to another one
        // would only waste a second attempt on the same wall.
        if structured.contains("max_turns")
            || structured.contains("interrupt")
            || structured.contains("cancel")
        {
            return FailureClass::Transient;
        }
    }
    let classified = classify_failure(text);
    if matches!(classified, FailureClass::Other) && structured.is_some() {
        warn!(
            "claude_code: unrecognized failure subtype {structured:?}; treating as non-account-level"
        );
    }
    classified
}

/// Classifies a CLI failure from its error text.
///
/// The strings come from the Claude Code CLI's `result` error events and
/// stderr, e.g. "You've hit your weekly limit · resets Aug 17, 5am
/// (America/Sao_Paulo)" or "OAuth token has expired · Please run /login".
pub(crate) fn classify_failure(text: &str) -> FailureClass {
    let lower = text.to_lowercase();
    if lower.contains("limit")
        && (lower.contains("hit your")
            || lower.contains("limit reached")
            || lower.contains("resets"))
    {
        return FailureClass::UsageLimit {
            reset_hint: extract_reset_hint(text),
        };
    }
    const AUTH_MARKERS: &[&str] = &[
        "oauth",
        "/login",
        "not logged in",
        "please log in",
        "login required",
        "invalid api key",
        "authentication",
        "credential",
        "token has expired",
        "token expired",
    ];
    if AUTH_MARKERS.iter().any(|marker| lower.contains(marker)) {
        return FailureClass::Auth;
    }
    // FORK: Anthropic failing, not the account. Checked after the limit and
    // auth markers so a 429 that names a plan window still reads as a limit.
    // Never a bare "529": that digit run appears in ordinary text.
    const SERVER_MARKERS: &[&str] = &[
        "overloaded",
        "api error: 5",
        "internal server error",
        "service unavailable",
        "bad gateway",
        "gateway timeout",
        "fetch failed",
        "econnreset",
        "etimedout",
    ];
    if SERVER_MARKERS.iter().any(|marker| lower.contains(marker)) {
        return FailureClass::ServerError;
    }
    FailureClass::Other
}

/// Pulls the human-readable "resets …" tail out of a limit message, if any.
fn extract_reset_hint(text: &str) -> Option<String> {
    let lower = text.to_lowercase();
    let index = lower.find("resets")?;
    // Error text is ASCII in practice; if a multibyte prefix shifted offsets,
    // give up on the hint rather than panicking on a char boundary.
    let tail = text.get(index..)?;
    let hint: String = tail.chars().take(100).collect();
    let hint = hint.trim().trim_end_matches('.').to_string();
    if hint.is_empty() { None } else { Some(hint) }
}

/// Canonical map key for an account directory. Windows paths compare
/// case-insensitively and with either separator.
pub(crate) fn dir_key(dir: &Path) -> String {
    let mut key = dir.to_string_lossy().replace('/', "\\");
    while key.ends_with('\\') {
        key.pop();
    }
    if cfg!(windows) {
        key.to_lowercase()
    } else {
        key
    }
}

/// Best-effort identity for error messages: the account email, else the dir.
pub(crate) fn account_label(dir: Option<&Path>) -> String {
    let Some(dir) = dir else {
        return "ambient environment".to_string();
    };
    match account_email(dir) {
        Some(email) => email,
        None => dir.to_string_lossy().into_owned(),
    }
}

fn account_email(dir: &Path) -> Option<String> {
    #[derive(Deserialize)]
    struct ClaudeJson {
        #[serde(rename = "oauthAccount")]
        oauth_account: Option<OauthAccount>,
    }
    #[derive(Deserialize)]
    struct OauthAccount {
        #[serde(rename = "emailAddress")]
        email_address: Option<String>,
    }
    let raw = std::fs::read(dir.join(".claude.json")).ok()?;
    let parsed: ClaudeJson = serde_json::from_slice(&raw).ok()?;
    parsed.oauth_account?.email_address
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis() as u64)
        .unwrap_or(0)
}

fn credentials_mtime_ms(dir: &Path) -> Option<u64> {
    let metadata = std::fs::metadata(dir.join(".credentials.json")).ok()?;
    let mtime = metadata.modified().ok()?;
    mtime
        .duration_since(UNIX_EPOCH)
        .ok()
        .map(|duration| duration.as_millis() as u64)
}

/// Cross-process health/preference state, shared with the MCP bridge.
#[derive(Serialize, Deserialize, Debug, Default, Clone)]
pub(crate) struct AccountsFile {
    #[serde(default)]
    pub(crate) version: u32,
    /// Mirror of the configured `account_dirs`, so external tooling can list
    /// accounts without parsing config.toml.
    #[serde(default)]
    pub(crate) dirs: Vec<String>,
    /// User-selected account to try first (written by the MCP bridge).
    #[serde(default)]
    pub(crate) preferred_dir: Option<String>,
    #[serde(default)]
    pub(crate) accounts: BTreeMap<String, AccountHealth>,
    /// Cached usage per account, refreshed lazily; also written by the MCP
    /// bridge whenever it fetches usage for display.
    #[serde(default)]
    pub(crate) usage: BTreeMap<String, UsageSnapshot>,
}

/// Point-in-time account usage from the OAuth usage endpoint.
#[derive(Serialize, Deserialize, Debug, Default, Clone)]
pub(crate) struct UsageSnapshot {
    #[serde(default)]
    pub(crate) five_hour_pct: Option<f64>,
    #[serde(default)]
    pub(crate) weekly_pct: Option<f64>,
    /// The binding constraint: max of the window utilizations. `None` = usage
    /// unknown (never fetched successfully).
    #[serde(default)]
    pub(crate) binding_pct: Option<f64>,
    #[serde(default)]
    pub(crate) five_hour_resets_at: Option<String>,
    #[serde(default)]
    pub(crate) weekly_resets_at: Option<String>,
    #[serde(default)]
    pub(crate) fetched_at_ms: u64,
    #[serde(default)]
    pub(crate) last_attempt_ms: u64,
}

impl UsageSnapshot {
    /// Remaining headroom before the tightest window closes. `None` = unknown.
    fn remaining_pct(&self) -> Option<f64> {
        self.binding_pct.map(|pct| 100.0 - pct)
    }

    /// FORK: this usage in the shape the Codex status line already renders.
    ///
    /// Returns `None` when nothing was ever fetched. Reporting zeros instead
    /// would draw a full, healthy bar for an account we know nothing about —
    /// worse than drawing nothing, because it reads as good news.
    pub(crate) fn to_rate_limit_snapshot(
        &self,
        account_label: Option<String>,
    ) -> Option<codex_protocol::protocol::RateLimitSnapshot> {
        use codex_protocol::protocol::RateLimitWindow;
        if self.five_hour_pct.is_none() && self.weekly_pct.is_none() {
            return None;
        }
        Some(codex_protocol::protocol::RateLimitSnapshot {
            // A limit id of its own: the display keys snapshots by it, so
            // sharing the default would make the Claude window overwrite the
            // OpenAI one (and vice versa) in a session that uses both.
            limit_id: Some("claude_code".to_string()),
            limit_name: account_label,
            // No quota alias here: the Claude account's windows are reported
            // for the model the turn actually ran on.
            normal_model_slug: None,
            primary: self.five_hour_pct.map(|used_percent| RateLimitWindow {
                used_percent,
                window_minutes: Some(5 * 60),
                resets_at: parse_reset_timestamp(self.five_hour_resets_at.as_deref()),
            }),
            secondary: self.weekly_pct.map(|used_percent| RateLimitWindow {
                used_percent,
                window_minutes: Some(7 * 24 * 60),
                resets_at: parse_reset_timestamp(self.weekly_resets_at.as_deref()),
            }),
            credits: None,
            individual_limit: None,
            spend_control_reached: None,
            plan_type: None,
            rate_limit_reached_type: None,
        })
    }
}

/// FORK: the reset time as a Unix timestamp, when the CLI gave a parseable one.
fn parse_reset_timestamp(resets_at: Option<&str>) -> Option<i64> {
    let resets_at = resets_at?;
    // The endpoint reports RFC 3339; anything else is a human hint we cannot
    // place on a clock, and an unplaced reset is better left blank than guessed.
    chrono::DateTime::parse_from_rfc3339(resets_at)
        .ok()
        .map(|parsed| parsed.timestamp())
        .or_else(|| resets_at.parse::<i64>().ok())
}

/// FORK: the cached usage recorded for one account, without a network call.
pub(crate) fn cached_usage(state_path: &Path, dir: Option<&Path>) -> Option<UsageSnapshot> {
    let dir = dir?;
    let state: AccountsFile = super::state_file::read(state_path);
    state.usage.get(&dir_key(dir)).cloned()
}

fn oauth_access_token(dir: &Path) -> Option<String> {
    #[derive(Deserialize)]
    struct Credentials {
        #[serde(rename = "claudeAiOauth")]
        claude_ai_oauth: Option<Oauth>,
    }
    #[derive(Deserialize)]
    struct Oauth {
        #[serde(rename = "accessToken")]
        access_token: Option<String>,
    }
    let raw = std::fs::read(dir.join(".credentials.json")).ok()?;
    serde_json::from_slice::<Credentials>(&raw)
        .ok()?
        .claude_ai_oauth?
        .access_token
        .filter(|token| !token.is_empty())
}

/// Fetches the account's usage. `None` on any failure (no token, network,
/// non-200, unparseable) — the caller keeps the previous snapshot.
async fn fetch_usage(dir: &Path) -> Option<UsageSnapshot> {
    let token = oauth_access_token(dir)?;
    let client = reqwest::Client::builder()
        .timeout(USAGE_FETCH_TIMEOUT)
        .build()
        .ok()?;
    let response = client
        .get(USAGE_ENDPOINT)
        .bearer_auth(token)
        .header("anthropic-beta", "oauth-2025-04-20")
        .send()
        .await
        .ok()?;
    if !response.status().is_success() {
        return None;
    }
    let body = response.text().await.ok()?;
    let raw: serde_json::Value = serde_json::from_str(&body).ok()?;
    let window = |name: &str| {
        let window = raw.get(name)?;
        Some((
            window
                .get("utilization")
                .and_then(serde_json::Value::as_f64),
            window
                .get("resets_at")
                .and_then(serde_json::Value::as_str)
                .map(str::to_string),
        ))
    };
    let (five_hour_pct, five_hour_resets_at) = window("five_hour").unwrap_or((None, None));
    let (weekly_pct, weekly_resets_at) = window("seven_day").unwrap_or((None, None));
    let binding_pct = match (five_hour_pct, weekly_pct) {
        (Some(a), Some(b)) => Some(a.max(b)),
        (Some(a), None) => Some(a),
        (None, Some(b)) => Some(b),
        (None, None) => None,
    };
    Some(UsageSnapshot {
        five_hour_pct,
        weekly_pct,
        binding_pct,
        five_hour_resets_at,
        weekly_resets_at,
        fetched_at_ms: now_ms(),
        last_attempt_ms: now_ms(),
    })
}

/// Refreshes stale usage snapshots for the given dirs. Returns true when the
/// state changed and should be persisted.
async fn refresh_usage(state: &mut AccountsFile, dirs: &[&PathBuf], now: u64) -> bool {
    let mut changed = false;
    for dir in dirs {
        let key = dir_key(dir);
        let snapshot = state.usage.get(&key);
        let fresh = snapshot
            .is_some_and(|snapshot| now.saturating_sub(snapshot.fetched_at_ms) < USAGE_TTL_MS);
        let attempted_recently = snapshot
            .is_some_and(|snapshot| now.saturating_sub(snapshot.last_attempt_ms) < USAGE_RETRY_MS);
        if fresh || attempted_recently {
            continue;
        }
        changed = true;
        match fetch_usage(dir).await {
            Some(snapshot) => {
                state.usage.insert(key, snapshot);
            }
            None => {
                state.usage.entry(key).or_default().last_attempt_ms = now;
            }
        }
    }
    changed
}

#[derive(Serialize, Deserialize, Debug, Default, Clone)]
pub(crate) struct AccountHealth {
    #[serde(default)]
    pub(crate) cooldown_until_ms: u64,
    #[serde(default)]
    pub(crate) reason: Option<String>,
    /// Human-readable reset hint from the limit message, for display only.
    #[serde(default)]
    pub(crate) reset_hint: Option<String>,
    #[serde(default)]
    pub(crate) detail: Option<String>,
    /// `.credentials.json` mtime at failure time; a change lifts auth cooldowns
    /// early because it means the user re-authenticated.
    #[serde(default)]
    pub(crate) cred_mtime_ms: Option<u64>,
    #[serde(default)]
    pub(crate) last_failure_ms: u64,
}

impl AccountsFile {
    fn load(path: &Path) -> Self {
        super::state_file::read(path)
    }

    /// Merges this in-memory copy into whatever is on disk right now.
    ///
    /// A blind overwrite loses the concurrent edits of the other agents sharing
    /// this file — up to ten in one process. Each field is folded in instead:
    /// the dirs mirror and preference are process-wide facts, while health and
    /// usage are per account and merged key by key.
    fn merge_into(self, path: &Path) {
        super::state_file::update(path, |on_disk: &mut AccountsFile| {
            on_disk.version = self.version.max(on_disk.version);
            if !self.dirs.is_empty() {
                on_disk.dirs = self.dirs;
            }
            if self.preferred_dir.is_some() {
                on_disk.preferred_dir = self.preferred_dir;
            }
            on_disk.accounts.extend(self.accounts);
            on_disk.usage.extend(self.usage);
        });
    }

    fn is_cooling(&self, dir: &Path, now: u64) -> bool {
        let Some(health) = self.accounts.get(&dir_key(dir)) else {
            return false;
        };
        if now >= health.cooldown_until_ms {
            return false;
        }
        // A re-login rewrites the credentials file and clears an auth cooldown.
        if health.reason.as_deref() == Some("auth")
            && let Some(recorded) = health.cred_mtime_ms
            && credentials_mtime_ms(dir) != Some(recorded)
        {
            return false;
        }
        true
    }
}

/// What the turn knows about which account it should prefer.
#[derive(Debug, Clone, Copy)]
pub(crate) struct AccountPolicy<'a> {
    pub(crate) selection: ClaudeCodeAccountSelection,
    /// Headroom, in percent, the sticky account must still have under `hybrid`.
    pub(crate) sticky_min_headroom_pct: f64,
    /// The account already serving this thread, if any.
    pub(crate) sticky: Option<&'a Path>,
    /// The account this agent was pinned to when it was spawned, if any.
    pub(crate) pinned: Option<&'a Path>,
}

impl AccountPolicy<'_> {
    /// Whether the thread keeps its current account for another turn.
    ///
    /// Only `hybrid` ever gives one up early, and only against a *known* usage
    /// snapshot: unknown usage must not cost a session that is working.
    fn keeps_sticky(&self, remaining_pct: Option<f64>) -> bool {
        if self.selection != ClaudeCodeAccountSelection::Hybrid {
            return true;
        }
        match remaining_pct {
            Some(remaining) => remaining >= self.sticky_min_headroom_pct,
            None => true,
        }
    }

    fn sort_ranked(&self, ranked: &mut [RankedAccount]) {
        match self.selection {
            // Least busy first among the accounts that still have real headroom,
            // so N agents starting at once spread across the accounts instead of
            // all reading the same snapshot and piling onto the same one.
            ClaudeCodeAccountSelection::Hybrid => {
                let threshold = self.sticky_min_headroom_pct;
                ranked.sort_by(|a, b| {
                    let a_healthy = a.remaining >= threshold;
                    let b_healthy = b.remaining >= threshold;
                    b_healthy
                        .cmp(&a_healthy)
                        .then_with(|| {
                            if a_healthy {
                                a.in_flight.cmp(&b.in_flight)
                            } else {
                                std::cmp::Ordering::Equal
                            }
                        })
                        .then_with(|| {
                            b.remaining
                                .partial_cmp(&a.remaining)
                                .unwrap_or(std::cmp::Ordering::Equal)
                        })
                        .then_with(|| a.index.cmp(&b.index))
                });
            }
            ClaudeCodeAccountSelection::Drain => {
                ranked.sort_by(|a, b| {
                    a.remaining
                        .partial_cmp(&b.remaining)
                        .unwrap_or(std::cmp::Ordering::Equal)
                        .then_with(|| a.index.cmp(&b.index))
                });
            }
            ClaudeCodeAccountSelection::Config => {
                ranked.sort_by_key(|account| account.index);
            }
        }
    }
}

/// One candidate account with everything the ordering needs.
struct RankedAccount {
    remaining: f64,
    in_flight: usize,
    index: usize,
    dir: PathBuf,
}

/// Turns currently running against each account, keyed like `dir_key`.
///
/// Usage snapshots have a multi-minute TTL, so a burst of spawns would otherwise
/// read identical headroom and choose identical accounts. This is the only piece
/// of selection state that is process-local: it describes work in flight right
/// now, which no other process can observe anyway.
static IN_FLIGHT: std::sync::LazyLock<std::sync::Mutex<BTreeMap<String, usize>>> =
    std::sync::LazyLock::new(|| std::sync::Mutex::new(BTreeMap::new()));

fn in_flight_counts() -> BTreeMap<String, usize> {
    IN_FLIGHT
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone()
}

/// Marks an account busy for as long as the guard lives.
pub(crate) struct InFlightGuard {
    key: Option<String>,
}

impl InFlightGuard {
    pub(crate) fn acquire(dir: Option<&Path>) -> Self {
        let Some(dir) = dir else {
            return Self { key: None };
        };
        let key = dir_key(dir);
        *IN_FLIGHT
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .entry(key.clone())
            .or_insert(0) += 1;
        Self { key: Some(key) }
    }
}

impl Drop for InFlightGuard {
    fn drop(&mut self) {
        let Some(key) = self.key.take() else {
            return;
        };
        let mut in_flight = IN_FLIGHT
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(count) = in_flight.get_mut(&key) {
            *count = count.saturating_sub(1);
            if *count == 0 {
                in_flight.remove(&key);
            }
        }
    }
}

/// Account plan for one turn: which directories to try, in order.
pub(crate) struct TurnAccounts {
    /// `None` means "no pinning — inherit the ambient environment" and is only
    /// used when no account dirs are configured or none is usable.
    pub(crate) candidates: Vec<Option<PathBuf>>,
    state_path: Option<PathBuf>,
}

impl TurnAccounts {
    /// Resolves the attempt order from config + shared state.
    ///
    /// Policy, in priority order:
    /// 1. The account this agent was pinned to at spawn time (`spawn_agent`'s
    ///    `account`) — an explicit, per-agent choice.
    /// 2. The user-selected preferred account (a deliberate, session-wide one).
    /// 3. `sticky` — the account already serving this thread — while it still
    ///    has headroom, so an ongoing conversation keeps its Claude session and
    ///    its prompt cache. `hybrid` releases it once the tightest window drops
    ///    below `sticky_min_headroom_pct`; the other policies keep it until it
    ///    is spent.
    /// 4. The remaining accounts with known usage, ordered by the policy:
    ///    `hybrid` prefers the least busy of the accounts that still have real
    ///    headroom (so a fan-out spreads instead of piling onto one account) and
    ///    treats the drained ones as a last resort; `drain` spends the account
    ///    closest to its limit first; `config` keeps the configured order.
    /// 5. Accounts with unknown usage, in config order.
    /// 6. Accounts that look spent (no headroom) or are on a failure cooldown —
    ///    still attempted last rather than skipped, so stale local state can
    ///    never dead-lock the provider; hitting their limit fails over anyway.
    pub(crate) async fn resolve(
        account_dirs: &[PathBuf],
        state_path: Option<&Path>,
        policy: AccountPolicy<'_>,
    ) -> Self {
        if account_dirs.is_empty() {
            return Self {
                candidates: vec![None],
                state_path: None,
            };
        }

        let state_path = state_path.map(Path::to_path_buf);
        let mut state = match state_path.as_deref() {
            Some(path) => AccountsFile::load(path),
            None => AccountsFile::default(),
        };

        // Keep the mirror fresh for external tooling.
        let dirs_mirror: Vec<String> = account_dirs
            .iter()
            .map(|dir| dir.to_string_lossy().into_owned())
            .collect();
        let mut dirty = state.dirs != dirs_mirror || state.version == 0;
        state.version = 1;
        state.dirs = dirs_mirror;

        // Deduplicated usable dirs in config order.
        let mut usable: Vec<&PathBuf> = Vec::new();
        for dir in account_dirs {
            if usable.iter().any(|seen| dir_key(seen) == dir_key(dir)) {
                continue;
            }
            if !dir.join(".credentials.json").is_file() {
                // Not logged in / overlay not mounted: unusable, skip entirely.
                warn!(
                    "claude_code: account dir has no credentials, skipping: {}",
                    dir.display()
                );
                continue;
            }
            usable.push(dir);
        }

        if usable.is_empty() {
            warn!("claude_code: no configured account dir is usable; using ambient environment");
            if dirty && let Some(path) = state_path.as_deref() {
                state.clone().merge_into(path);
            }
            return Self {
                candidates: vec![None],
                state_path,
            };
        }

        let now = now_ms();
        // Usage only matters when there is a choice to make, and `config` order
        // never consults it — no reason to spend a network round trip.
        if usable.len() > 1 && policy.selection != ClaudeCodeAccountSelection::Config {
            dirty |= refresh_usage(&mut state, &usable, now).await;
        }
        if dirty && let Some(path) = state_path.as_deref() {
            state.clone().merge_into(path);
        }

        let pinned_key = policy.pinned.map(dir_key);
        let preferred_key = state
            .preferred_dir
            .as_deref()
            .map(|preferred| dir_key(Path::new(preferred)));
        let sticky_key = policy.sticky.map(dir_key);
        let in_flight = in_flight_counts();

        // Rank 0 = per-agent pin, 1 = session preference, 2 = the thread's own
        // account. A stable sort then keeps config order within a rank.
        let mut pinned: Vec<(u8, PathBuf)> = Vec::new();
        let mut ranked: Vec<RankedAccount> = Vec::new();
        let mut unknown: Vec<PathBuf> = Vec::new();
        let mut spent: Vec<PathBuf> = Vec::new();
        let mut cooling: Vec<PathBuf> = Vec::new();
        for (index, dir) in usable.iter().enumerate() {
            let key = dir_key(dir);
            if state.is_cooling(dir, now) {
                cooling.push((*dir).clone());
                continue;
            }
            let remaining = state.usage.get(&key).and_then(UsageSnapshot::remaining_pct);
            // An explicit per-agent pin outranks everything, then the deliberate
            // session preference, then the thread's own account.
            let is_pinned = pinned_key.as_deref() == Some(key.as_str());
            let is_preferred = preferred_key.as_deref() == Some(key.as_str());
            let is_sticky = sticky_key.as_deref() == Some(key.as_str());
            if is_pinned || is_preferred || (is_sticky && policy.keeps_sticky(remaining)) {
                let rank = if is_pinned {
                    0
                } else if is_preferred {
                    1
                } else {
                    2
                };
                pinned.push((rank, (*dir).clone()));
                continue;
            }
            match remaining {
                Some(remaining) if remaining <= 0.0 => spent.push((*dir).clone()),
                Some(remaining) => ranked.push(RankedAccount {
                    remaining,
                    in_flight: in_flight.get(&key).copied().unwrap_or(0),
                    index,
                    dir: (*dir).clone(),
                }),
                None => unknown.push((*dir).clone()),
            }
        }
        policy.sort_ranked(&mut ranked);
        pinned.sort_by_key(|(rank, _)| *rank);

        let mut candidates: Vec<Option<PathBuf>> = Vec::new();
        candidates.extend(pinned.into_iter().map(|(_, dir)| Some(dir)));
        candidates.extend(ranked.into_iter().map(|account| Some(account.dir)));
        candidates.extend(unknown.into_iter().map(Some));
        candidates.extend(spent.into_iter().map(Some));
        candidates.extend(cooling.into_iter().map(Some));

        Self {
            candidates,
            state_path,
        }
    }

    /// Records an account-level failure so later turns (and other processes)
    /// deprioritize the account until its cooldown lapses.
    pub(crate) fn record_failure(&self, dir: &Path, class: &FailureClass, detail: &str) {
        let Some(path) = self.state_path.as_deref() else {
            return;
        };
        if !class.is_account_level() {
            return;
        }
        let now = now_ms();
        let cooldown = match class {
            FailureClass::UsageLimit { .. } => USAGE_LIMIT_COOLDOWN_MS,
            FailureClass::Auth => AUTH_COOLDOWN_MS,
            FailureClass::Transient | FailureClass::ServerError | FailureClass::Other => {
                return;
            }
        };
        // Read-modify-write under one lock: a sibling agent recording its own
        // failure at the same moment must not erase this one.
        super::state_file::update(path, |state: &mut AccountsFile| {
            let health = state.accounts.entry(dir_key(dir)).or_default();
            health.cooldown_until_ms = now + cooldown;
            health.reason = Some(class.reason().to_string());
            health.reset_hint = match class {
                FailureClass::UsageLimit { reset_hint } => reset_hint.clone(),
                _ => None,
            };
            health.detail = Some(detail.chars().take(300).collect());
            health.cred_mtime_ms = credentials_mtime_ms(dir);
            health.last_failure_ms = now;
        });
    }

    /// Clears any recorded failure once the account served a turn again.
    pub(crate) fn mark_success(&self, dir: Option<&Path>) {
        let (Some(path), Some(dir)) = (self.state_path.as_deref(), dir) else {
            return;
        };
        super::state_file::update(path, |state: &mut AccountsFile| {
            state.accounts.remove(&dir_key(dir));
        });
    }
}

/// FORK: one configured account, as the `claude_accounts` tool reports it.
#[derive(Debug, Clone, Serialize)]
pub struct AccountStatus {
    /// 1-based, matching what `spawn_agent(account = …)` accepts.
    pub index: usize,
    pub account: String,
    pub config_dir: String,
    /// False when the directory has no credentials: the account is configured
    /// but not logged in, and the provider skips it entirely.
    pub logged_in: bool,
    pub preferred: bool,
    pub five_hour_used_pct: Option<f64>,
    pub weekly_used_pct: Option<f64>,
    /// Headroom before the tightest window closes.
    pub remaining_pct: Option<f64>,
    pub five_hour_resets_at: Option<String>,
    pub weekly_resets_at: Option<String>,
    /// Turns this process is running against the account right now.
    pub running_turns: usize,
    /// Seconds left on a failure cooldown, if any.
    pub cooldown_seconds_left: Option<u64>,
    pub cooldown_reason: Option<String>,
    pub limit_reset_hint: Option<String>,
}

/// FORK: reports every configured account, optionally refreshing usage first.
///
/// Read-only: it never reorders anything and never spends a turn. `refresh`
/// respects the same TTL as turn selection, so asking twice in a minute does not
/// hit the network twice.
pub(crate) async fn list_accounts(
    account_dirs: &[PathBuf],
    state_path: Option<&Path>,
    refresh: bool,
) -> Vec<AccountStatus> {
    let mut state = match state_path {
        Some(path) => AccountsFile::load(path),
        None => AccountsFile::default(),
    };
    let now = now_ms();
    if refresh {
        let usable: Vec<&PathBuf> = account_dirs
            .iter()
            .filter(|dir| dir.join(".credentials.json").is_file())
            .collect();
        if refresh_usage(&mut state, &usable, now).await
            && let Some(path) = state_path
        {
            state.clone().merge_into(path);
        }
    }
    let preferred_key = state
        .preferred_dir
        .as_deref()
        .map(|preferred| dir_key(Path::new(preferred)));
    let in_flight = in_flight_counts();

    account_dirs
        .iter()
        .enumerate()
        .map(|(index, dir)| {
            let key = dir_key(dir);
            let usage = state.usage.get(&key);
            let health = state.accounts.get(&key);
            let cooling = state.is_cooling(dir, now);
            AccountStatus {
                index: index + 1,
                account: account_label(Some(dir)),
                config_dir: dir.to_string_lossy().into_owned(),
                logged_in: dir.join(".credentials.json").is_file(),
                preferred: preferred_key.as_deref() == Some(key.as_str()),
                five_hour_used_pct: usage.and_then(|usage| usage.five_hour_pct),
                weekly_used_pct: usage.and_then(|usage| usage.weekly_pct),
                remaining_pct: usage.and_then(UsageSnapshot::remaining_pct),
                five_hour_resets_at: usage.and_then(|usage| usage.five_hour_resets_at.clone()),
                weekly_resets_at: usage.and_then(|usage| usage.weekly_resets_at.clone()),
                running_turns: in_flight.get(&key).copied().unwrap_or(0),
                cooldown_seconds_left: cooling.then(|| {
                    health
                        .map(|health| health.cooldown_until_ms.saturating_sub(now) / 1000)
                        .unwrap_or_default()
                }),
                cooldown_reason: cooling
                    .then(|| health.and_then(|health| health.reason.clone()))
                    .flatten(),
                limit_reset_hint: health.and_then(|health| health.reset_hint.clone()),
            }
        })
        .collect()
}

/// FORK: records (or clears) the account new work should prefer.
///
/// Running agents keep the account they already resumed against; this only
/// changes what the next selection tries first.
pub(crate) fn select_account(state_path: &Path, dir: Option<&Path>) {
    super::state_file::update(state_path, |state: &mut AccountsFile| {
        state.version = 1;
        // `None` here means "clear the preference", which `merge_into` cannot
        // express, so this one writes through directly.
        state.preferred_dir = dir.map(|dir| dir.to_string_lossy().into_owned());
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    #[test]
    fn classifies_weekly_limit_with_reset_hint() {
        let class = classify_failure(
            "You've hit your weekly limit \u{b7} resets Aug 17, 5am (America/Sao_Paulo)",
        );
        match class {
            FailureClass::UsageLimit { reset_hint } => {
                assert_eq!(
                    reset_hint.as_deref(),
                    Some("resets Aug 17, 5am (America/Sao_Paulo)")
                );
            }
            other => panic!("expected usage limit, got {other:?}"),
        }
    }

    #[test]
    fn classifies_session_limit_without_date() {
        assert!(matches!(
            classify_failure("You've hit your usage limit \u{b7} resets 5am (America/Sao_Paulo)"),
            FailureClass::UsageLimit { .. }
        ));
    }

    #[test]
    fn classifies_auth_failures() {
        assert_eq!(
            classify_failure("OAuth token has expired \u{b7} Please run /login"),
            FailureClass::Auth
        );
        assert_eq!(classify_failure("Invalid API key"), FailureClass::Auth);
    }

    #[test]
    fn everything_else_is_not_account_level() {
        let class = classify_failure("execution error: tool loop aborted");
        assert_eq!(class, FailureClass::Other);
        assert!(!class.is_account_level());
        assert!(!class.is_retryable_in_place());
    }

    /// FORK: Anthropic failing is not the account failing. Five turns died on
    /// 529s that were classified as `Other`, so nothing retried and nothing
    /// failed over.
    #[test]
    fn anthropic_server_errors_are_retryable_in_place() {
        for text in [
            "API Error: 529 {\"type\":\"error\",\"error\":{\"type\":\"overloaded_error\"}}",
            "API Error: 500 Internal server error",
            "Service Unavailable",
            "Bad Gateway",
            "fetch failed",
            "ECONNRESET",
        ] {
            let class = classify_failure(text);
            assert_eq!(class, FailureClass::ServerError, "{text}");
            assert!(!class.is_account_level(), "{text}");
            assert!(class.is_retryable_in_place(), "{text}");
        }
        // A bare status number in prose is not a server error.
        assert_eq!(
            classify_failure("the file has 529 lines"),
            FailureClass::Other
        );
    }

    /// FORK: the `result` subtype is `error_during_execution` for every API
    /// failure; only the flagged assistant frame names which one it was.
    #[test]
    fn the_api_error_name_decides_the_class() {
        let frame = serde_json::json!({
            "type": "result",
            "subtype": "error_during_execution",
            "is_error": true,
            "errors": ["API Error: 529 overloaded"],
        });
        assert_eq!(
            classify_result_failure(&frame, Some("overloaded"), ""),
            FailureClass::ServerError
        );
        assert_eq!(
            classify_result_failure(&frame, Some("server_error"), ""),
            FailureClass::ServerError
        );
        assert_eq!(
            classify_result_failure(&frame, Some("authentication_failed"), ""),
            FailureClass::Auth
        );
        assert_eq!(
            classify_result_failure(&frame, Some("billing_error"), ""),
            FailureClass::Auth
        );
        // `rate_limit` covers both a plan limit and a raw 429; the text decides.
        assert!(matches!(
            classify_result_failure(
                &frame,
                Some("rate_limit"),
                "You've hit your weekly limit \u{b7} resets Aug 17, 5am"
            ),
            FailureClass::UsageLimit { .. }
        ));
        assert_eq!(
            classify_result_failure(&frame, Some("rate_limit"), "429 Too Many Requests"),
            FailureClass::ServerError
        );
        // With no name, the text in `errors[]` still classifies the failure.
        assert_eq!(
            classify_result_failure(&frame, None, ""),
            FailureClass::ServerError
        );
    }

    /// FORK: a server error must not put the account in cooldown -- it is not
    /// the account's fault, and the next turn should try it first again.
    #[test]
    fn a_server_error_never_records_an_account_cooldown() {
        let temp = tempfile::tempdir().expect("tempdir");
        let dirs = account_fixture(&temp, &["a"]);
        let state_path = temp.path().join("accounts.json");
        let accounts = TurnAccounts {
            candidates: vec![Some(dirs[0].clone())],
            state_path: Some(state_path.clone()),
        };
        accounts.record_failure(&dirs[0], &FailureClass::ServerError, "API Error: 529");
        assert!(
            !state_path.exists(),
            "a server error must leave the account health file alone"
        );
        accounts.record_failure(&dirs[0], &FailureClass::Auth, "OAuth token has expired");
        assert!(state_path.exists(), "an auth failure is still recorded");
    }

    fn account_fixture(temp: &tempfile::TempDir, names: &[&str]) -> Vec<PathBuf> {
        names
            .iter()
            .map(|name| {
                let dir = temp.path().join(name);
                std::fs::create_dir_all(&dir).expect("mkdir");
                std::fs::write(dir.join(".credentials.json"), "{}").expect("creds");
                dir
            })
            .collect()
    }

    /// Test policy: hybrid with the shipped 20% sticky threshold.
    fn policy<'a>(
        selection: ClaudeCodeAccountSelection,
        sticky: Option<&'a Path>,
        pinned: Option<&'a Path>,
    ) -> AccountPolicy<'a> {
        AccountPolicy {
            selection,
            sticky_min_headroom_pct: 20.0,
            sticky,
            pinned,
        }
    }

    fn fresh_snapshot(binding_pct: f64) -> UsageSnapshot {
        UsageSnapshot {
            binding_pct: Some(binding_pct),
            weekly_pct: Some(binding_pct),
            fetched_at_ms: now_ms(),
            ..UsageSnapshot::default()
        }
    }

    #[tokio::test]
    async fn resolve_orders_preferred_first_and_cooling_last() {
        let temp = tempfile::tempdir().expect("tempdir");
        let dirs = account_fixture(&temp, &["a", "b"]);
        let (dir_a, dir_b) = (dirs[0].clone(), dirs[1].clone());
        let state_path = temp.path().join(ACCOUNTS_STATE_FILE_NAME);

        // Preferred account jumps the config order.
        let state = AccountsFile {
            preferred_dir: Some(dir_b.to_string_lossy().into_owned()),
            ..AccountsFile::default()
        };
        state.clone().merge_into(&state_path);
        let plan = TurnAccounts::resolve(
            &dirs,
            Some(&state_path),
            policy(ClaudeCodeAccountSelection::Hybrid, None, None),
        )
        .await;
        assert_eq!(
            plan.candidates,
            vec![Some(dir_b.clone()), Some(dir_a.clone())]
        );

        // A cooling account drops to the back but is still attempted.
        plan.record_failure(
            &dir_b,
            &FailureClass::UsageLimit { reset_hint: None },
            "You've hit your weekly limit",
        );
        let plan = TurnAccounts::resolve(
            &dirs,
            Some(&state_path),
            policy(ClaudeCodeAccountSelection::Hybrid, None, None),
        )
        .await;
        assert_eq!(plan.candidates, vec![Some(dir_a), Some(dir_b.clone())]);

        // Success clears the record.
        plan.mark_success(Some(&dir_b));
        let reloaded = AccountsFile::load(&state_path);
        assert!(reloaded.accounts.is_empty());
    }

    #[tokio::test]
    async fn resolve_skips_dirs_without_credentials_and_falls_back_to_ambient() {
        let temp = tempfile::tempdir().expect("tempdir");
        let dir_a = temp.path().join("a");
        std::fs::create_dir_all(&dir_a).expect("mkdir");
        let dirs = vec![dir_a];
        let plan = TurnAccounts::resolve(
            &dirs,
            None,
            policy(ClaudeCodeAccountSelection::Hybrid, None, None),
        )
        .await;
        assert_eq!(plan.candidates, vec![None]);
    }

    #[tokio::test]
    async fn drain_prefers_least_remaining_and_puts_spent_last() {
        let temp = tempfile::tempdir().expect("tempdir");
        let dirs = account_fixture(&temp, &["a", "b", "c"]);
        let state_path = temp.path().join(ACCOUNTS_STATE_FILE_NAME);

        // a: 90% headroom, b: 20% headroom, c: spent.
        let mut usage = BTreeMap::new();
        usage.insert(dir_key(&dirs[0]), fresh_snapshot(10.0));
        usage.insert(dir_key(&dirs[1]), fresh_snapshot(80.0));
        usage.insert(dir_key(&dirs[2]), fresh_snapshot(100.0));
        let state = AccountsFile {
            usage,
            ..AccountsFile::default()
        };
        state.clone().merge_into(&state_path);

        let plan = TurnAccounts::resolve(
            &dirs,
            Some(&state_path),
            policy(ClaudeCodeAccountSelection::Drain, None, None),
        )
        .await;
        assert_eq!(
            plan.candidates,
            vec![
                Some(dirs[1].clone()),
                Some(dirs[0].clone()),
                Some(dirs[2].clone()),
            ]
        );
    }

    #[tokio::test]
    async fn sticky_thread_account_ranks_ahead_of_drain_but_behind_preferred() {
        let temp = tempfile::tempdir().expect("tempdir");
        let dirs = account_fixture(&temp, &["a", "b", "c"]);
        let state_path = temp.path().join(ACCOUNTS_STATE_FILE_NAME);

        // b would win the drain order, but the thread already runs on a.
        let mut usage = BTreeMap::new();
        usage.insert(dir_key(&dirs[0]), fresh_snapshot(10.0));
        usage.insert(dir_key(&dirs[1]), fresh_snapshot(80.0));
        let state = AccountsFile {
            usage,
            ..AccountsFile::default()
        };
        state.clone().merge_into(&state_path);

        let plan = TurnAccounts::resolve(
            &dirs,
            Some(&state_path),
            policy(ClaudeCodeAccountSelection::Drain, Some(&dirs[0]), None),
        )
        .await;
        assert_eq!(plan.candidates.first(), Some(&Some(dirs[0].clone())));

        // A deliberate selection outranks the sticky account.
        let mut state = AccountsFile::load(&state_path);
        state.preferred_dir = Some(dirs[2].to_string_lossy().into_owned());
        state.clone().merge_into(&state_path);
        let plan = TurnAccounts::resolve(
            &dirs,
            Some(&state_path),
            policy(ClaudeCodeAccountSelection::Drain, Some(&dirs[0]), None),
        )
        .await;
        assert_eq!(
            plan.candidates,
            vec![
                Some(dirs[2].clone()),
                Some(dirs[0].clone()),
                Some(dirs[1].clone()),
            ]
        );
    }

    #[tokio::test]
    async fn auth_cooldown_lifts_when_credentials_change() {
        let temp = tempfile::tempdir().expect("tempdir");
        let dir = temp.path().join("a");
        std::fs::create_dir_all(&dir).expect("mkdir");
        std::fs::write(dir.join(".credentials.json"), "{}").expect("creds");
        let state_path = temp.path().join(ACCOUNTS_STATE_FILE_NAME);
        let dirs = vec![dir.clone()];

        let plan = TurnAccounts::resolve(
            &dirs,
            Some(&state_path),
            policy(ClaudeCodeAccountSelection::Hybrid, None, None),
        )
        .await;
        plan.record_failure(&dir, &FailureClass::Auth, "OAuth token has expired");
        let state = AccountsFile::load(&state_path);
        assert!(state.is_cooling(&dir, now_ms()));

        // Simulate a re-login: rewrite credentials with a different mtime.
        let mut recorded = state;
        let key = dir_key(&dir);
        recorded
            .accounts
            .get_mut(&key)
            .expect("health entry")
            .cred_mtime_ms = Some(1);
        assert!(!recorded.is_cooling(&dir, now_ms()));
    }

    #[tokio::test]
    async fn hybrid_keeps_the_thread_account_while_it_has_headroom() {
        let temp = tempfile::tempdir().expect("tempdir");
        let dirs = account_fixture(&temp, &["a", "b"]);
        let state_path = temp.path().join(ACCOUNTS_STATE_FILE_NAME);

        // a has 50% left, b has 95%. Keeping a preserves its Claude session.
        let mut usage = BTreeMap::new();
        usage.insert(dir_key(&dirs[0]), fresh_snapshot(50.0));
        usage.insert(dir_key(&dirs[1]), fresh_snapshot(5.0));
        let state = AccountsFile {
            usage,
            ..AccountsFile::default()
        };
        state.clone().merge_into(&state_path);

        let plan = TurnAccounts::resolve(
            &dirs,
            Some(&state_path),
            policy(ClaudeCodeAccountSelection::Hybrid, Some(&dirs[0]), None),
        )
        .await;

        assert_eq!(
            plan.candidates,
            vec![Some(dirs[0].clone()), Some(dirs[1].clone())]
        );
    }

    #[tokio::test]
    async fn hybrid_moves_off_a_thread_account_that_ran_low() {
        let temp = tempfile::tempdir().expect("tempdir");
        let dirs = account_fixture(&temp, &["a", "b"]);
        let state_path = temp.path().join(ACCOUNTS_STATE_FILE_NAME);

        // a is down to 5% — below the 20% threshold — so the fresher account
        // wins even though switching costs a replay.
        let mut usage = BTreeMap::new();
        usage.insert(dir_key(&dirs[0]), fresh_snapshot(95.0));
        usage.insert(dir_key(&dirs[1]), fresh_snapshot(30.0));
        let state = AccountsFile {
            usage,
            ..AccountsFile::default()
        };
        state.clone().merge_into(&state_path);

        let plan = TurnAccounts::resolve(
            &dirs,
            Some(&state_path),
            policy(ClaudeCodeAccountSelection::Hybrid, Some(&dirs[0]), None),
        )
        .await;

        assert_eq!(
            plan.candidates,
            vec![Some(dirs[1].clone()), Some(dirs[0].clone())]
        );
    }

    #[tokio::test]
    async fn a_spawn_pin_outranks_the_preferred_and_sticky_accounts() {
        let temp = tempfile::tempdir().expect("tempdir");
        let dirs = account_fixture(&temp, &["a", "b", "c"]);
        let state_path = temp.path().join(ACCOUNTS_STATE_FILE_NAME);
        let state = AccountsFile {
            preferred_dir: Some(dirs[1].to_string_lossy().into_owned()),
            ..AccountsFile::default()
        };
        state.clone().merge_into(&state_path);

        let plan = TurnAccounts::resolve(
            &dirs,
            Some(&state_path),
            policy(
                ClaudeCodeAccountSelection::Hybrid,
                Some(&dirs[0]),
                Some(&dirs[2]),
            ),
        )
        .await;

        assert_eq!(
            plan.candidates,
            vec![
                Some(dirs[2].clone()),
                Some(dirs[1].clone()),
                Some(dirs[0].clone()),
            ]
        );
    }

    #[tokio::test]
    async fn hybrid_sends_a_second_agent_to_the_idle_account() {
        let temp = tempfile::tempdir().expect("tempdir");
        let dirs = account_fixture(&temp, &["a", "b"]);
        let state_path = temp.path().join(ACCOUNTS_STATE_FILE_NAME);

        // Both accounts are healthy; a is marginally fresher.
        let mut usage = BTreeMap::new();
        usage.insert(dir_key(&dirs[0]), fresh_snapshot(10.0));
        usage.insert(dir_key(&dirs[1]), fresh_snapshot(11.0));
        let state = AccountsFile {
            usage,
            ..AccountsFile::default()
        };
        state.clone().merge_into(&state_path);

        let busy = InFlightGuard::acquire(Some(&dirs[0]));
        let plan = TurnAccounts::resolve(
            &dirs,
            Some(&state_path),
            policy(ClaudeCodeAccountSelection::Hybrid, None, None),
        )
        .await;
        assert_eq!(plan.candidates.first(), Some(&Some(dirs[1].clone())));

        // Once that turn ends the headroom order decides again.
        drop(busy);
        let plan = TurnAccounts::resolve(
            &dirs,
            Some(&state_path),
            policy(ClaudeCodeAccountSelection::Hybrid, None, None),
        )
        .await;
        assert_eq!(plan.candidates.first(), Some(&Some(dirs[0].clone())));
    }
}
