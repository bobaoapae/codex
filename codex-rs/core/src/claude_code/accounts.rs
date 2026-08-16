//! FORK: account pinning and failover for the `claude_code` provider.
//!
//! The provider historically inherited `CLAUDE_CONFIG_DIR` from the ambient
//! environment, so the Claude account backing a Codex thread was decided by
//! whatever shell happened to launch Codex. With `[claude_code].account_dirs`
//! configured, every spawn is pinned to one directory of that list, and
//! account-level failures (usage limit, expired login) fail over to the next.
//!
//! Health state is shared with external tooling (the claude-agents MCP bridge)
//! through a small JSON file in `CODEX_HOME`, which also mirrors the configured
//! directory list and carries a user-selected preferred account.

use serde::Deserialize;
use serde::Serialize;
use std::collections::BTreeMap;
use std::path::Path;
use std::path::PathBuf;
use std::time::SystemTime;
use std::time::UNIX_EPOCH;
use tracing::warn;

/// File name under `CODEX_HOME` shared with the claude-agents MCP bridge.
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
    /// Anything else — not an account problem, so not worth failing over.
    Other,
}

impl FailureClass {
    pub(crate) fn is_account_level(&self) -> bool {
        !matches!(self, FailureClass::Other)
    }

    fn reason(&self) -> &'static str {
        match self {
            FailureClass::UsageLimit { .. } => "usage_limit",
            FailureClass::Auth => "auth",
            FailureClass::Other => "other",
        }
    }
}

/// Classifies a CLI failure from its error text.
///
/// The strings come from the Claude Code CLI's `result` error events and
/// stderr, e.g. "You've hit your weekly limit · resets Aug 17, 5am
/// (America/Sao_Paulo)" or "OAuth token has expired · Please run /login".
pub(crate) fn classify_failure(text: &str) -> FailureClass {
    let lower = text.to_lowercase();
    if lower.contains("limit")
        && (lower.contains("hit your") || lower.contains("limit reached") || lower.contains("resets"))
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
    if cfg!(windows) { key.to_lowercase() } else { key }
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
            window.get("utilization").and_then(serde_json::Value::as_f64),
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
        let fresh =
            snapshot.is_some_and(|snapshot| now.saturating_sub(snapshot.fetched_at_ms) < USAGE_TTL_MS);
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
        match std::fs::read(path) {
            Ok(raw) => serde_json::from_slice(&raw).unwrap_or_default(),
            Err(_) => Self::default(),
        }
    }

    fn save(&self, path: &Path) {
        let Ok(raw) = serde_json::to_vec_pretty(self) else {
            return;
        };
        let tmp = path.with_extension("json.tmp");
        let write_result = std::fs::write(&tmp, raw).and_then(|()| {
            // `rename` fails on Windows when the target exists; replace it.
            if path.exists() {
                std::fs::remove_file(path)?;
            }
            std::fs::rename(&tmp, path)
        });
        if let Err(err) = write_result {
            warn!("claude_code: failed to persist account state: {err}");
        }
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
    /// 1. The user-selected preferred account (a deliberate choice).
    /// 2. `sticky` — the account already serving this thread, so an ongoing
    ///    conversation keeps its Claude session until the account is spent.
    /// 3. Remaining accounts with known usage, **least remaining headroom
    ///    first**: the protocol drains the account closest to its limit and
    ///    keeps the fresher ones in reserve.
    /// 4. Accounts with unknown usage, in config order.
    /// 5. Accounts that look spent (no headroom) or are on a failure cooldown —
    ///    still attempted last rather than skipped, so stale local state can
    ///    never dead-lock the provider; hitting their limit fails over anyway.
    pub(crate) async fn resolve(
        account_dirs: &[PathBuf],
        state_path: Option<&Path>,
        sticky: Option<&Path>,
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
                state.save(path);
            }
            return Self {
                candidates: vec![None],
                state_path,
            };
        }

        let now = now_ms();
        // Usage only matters when there is a choice to make.
        if usable.len() > 1 {
            dirty |= refresh_usage(&mut state, &usable, now).await;
        }
        if dirty && let Some(path) = state_path.as_deref() {
            state.save(path);
        }

        let preferred_key = state
            .preferred_dir
            .as_deref()
            .map(|preferred| dir_key(Path::new(preferred)));
        let sticky_key = sticky.map(dir_key);

        let mut pinned: Vec<PathBuf> = Vec::new();
        let mut drain: Vec<(f64, usize, PathBuf)> = Vec::new();
        let mut unknown: Vec<PathBuf> = Vec::new();
        let mut spent: Vec<PathBuf> = Vec::new();
        let mut cooling: Vec<PathBuf> = Vec::new();
        for (index, dir) in usable.iter().enumerate() {
            let key = dir_key(dir);
            if state.is_cooling(dir, now) {
                cooling.push((*dir).clone());
                continue;
            }
            let is_preferred = preferred_key.as_deref() == Some(key.as_str());
            let is_sticky = sticky_key.as_deref() == Some(key.as_str());
            if is_preferred || is_sticky {
                // Preferred outranks sticky when both exist.
                if is_preferred {
                    pinned.insert(0, (*dir).clone());
                } else {
                    pinned.push((*dir).clone());
                }
                continue;
            }
            match state.usage.get(&key).and_then(UsageSnapshot::remaining_pct) {
                Some(remaining) if remaining <= 0.0 => spent.push((*dir).clone()),
                Some(remaining) => drain.push((remaining, index, (*dir).clone())),
                None => unknown.push((*dir).clone()),
            }
        }
        drain.sort_by(|a, b| {
            a.0.partial_cmp(&b.0)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then(a.1.cmp(&b.1))
        });

        let mut candidates: Vec<Option<PathBuf>> = Vec::new();
        candidates.extend(pinned.into_iter().map(Some));
        candidates.extend(drain.into_iter().map(|(_, _, dir)| Some(dir)));
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
            FailureClass::Other => return,
        };
        let mut state = AccountsFile::load(path);
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
        state.save(path);
    }

    /// Clears any recorded failure once the account served a turn again.
    pub(crate) fn mark_success(&self, dir: Option<&Path>) {
        let (Some(path), Some(dir)) = (self.state_path.as_deref(), dir) else {
            return;
        };
        let mut state = AccountsFile::load(path);
        if state.accounts.remove(&dir_key(dir)).is_some() {
            state.save(path);
        }
    }
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
        state.save(&state_path);
        let plan = TurnAccounts::resolve(&dirs, Some(&state_path), None).await;
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
        let plan = TurnAccounts::resolve(&dirs, Some(&state_path), None).await;
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
        let plan = TurnAccounts::resolve(&dirs, None, None).await;
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
        state.save(&state_path);

        let plan = TurnAccounts::resolve(&dirs, Some(&state_path), None).await;
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
        state.save(&state_path);

        let plan = TurnAccounts::resolve(&dirs, Some(&state_path), Some(&dirs[0])).await;
        assert_eq!(plan.candidates.first(), Some(&Some(dirs[0].clone())));

        // A deliberate selection outranks the sticky account.
        let mut state = AccountsFile::load(&state_path);
        state.preferred_dir = Some(dirs[2].to_string_lossy().into_owned());
        state.save(&state_path);
        let plan = TurnAccounts::resolve(&dirs, Some(&state_path), Some(&dirs[0])).await;
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

        let plan = TurnAccounts::resolve(&dirs, Some(&state_path), None).await;
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
}
