//! FORK: `codex account` — manage multiple signed-in accounts.
//!
//! Stored accounts live in the vault (`codex_login::vault`); the active
//! account remains whatever `auth.json` holds, so the desktop app, the TUI,
//! and upstream `codex login`/`logout` keep working unmodified. Switching
//! rewrites the active slot only — sessions, history, and memories are
//! account-agnostic and are never touched.

mod picker;
mod render;
mod usage;

use anyhow::Context;
use anyhow::Result;
use anyhow::bail;
use codex_core::config::Config;
use codex_login::AuthCredentialsStoreMode;
use codex_login::CLIENT_ID;
use codex_login::CodexAuth;
use codex_login::RefreshTokenError;
use codex_login::ServerOptions;
use codex_login::load_auth_dot_json;
use codex_login::run_device_code_login;
use codex_login::run_login_server;
use codex_login::token_data::parse_jwt_expiration;
use codex_login::vault::AccountVault;
use codex_login::vault::VaultEntry;
use codex_login::vault::account_identity;
use codex_protocol::config_types::ForcedLoginMethod;
use codex_utils_cli::CliConfigOverrides;

use render::AccountRow;
use usage::UsagePlan;
use usage::UsageState;

/// Subcommands:
/// - `list`   — list stored accounts and their usage limits (with `--json`)
/// - `add`    — sign in another account and store it, keeping the active one
/// - `switch` — make a stored account the active one (picker without a name)
/// - `remove` — delete a stored account
#[derive(Debug, clap::Parser)]
pub struct AccountCli {
    #[clap(flatten)]
    pub config_overrides: CliConfigOverrides,

    #[command(subcommand)]
    pub subcommand: AccountSubcommand,
}

#[derive(Debug, clap::Subcommand)]
pub enum AccountSubcommand {
    /// List stored accounts with their current usage limits.
    List(ListArgs),
    /// Sign in another account and store it without touching the active one.
    Add(AddArgs),
    /// Make a stored account the active one (interactive picker without NAME).
    Switch(SwitchArgs),
    /// Remove a stored account.
    Remove(RemoveArgs),
    /// FORK: inspect and choose among the Claude Code accounts that back
    /// `claude-*` agents.
    ///
    /// These are a different thing from the ChatGPT accounts above: they are
    /// config directories for the local `claude` CLI, listed in
    /// `[claude_code].account_dirs`, and they are what a Claude subagent spends.
    /// Until now they were only visible from inside an agent turn.
    #[command(subcommand)]
    Claude(ClaudeSubcommand),
}

#[derive(Debug, clap::Subcommand)]
pub enum ClaudeSubcommand {
    /// List configured Claude accounts and their usage windows.
    List(ClaudeListArgs),
    /// Choose which Claude account new agents should try first.
    Use(ClaudeUseArgs),
}

#[derive(Debug, clap::Parser)]
pub struct ClaudeListArgs {
    /// Output as JSON.
    #[arg(long)]
    pub json: bool,

    /// Skip fetching current usage (faster, offline-friendly).
    #[arg(long)]
    pub no_usage: bool,
}

#[derive(Debug, clap::Parser)]
pub struct ClaudeUseArgs {
    /// Account to prefer: an index from `list`, a config-dir path, or part of
    /// the account email. `auto` clears the preference.
    pub account: String,
}

#[derive(Debug, clap::Parser)]
pub struct ListArgs {
    /// Output as JSON.
    #[arg(long)]
    pub json: bool,

    /// Skip fetching current usage limits (faster, offline-friendly).
    #[arg(long)]
    pub no_usage: bool,
}

#[derive(Debug, clap::Parser)]
pub struct AddArgs {
    /// Label for the stored account (defaults to the email's local part).
    #[arg(long)]
    pub label: Option<String>,

    /// Use the device-code flow instead of opening a browser.
    #[arg(long = "device-auth")]
    pub use_device_code: bool,
}

#[derive(Debug, clap::Parser)]
pub struct SwitchArgs {
    /// Account to activate: label, email, or session-id prefix.
    pub name: Option<String>,

    /// Skip fetching usage limits for the interactive picker.
    #[arg(long)]
    pub no_usage: bool,
}

#[derive(Debug, clap::Parser)]
pub struct RemoveArgs {
    /// Account to remove: label, email, or session-id prefix.
    pub name: String,

    /// Also revoke the stored tokens server-side (best effort).
    #[arg(long)]
    pub revoke: bool,

    /// Allow removing the entry that matches the currently active account.
    #[arg(long)]
    pub force: bool,
}

