//! FORK: `codex chatgpt-web …` — the shared connector daemon for the
//! `chatgpt_web` provider (`[chatgpt_web] tools = "connector"`).

use anyhow::Context;
use anyhow::Result;
use clap::Parser;
use clap::Subcommand;
use codex_core::chatgpt_web_daemon;
use codex_core::config::Config;
use codex_utils_cli::CliConfigOverrides;
use std::path::Path;
use std::path::PathBuf;
use std::time::Duration;

#[derive(Debug, Parser)]
pub struct ChatgptWebCli {
    #[clap(skip)]
    pub config_overrides: CliConfigOverrides,

    #[command(subcommand)]
    subcommand: ChatgptWebSubcommand,
}

#[derive(Debug, Subcommand)]
enum ChatgptWebSubcommand {
    /// Run the shared daemon (tunnel + public MCP server + turn broker).
    Daemon(DaemonArgs),
    /// Show whether the daemon is running, its tunnel and connector state.
    Status,
    /// Stop the running daemon.
    Stop,
    /// Check the pieces the connector mode depends on.
    Doctor,
    /// One-time setup of the OpenAI Secure MCP Tunnel credentials.
    Setup(SetupArgs),
    /// Manage the ChatGPT-side connector record.
    Registry(RegistryArgs),
}

#[derive(Debug, Parser)]
struct DaemonArgs {
    /// Stay attached to the terminal and log to stderr.
    #[arg(long)]
    foreground: bool,
    /// Exit after this long with no Codex sessions attached (0 = never).
    #[arg(long)]
    idle_shutdown_ms: Option<u64>,
}

#[derive(Debug, Parser)]
struct SetupArgs {
    /// Tunnel id from platform.openai.com → Settings → Organization → Tunnels.
    #[arg(long)]
    tunnel_id: String,
    /// File containing the restricted API key (Tunnels: Read + Use). Use `-` for stdin.
    #[arg(long)]
    api_key_file: PathBuf,
    /// Do not start the daemon after writing the credentials.
    #[arg(long)]
    no_start: bool,
}

#[derive(Debug, Parser)]
struct RegistryArgs {
    #[command(subcommand)]
    action: RegistryAction,
}

#[derive(Debug, Subcommand)]
enum RegistryAction {
    /// Create or repair the connector on the ChatGPT side.
    Reconcile,
    /// Print the recorded connector.
    Show,
    /// Delete the recorded connector on the ChatGPT side.
    Delete,
}

async fn load_config(config_overrides: &CliConfigOverrides) -> Result<Config> {
    let overrides = config_overrides
        .parse_overrides()
        .map_err(|err| anyhow::anyhow!("error parsing -c overrides: {err}"))?;
    Config::load_with_cli_overrides(overrides)
        .await
        .context("error loading configuration")
}

impl ChatgptWebCli {
    pub async fn run(self) -> Result<()> {
        let config = load_config(&self.config_overrides).await?;
        let codex_home = config.codex_home.to_path_buf();
        match self.subcommand {
            ChatgptWebSubcommand::Daemon(args) => run_daemon(&config, args).await,
            ChatgptWebSubcommand::Status => run_status(&codex_home).await,
            ChatgptWebSubcommand::Stop => {
                if chatgpt_web_daemon::stop(&codex_home).await? {
                    println!("chatgpt-web daemon stopped");
                } else {
                    println!("chatgpt-web daemon is not running");
                }
                Ok(())
            }
            ChatgptWebSubcommand::Doctor => run_doctor(&config).await,
            ChatgptWebSubcommand::Setup(args) => run_setup(&config, args).await,
            ChatgptWebSubcommand::Registry(args) => match args.action {
                RegistryAction::Reconcile => {
                    let body =
                        chatgpt_web_daemon::reconcile_via_daemon(&codex_home, &config.chatgpt_web)
                            .await?;
                    println!("{}", serde_json::to_string_pretty(&body)?);
                    Ok(())
                }
                RegistryAction::Show => run_registry_show(&codex_home).await,
                RegistryAction::Delete => run_registry_delete(&config).await,
            },
        }
    }
}

