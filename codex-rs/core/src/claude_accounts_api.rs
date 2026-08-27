//! FORK: the Claude account surface, for callers outside a turn.
//!
//! `claude_accounts` and `claude_account_select` are tools, reachable only from
//! inside a running agent turn. The person the accounts belong to had no way to
//! see them at all: which one a thread was spending, how much headroom was left,
//! which one new work would pick. This is the same state behind a plain
//! function, so `codex account claude list|use` can show and change it.

use crate::claude_code::AccountAlias;
use crate::claude_code::accounts;
use crate::claude_code::resolve_account_alias;
use crate::config::Config;

pub use crate::claude_code::accounts::AccountStatus as ClaudeAccountStatus;

/// Lists the configured Claude accounts.
///
/// `refresh_usage` decides whether to spend a network round trip per account;
/// the cached values are used either way, so a refusal to refresh still shows
/// what is known rather than nothing.
pub async fn list(
    config: &Config,
    refresh_usage: bool,
) -> Result<Vec<ClaudeAccountStatus>, String> {
    let state_path = state_path(config)?;
    Ok(accounts::list_accounts(
        &config.claude_code_account_dirs,
        Some(&state_path),
        refresh_usage,
    )
    .await)
}

/// Records which account new Claude work should try first.
///
/// Running agents keep the account they already resumed against: changing it
/// under them would cost each one a full transcript replay for no benefit.
/// Returns the label of the selected account, or `None` when the preference was
/// cleared.
pub fn select(config: &Config, alias: &str) -> Result<Option<String>, String> {
    let state_path = state_path(config)?;
    match resolve_account_alias(&config.claude_code_account_dirs, alias)? {
        AccountAlias::Auto => {
            accounts::select_account(&state_path, None);
            Ok(None)
        }
        AccountAlias::Dir(dir) => {
            accounts::select_account(&state_path, Some(&dir));
            Ok(Some(accounts::account_label(Some(&dir))))
        }
    }
}

fn state_path(config: &Config) -> Result<std::path::PathBuf, String> {
    if config.claude_code_account_dirs.is_empty() {
        return Err(
            "no Claude accounts are configured; set `[claude_code].account_dirs` in config.toml"
                .to_string(),
        );
    }
    Ok(config
        .codex_home
        .to_path_buf()
        .join(accounts::ACCOUNTS_STATE_FILE_NAME))
}