impl AccountCli {
    pub async fn run(self) -> Result<()> {
        let config = load_config(&self.config_overrides).await?;
        match self.subcommand {
            AccountSubcommand::List(args) => run_list(&config, args).await,
            AccountSubcommand::Add(args) => run_add(&config, args).await,
            AccountSubcommand::Switch(args) => run_switch(&config, args).await,
            AccountSubcommand::Remove(args) => run_remove(&config, args).await,
            AccountSubcommand::Claude(subcommand) => match subcommand {
                ClaudeSubcommand::List(args) => run_claude_list(&config, args).await,
                ClaudeSubcommand::Use(args) => run_claude_use(&config, args),
            },
        }
    }
}

async fn load_config(config_overrides: &CliConfigOverrides) -> Result<Config> {
    let overrides = config_overrides
        .parse_overrides()
        .map_err(|err| anyhow::anyhow!("error parsing -c overrides: {err}"))?;
    let config = Config::load_with_cli_overrides(overrides)
        .await
        .context("error loading configuration")?;
    config
        .auth_config()
        .validate()
        .context("error validating authentication configuration")?;
    Ok(config)
}

/// Everything the subcommands need to reason about stored vs. active accounts.
struct AccountsSnapshot {
    vault: AccountVault,
    entries: Vec<VaultEntry>,
    active_identity: Option<String>,
    /// Set when auth.json holds an account that is not stored in the vault.
    active_unstored: Option<codex_login::AuthDotJson>,
}

fn load_snapshot(config: &Config) -> Result<AccountsSnapshot> {
    let vault = AccountVault::new(&config.codex_home);
    let entries = vault.list().context("failed to list stored accounts")?;
    let active_auth = load_auth_dot_json(
        &config.codex_home,
        config.cli_auth_credentials_store_mode,
        config.auth_keyring_backend_kind(),
    )
    .context("failed to read the active credentials")?;

    let active_identity = active_auth.as_ref().and_then(account_identity);
    let active_unstored = match (&active_identity, active_auth) {
        (Some(identity), Some(auth))
            if !entries
                .iter()
                .any(|entry| entry.identity().as_deref() == Some(identity)) =>
        {
            Some(auth)
        }
        _ => None,
    };

    Ok(AccountsSnapshot {
        vault,
        entries,
        active_identity,
        active_unstored,
    })
}

fn build_rows(snapshot: &AccountsSnapshot) -> Vec<AccountRow> {
    let mut rows = Vec::new();

    if let Some(auth) = &snapshot.active_unstored {
        let email = auth
            .tokens
            .as_ref()
            .and_then(|tokens| tokens.id_token.email.clone());
        let plan = auth
            .tokens
            .as_ref()
            .and_then(|tokens| tokens.id_token.get_chatgpt_plan_type());
        let label = match (&email, auth.openai_api_key.is_some()) {
            (Some(email), _) => email.split('@').next().unwrap_or("active").to_string(),
            (None, true) => "api-key".to_string(),
            (None, false) => "active".to_string(),
        };
        rows.push(AccountRow {
            label: format!("{label} (not stored)"),
            email,
            plan,
            session_id: None,
            is_active: true,
            usage: UsageState::NotApplicable,
        });
    }

    for entry in &snapshot.entries {
        rows.push(AccountRow {
            label: entry.label.clone(),
            email: entry.email().map(str::to_string),
            plan: entry.plan_type(),
            session_id: Some(entry.session_id.clone()),
            is_active: entry.identity().as_deref() == snapshot.active_identity.as_deref(),
            usage: UsageState::NotApplicable,
        });
    }

    rows
}