/// Rotates `daemon.log` when it grows past 5 MB, then returns a writer for it.
fn open_daemon_log(path: &Path) -> Result<std::fs::File> {
    const MAX_LOG_BYTES: u64 = 5 * 1024 * 1024;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    if std::fs::metadata(path)
        .map(|meta| meta.len() > MAX_LOG_BYTES)
        .unwrap_or(false)
    {
        let rotated = path.with_extension("log.1");
        let _ = std::fs::remove_file(&rotated);
        let _ = std::fs::rename(path, &rotated);
    }
    std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .with_context(|| format!("opening {}", path.display()))
}

/// FORK: the daemon owns its log level.
///
/// It used to read `RUST_LOG`, which is the Desktop's variable, not the user's:
/// the app launches the app-server with `RUST_LOG=warn`, the daemon inherits it
/// through the spawn, and `daemon.log` ends up with warnings only — which is
/// why the "daemon did not come up within 15s" failures left no trace at all.
/// `CODEX_CHATGPT_WEB_LOG` (the `CODEX_CHATGPT_WEB_TUNNEL_KEY` family) is the
/// knob; the default is `info` regardless of `RUST_LOG`.
fn daemon_log_filter(requested: Option<&str>) -> tracing_subscriber::EnvFilter {
    const DEFAULT: &str = "info";
    match requested.map(str::trim).filter(|value| !value.is_empty()) {
        Some(value) => match tracing_subscriber::EnvFilter::try_new(value) {
            Ok(filter) => filter,
            Err(error) => {
                eprintln!(
                    "chatgpt-web daemon: ignoring CODEX_CHATGPT_WEB_LOG={value:?} ({error}); using {DEFAULT}"
                );
                tracing_subscriber::EnvFilter::new(DEFAULT)
            }
        },
        None => tracing_subscriber::EnvFilter::new(DEFAULT),
    }
}

async fn run_daemon(config: &Config, args: DaemonArgs) -> Result<()> {
    let codex_home = config.codex_home.to_path_buf();
    let paths = chatgpt_web_daemon::state::DaemonPaths::new(&codex_home);
    let filter = daemon_log_filter(std::env::var("CODEX_CHATGPT_WEB_LOG").ok().as_deref());
    if args.foreground {
        let _ = tracing_subscriber::fmt()
            .with_env_filter(filter)
            .with_writer(std::io::stderr)
            .try_init();
    } else {
        let log = open_daemon_log(&paths.log)?;
        let _ = tracing_subscriber::fmt()
            .with_env_filter(filter)
            .with_ansi(false)
            .with_writer(log)
            .try_init();
    }
    let idle_shutdown = args
        .idle_shutdown_ms
        .or(Some(config.chatgpt_web.daemon_idle_shutdown_ms))
        .filter(|ms| *ms > 0)
        .map(Duration::from_millis);
    let mut run_config =
        chatgpt_web_daemon::DaemonRunConfig::new(config.chatgpt_web.clone(), codex_home)
            .with_live_registry();
    run_config.foreground = args.foreground;
    run_config.idle_shutdown = idle_shutdown;
    // FORK: a background daemon has a null stderr, so an error returned from
    // here used to vanish. Record it where the log can be read.
    match chatgpt_web_daemon::run(run_config).await {
        Ok(()) => Ok(()),
        Err(error) => {
            tracing::error!("chatgpt-web daemon exited with an error: {error:#}");
            Err(error)
        }
    }
}

async fn run_status(codex_home: &Path) -> Result<()> {
    let status = chatgpt_web_daemon::status(codex_home).await;
    println!("{}", serde_json::to_string_pretty(&status)?);
    Ok(())
}