/// Decide, per row, how to obtain usage data. Vault copies of the ACTIVE
/// account are never used for fetching: refresh tokens are single-use, and a
/// refresh through a stale vault copy would burn the rotation that auth.json
/// holds. The active account always goes through the active slot instead.
async fn attach_usage(config: &Config, snapshot: &AccountsSnapshot, rows: &mut [AccountRow]) {
    let auth_route_config = config.auth_route_config();
    let mut plans = Vec::with_capacity(rows.len());

    for row in rows.iter() {
        if row.is_active {
            let auth = CodexAuth::from_auth_storage(
                &config.codex_home,
                config.cli_auth_credentials_store_mode,
                Some(&config.chatgpt_base_url),
                config.auth_keyring_backend_kind(),
                &auth_route_config,
            )
            .await;
            plans.push(match auth {
                Ok(Some(auth)) if auth.uses_codex_backend() => UsagePlan::Fetch(Box::new(auth)),
                Ok(_) => UsagePlan::NotApplicable,
                Err(err) => UsagePlan::Unavailable(err.to_string()),
            });
            continue;
        }

        let Some(session_id) = &row.session_id else {
            plans.push(UsagePlan::NotApplicable);
            continue;
        };
        let Some(entry) = snapshot
            .entries
            .iter()
            .find(|entry| entry.session_id == *session_id)
        else {
            plans.push(UsagePlan::NotApplicable);
            continue;
        };
        if !entry.is_chatgpt() {
            plans.push(UsagePlan::NotApplicable);
            continue;
        }

        let entry = match refresh_if_expiring(&snapshot.vault, entry, &auth_route_config).await {
            Ok(entry) => entry,
            Err(RefreshTokenError::Permanent(_)) => {
                plans.push(UsagePlan::ReauthNeeded);
                continue;
            }
            Err(RefreshTokenError::Transient(err)) => {
                plans.push(UsagePlan::Unavailable(err.to_string()));
                continue;
            }
        };

        match snapshot.vault.codex_auth_for(&entry, &auth_route_config) {
            Ok(auth) if auth.uses_codex_backend() => {
                plans.push(UsagePlan::Fetch(Box::new(auth)));
            }
            Ok(_) => plans.push(UsagePlan::NotApplicable),
            Err(err) => plans.push(UsagePlan::Unavailable(err.to_string())),
        }
    }

    let states = usage::fetch_usage(config, plans).await;
    for (row, state) in rows.iter_mut().zip(states) {
        row.usage = state;
    }
}

/// Access tokens rotate on refresh, so only refresh proactively when the
/// stored access token is about to expire; otherwise use the entry as-is.
async fn refresh_if_expiring(
    vault: &AccountVault,
    entry: &VaultEntry,
    auth_route_config: &codex_login::AuthRouteConfig,
) -> std::result::Result<VaultEntry, RefreshTokenError> {
    let expires_soon = entry
        .auth
        .tokens
        .as_ref()
        .and_then(|tokens| parse_jwt_expiration(&tokens.access_token).ok().flatten())
        .is_some_and(|expiry| expiry <= chrono::Utc::now() + chrono::Duration::seconds(60));

    if expires_soon {
        vault
            .refresh_entry(&entry.session_id, auth_route_config)
            .await
    } else {
        Ok(entry.clone())
    }
}

async fn run_list(config: &Config, args: ListArgs) -> Result<()> {
    let snapshot = load_snapshot(config)?;
    if snapshot.entries.is_empty() && snapshot.active_unstored.is_none() {
        if args.json {
            println!("[]");
        } else {
            println!("No accounts. Sign in with `codex login`, then `codex account add`.");
        }
        return Ok(());
    }

    let mut rows = build_rows(&snapshot);
    if !args.no_usage {
        attach_usage(config, &snapshot, &mut rows).await;
    }

    if args.json {
        println!(
            "{}",
            serde_json::to_string_pretty(&render::rows_to_json(&rows))?
        );
    } else {
        print!("{}", render::render_table(&rows));
    }
    Ok(())
}

/// FORK: `codex account claude list`.
async fn run_claude_list(config: &Config, args: ClaudeListArgs) -> Result<()> {
    let accounts = codex_core::claude_accounts_api::list(config, !args.no_usage)
        .await
        .map_err(|err| anyhow::anyhow!("{err}"))?;
    if args.json {
        println!("{}", serde_json::to_string_pretty(&accounts)?);
        return Ok(());
    }
    if accounts.is_empty() {
        println!("No Claude accounts configured. Set `[claude_code].account_dirs` in config.toml.");
        return Ok(());
    }
    print!("{}", render::render_claude_table(&accounts));
    Ok(())
}

/// FORK: `codex account claude use`.
fn run_claude_use(config: &Config, args: ClaudeUseArgs) -> Result<()> {
    match codex_core::claude_accounts_api::select(config, &args.account)
        .map_err(|err| anyhow::anyhow!("{err}"))?
    {
        Some(label) => println!("New Claude agents will try {label} first."),
        None => println!("Cleared the Claude account preference; selection is automatic again."),
    }
    Ok(())
}