async fn run_registry_show(codex_home: &Path) -> Result<()> {
    let paths = chatgpt_web_daemon::state::DaemonPaths::new(codex_home);
    match chatgpt_web_daemon::state::read_json_opt::<chatgpt_web_daemon::state::ConnectorRecord>(
        &paths.connector,
    ) {
        Some(record) => println!("{}", serde_json::to_string_pretty(&record)?),
        None => println!(
            "no connector recorded at {} (run `codex chatgpt-web registry reconcile`)",
            paths.connector.display()
        ),
    }
    // FORK (C2): the live status comes from the running daemon, if any.
    let status = chatgpt_web_daemon::status(codex_home).await;
    match status.health {
        Some(health) => println!(
            "daemon pid {}: registry {}, tunnel {}",
            health.pid, health.registry_status, health.tunnel_state
        ),
        None => println!("daemon not running"),
    }
    Ok(())
}

/// Deletes the recorded connector (and any other connector carrying the
/// configured name) on the ChatGPT side, directly through chrome-mcp — no
/// daemon needed.
async fn run_registry_delete(config: &Config) -> Result<()> {
    let settings = &config.chatgpt_web;
    let paths = chatgpt_web_daemon::state::DaemonPaths::new(&config.codex_home);
    let api = chatgpt_web_daemon::registry_api::ChromeMcpPageApi::from_settings(settings);
    let deleted = chatgpt_web_daemon::registry::delete_recorded(
        &api,
        &settings.connector_name,
        &paths.connector,
    )
    .await
    .map_err(|error| anyhow::anyhow!("{error}"))?;
    if deleted.is_empty() {
        println!(
            "no connector named `{}` found on the ChatGPT side; {} removed",
            settings.connector_name,
            paths.connector.display()
        );
    } else {
        for entry in deleted {
            println!("deleted {entry}");
        }
    }
    println!("the running daemon (if any) will recreate the connector on its next reconcile");
    Ok(())
}

async fn run_setup(config: &Config, args: SetupArgs) -> Result<()> {
    let codex_home = config.codex_home.as_path();
    // The credentials just written must win over whatever the session passed.
    let mut settings = config.chatgpt_web.clone();
    settings.tunnel = codex_config::config_toml::ChatGptWebTunnel::Openai;
    settings.tunnel_id = Some(args.tunnel_id.clone());
    let api_key = if args.api_key_file == Path::new("-") {
        let mut buffer = String::new();
        std::io::Read::read_to_string(&mut std::io::stdin(), &mut buffer)
            .context("reading the API key from stdin")?;
        buffer
    } else {
        std::fs::read_to_string(&args.api_key_file)
            .with_context(|| format!("reading {}", args.api_key_file.display()))?
    };
    let key_path = chatgpt_web_daemon::setup_tunnel(codex_home, &args.tunnel_id, &api_key)?;
    println!(
        "stored the tunnel key at {} and set [chatgpt_web] tunnel_id / tunnel = \"openai\"",
        key_path.display()
    );
    if args.no_start {
        return Ok(());
    }
    // A daemon started under the old settings must pick up the new ones.
    if chatgpt_web_daemon::stop(codex_home).await? {
        println!("restarted the running daemon");
    }
    println!("starting the daemon and waiting for the tunnel…");
    let health =
        chatgpt_web_daemon::wait_tunnel_ready(codex_home, &settings, Duration::from_secs(150))
            .await?;
    println!(
        "tunnel ready ({}); registry: {}",
        health.public_url.unwrap_or_default(),
        health.registry_status
    );
    Ok(())
}

async fn run_doctor(config: &Config) -> Result<()> {
    let settings = &config.chatgpt_web;
    let codex_home = config.codex_home.to_path_buf();
    let paths = chatgpt_web_daemon::state::DaemonPaths::new(&codex_home);
    let mut problems = 0usize;
    let mut report = |ok: bool, line: String| {
        println!("{} {line}", if ok { "ok  " } else { "FAIL" });
        if !ok {
            problems += 1;
        }
    };

    match chatgpt_web_daemon::probe_chrome_mcp(&settings.daemon_url).await {
        Ok(body) => {
            let connected = body["extensionConnected"].as_bool().unwrap_or(false);
            report(
                connected,
                format!(
                    "chrome-mcp daemon at {} (extension connected: {connected})",
                    settings.daemon_url
                ),
            );
        }
        Err(error) => report(false, format!("chrome-mcp daemon: {error:#}")),
    }

    match settings.tunnel {
        codex_config::config_toml::ChatGptWebTunnel::Openai => {
            let has_id = settings
                .tunnel_id
                .as_deref()
                .is_some_and(chatgpt_web_daemon::tunnel::is_valid_tunnel_id);
            report(
                has_id,
                match &settings.tunnel_id {
                    Some(id) => format!("tunnel_id {id}"),
                    None => "tunnel_id missing — run `codex chatgpt-web setup`".to_string(),
                },
            );
            let key_path = settings
                .tunnel_key_file
                .clone()
                .unwrap_or_else(|| paths.tunnel_key.clone());
            let has_key = std::env::var("CODEX_CHATGPT_WEB_TUNNEL_KEY").is_ok()
                || chatgpt_web_daemon::state::read_secret(&key_path).is_some();
            report(has_key, format!("tunnel API key at {}", key_path.display()));
            let binary = chatgpt_web_daemon::tunnel::resolve_tunnel_client(
                settings.tunnel_client_path.as_deref(),
                &paths.bin_dir,
                &settings.tunnel_client_version,
            );
            report(
                binary.is_some(),
                match &binary {
                    Some(path) => format!("tunnel-client at {}", path.display()),
                    None => format!(
                        "tunnel-client v{} not installed (the daemon downloads it on first start)",
                        settings.tunnel_client_version
                    ),
                },
            );
        }
        codex_config::config_toml::ChatGptWebTunnel::Cloudflared => {
            let binary = chatgpt_web_daemon::tunnel::resolve_cloudflared(
                settings.cloudflared_path.as_deref(),
            );
            report(
                binary.is_some(),
                match &binary {
                    Some(path) => format!("cloudflared at {}", path.display()),
                    None => "cloudflared not found".to_string(),
                },
            );
        }
        codex_config::config_toml::ChatGptWebTunnel::Manual => {
            report(
                settings.manual_mcp_url.is_some(),
                format!(
                    "manual tunnel URL {}",
                    settings.manual_mcp_url.as_deref().unwrap_or("(missing)")
                ),
            );
        }
    }

    let status = chatgpt_web_daemon::status(&codex_home).await;
    match (&status.alive, &status.health) {
        (true, Some(health)) => report(
            true,
            format!(
                "daemon pid {} (tunnel: {}, registry: {}, sessions: {}, turns: {})",
                health.pid,
                health.tunnel_state,
                health.registry_status,
                health.sessions,
                health.active_turns
            ),
        ),
        _ => println!("info daemon not running (it starts on the first connector turn)"),
    }

    if problems == 0 {
        println!("all checks passed");
        Ok(())
    } else {
        anyhow::bail!("{problems} check(s) failed")
    }
}

#[cfg(test)]
mod tests {
    use super::daemon_log_filter;

    /// FORK: the Desktop launches the app-server with `RUST_LOG=warn` and the
    /// daemon inherits it, which is how `daemon.log` ended up with warnings
    /// only. The daemon reads its own variable and defaults to `info`.
    #[test]
    fn the_daemon_log_filter_defaults_to_info_and_accepts_its_own_variable() {
        // SAFETY: single-threaded test; nothing else reads RUST_LOG here.
        unsafe { std::env::set_var("RUST_LOG", "warn") };

        assert_eq!(daemon_log_filter(None).to_string(), "info");
        assert_eq!(daemon_log_filter(Some("   ")).to_string(), "info");
        assert_eq!(daemon_log_filter(Some("debug")).to_string(), "debug");
        // An unusable value falls back rather than silencing the daemon.
        assert_eq!(daemon_log_filter(Some("=;=not a filter")).to_string(), "info");

        // SAFETY: as above.
        unsafe { std::env::remove_var("RUST_LOG") };
    }
}