async fn run_add(config: &Config, args: AddArgs) -> Result<()> {
    if !config
        .auth_config()
        .is_login_method_allowed(ForcedLoginMethod::Chatgpt)
    {
        bail!("ChatGPT login is disabled by your configuration.");
    }

    // Run the stock login flow against the ephemeral (in-memory) store: the
    // browser/device-code flow completes exactly as `codex login` would, but
    // auth.json — and therefore the currently active account — is never
    // touched, and nothing is revoked.
    let opts = ServerOptions::new(
        config.codex_home.to_path_buf(),
        CLIENT_ID.to_string(),
        config.auth_config().effective_chatgpt_workspaces(),
        AuthCredentialsStoreMode::Ephemeral,
        config.auth_keyring_backend_kind(),
        config.auth_route_config(),
    );

    if args.use_device_code {
        run_device_code_login(opts)
            .await
            .context("device-code login failed")?;
    } else {
        let server = run_login_server(opts).context("failed to start the login server")?;
        eprintln!(
            "Starting local login server on http://localhost:{}.\nIf your browser did not open, navigate to this URL to authenticate:\n\n{}\n",
            server.actual_port, server.auth_url
        );
        server.block_until_done().await.context("login failed")?;
    }

    let payload = load_auth_dot_json(
        &config.codex_home,
        AuthCredentialsStoreMode::Ephemeral,
        config.auth_keyring_backend_kind(),
    )
    .context("failed to read the completed login")?
    .context("login completed but produced no credentials")?;

    let vault = AccountVault::new(&config.codex_home);
    let entry = vault
        .upsert(payload, args.label)
        .context("failed to store the account")?;

    println!(
        "Stored account \"{}\"{}.",
        entry.label,
        entry
            .email()
            .map(|email| format!(" ({email})"))
            .unwrap_or_default()
    );
    println!(
        "The active account was not changed. Run `codex account switch {}` to use it.",
        entry.label
    );
    Ok(())
}

async fn run_switch(config: &Config, args: SwitchArgs) -> Result<()> {
    let snapshot = load_snapshot(config)?;
    if snapshot.entries.is_empty() {
        bail!("No stored accounts. Add one with `codex account add`.");
    }

    let target = match &args.name {
        Some(name) => snapshot.vault.resolve(name)?,
        None => {
            let mut rows = build_rows(&snapshot);
            if !args.no_usage {
                attach_usage(config, &snapshot, &mut rows).await;
            }
            let Some(index) = picker::pick_account(&rows)? else {
                println!("Switch cancelled.");
                return Ok(());
            };
            let Some(session_id) = rows[index].session_id.clone() else {
                println!("That account is already active.");
                return Ok(());
            };
            snapshot.vault.resolve(&session_id)?
        }
    };

    if target.identity().as_deref() == snapshot.active_identity.as_deref() {
        println!("\"{}\" is already the active account.", target.label);
        return Ok(());
    }

    // Preserve the outgoing account (auth.json holds its freshest tokens —
    // refresh tokens are single-use, so this rotation must not be lost).
    let outgoing = load_auth_dot_json(
        &config.codex_home,
        config.cli_auth_credentials_store_mode,
        config.auth_keyring_backend_kind(),
    )
    .context("failed to read the active credentials")?;
    let outgoing_label = match outgoing {
        Some(auth) if account_identity(&auth).is_some() => {
            let entry = snapshot
                .vault
                .upsert(auth, None)
                .context("failed to store the outgoing account")?;
            Some(entry.label)
        }
        _ => None,
    };

    snapshot
        .vault
        .write_active_slot(
            &target,
            config.cli_auth_credentials_store_mode,
            config.auth_keyring_backend_kind(),
        )
        .context("failed to write the active credentials")?;
    snapshot
        .vault
        .touch_last_used(&target.session_id)
        .context("failed to update account metadata")?;

    println!(
        "Switched to \"{}\"{}.",
        target.label,
        target
            .email()
            .map(|email| format!(" ({email})"))
            .unwrap_or_default()
    );
    if let Some(outgoing_label) = outgoing_label {
        println!("The previous account was kept as \"{outgoing_label}\".");
    }
    println!(
        "Running Codex processes (terminal sessions, the desktop app, IDE extensions) keep the previous account until restarted."
    );
    Ok(())
}

async fn run_remove(config: &Config, args: RemoveArgs) -> Result<()> {
    let snapshot = load_snapshot(config)?;
    let entry = snapshot.vault.resolve(&args.name)?;

    let is_active = entry.identity().as_deref() == snapshot.active_identity.as_deref()
        && snapshot.active_identity.is_some();
    if is_active && !args.force {
        bail!(
            "\"{}\" matches the currently active account. Switch to another account first, or pass --force to remove only the stored entry (auth.json is kept).",
            entry.label
        );
    }

    if args.revoke
        && let Err(err) = snapshot
            .vault
            .revoke_entry_tokens(&entry, &config.auth_route_config())
            .await
    {
        eprintln!("Warning: failed to revoke tokens server-side: {err}");
    }

    snapshot
        .vault
        .remove(&entry.session_id)
        .context("failed to remove the stored account")?;
    println!("Removed stored account \"{}\".", entry.label);
    Ok(())
}
