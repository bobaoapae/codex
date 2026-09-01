use std::io;
use std::io::IsTerminal;
use std::io::Write;
use std::path::Path;
use std::time::Duration;
use std::time::Instant;

use anyhow::Context;
use clap::Parser;
use codex_core::config::ConfigBuilder;
use codex_protocol::ThreadId;
use codex_thread_store::LocalThreadStore;
use codex_thread_store::LocalThreadStoreConfig;
use codex_thread_store::RolloutMigrationMode;
use codex_thread_store::RolloutMigrationOptions;
use codex_thread_store::RolloutMigrationPreviewOptions;
use codex_thread_store::RolloutMigrationProgress;
use codex_thread_store::RolloutMigrationStatus;
use codex_utils_cli::CliConfigOverrides;

#[path = "migrate_rollouts_receipt.rs"]
mod receipt;
#[path = "migrate_rollouts_report.rs"]
mod report;

use receipt::persist_apply_receipt;
use report::CliMigrationReport;

#[derive(Debug, Parser)]
pub(crate) struct MigrateRolloutsCommand {
    /// Publish the migration. Without this flag the command only reports eligible sessions.
    #[arg(long, requires = "preview_report")]
    apply: bool,

    /// Read the exact JSON preview report to apply, or save a preview report at this path.
    #[arg(long, value_name = "PATH")]
    preview_report: Option<std::path::PathBuf>,

    /// Restrict inspection or migration to one or more thread IDs.
    #[arg(long, value_name = "THREAD_ID", value_parser = ThreadId::from_string)]
    thread: Vec<ThreadId>,

    /// Limit aggregate rollout read and write throughput, in MiB per second.
    #[arg(
        long,
        value_name = "MIB",
        value_parser = clap::value_parser!(u64).range(1..)
    )]
    max_mib_per_second: Option<u64>,

    /// Emit the complete per-thread report as JSON.
    #[arg(long)]
    json: bool,

    /// Print one line for every inspected rollout.
    #[arg(long)]
    verbose: bool,
}

pub(crate) async fn run(
    command: MigrateRolloutsCommand,
    config_overrides: CliConfigOverrides,
) -> anyhow::Result<()> {
    let frozen_preview = if command.apply {
        let path = command.preview_report.as_deref().ok_or_else(|| {
            anyhow::anyhow!("--apply requires --preview-report <path> from a prior JSON preview")
        })?;
        Some(report::load_frozen_preview(path).await?)
    } else {
        None
    };
    let overrides = config_overrides
        .parse_overrides()
        .map_err(anyhow::Error::msg)?;
    let config = ConfigBuilder::default()
        .cli_overrides(overrides)
        .build()
        .await?;
    let json = command.json;
    let verbose = command.verbose;
    let operation_started_at = Instant::now();
    if !command.apply {
        let preview_options = RolloutMigrationPreviewOptions {
            thread_ids: command.thread.clone(),
            max_mib_per_second: command.max_mib_per_second,
        };
        // Preview intentionally uses a store without a state DB. StateRuntime::init performs
        // legacy metadata backfill, which would violate the default command's read-only contract.
        let preview_store = LocalThreadStore::new(
            LocalThreadStoreConfig::from_config(&config),
            /*state_db*/ None,
        );
        let preview = preview_store
            .preview_rollout_migration(preview_options)
            .await
            .context("failed to preview local rollouts")?;
        let report = CliMigrationReport::for_preview(
            preview,
            command.max_mib_per_second,
            operation_started_at.elapsed(),
        );
        if let Some(path) = command.preview_report.as_deref() {
            report::save_json_report(&report, path).await?;
        }
        report::write_report(&report, json, verbose, /*thread_storage*/ None)?;
        return Ok(());
    }

    let Some(preview) = frozen_preview else {
        return Err(anyhow::anyhow!(
            "--apply requires --preview-report <path> from a prior JSON preview"
        ));
    };

    let otel = codex_core::otel_init::build_provider(
        &config,
        env!("CARGO_PKG_VERSION"),
        /*service_name_override*/ None,
        /*default_analytics_enabled*/ true,
    )
    .unwrap_or_else(|error| {
        eprintln!("Could not create otel exporter: {error}");
        None
    });
    codex_core::otel_init::record_process_start(otel.as_ref(), "codex_migrate_rollouts");
    let thread_history_db_path = config.sqlite.thread_history_db_path();
    let thread_storage_before = if !json {
        thread_storage_bytes(
            config.codex_home.as_path(),
            thread_history_db_path.as_path(),
        )
        .await
        .ok()
    } else {
        None
    };
    let state_db = Some(
        codex_rollout::state_db::try_init(&config)
            .await
            .context("failed to initialize local thread metadata")?,
    );
    let store = LocalThreadStore::new(LocalThreadStoreConfig::from_config(&config), state_db);
    let mut progress = MigrationProgress::new(RolloutMigrationMode::Apply, json);
    progress.begin();
    let result = store
        .migrate_rollouts_from_preview_with_progress(
            preview.clone(),
            RolloutMigrationOptions {
                mode: RolloutMigrationMode::Apply,
                thread_ids: command.thread,
                max_mib_per_second: command.max_mib_per_second,
            },
            |update| progress.update(update),
        )
        .await;
    progress.finish();
    let report = result?;
    let thread_storage = match thread_storage_before {
        Some(before) => thread_storage_bytes(
            config.codex_home.as_path(),
            thread_history_db_path.as_path(),
        )
        .await
        .ok()
        .map(|after| (before, after)),
        None => None,
    };

    if let Some(apply_receipt) = report.apply_receipt.as_ref() {
        let receipt = report::effective_apply_receipt(&preview, apply_receipt);
        persist_apply_receipt(&config, &receipt)
            .await
            .context("failed to persist rollout migration receipt")?;
    }

    let cli_report = CliMigrationReport::for_apply(
        preview,
        report,
        command.max_mib_per_second,
        operation_started_at.elapsed(),
    );
    report::write_report(&cli_report, json, verbose, thread_storage)?;

    if cli_report
        .outcomes
        .as_ref()
        .into_iter()
        .flatten()
        .any(|outcome| outcome.status == RolloutMigrationStatus::Failed)
    {
        anyhow::bail!("one or more rollout migrations failed");
    }
    Ok(())
}

const TTY_PROGRESS_INTERVAL: Duration = Duration::from_millis(250);
const NON_TTY_PROGRESS_INTERVAL: usize = 1_000;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ProgressOutput {
    Quiet,
    Tty,
    Plain,
}

struct MigrationProgress {
    mode: RolloutMigrationMode,
    output: ProgressOutput,
    started_at: Instant,
    last_rendered_at: Instant,
    last_plain_processed: usize,
    counts: MigrationCounts,
    wrote_tty_line: bool,
}

impl MigrationProgress {
    fn new(mode: RolloutMigrationMode, json: bool) -> Self {
        let now = Instant::now();
        let output = if json {
            ProgressOutput::Quiet
        } else if io::stderr().is_terminal()
            && std::env::var("TERM").ok().as_deref() != Some("dumb")
        {
            ProgressOutput::Tty
        } else {
            ProgressOutput::Plain
        };
        Self {
            mode,
            output,
            started_at: now,
            last_rendered_at: now,
            last_plain_processed: 0,
            counts: MigrationCounts::default(),
            wrote_tty_line: false,
        }
    }

    fn begin(&self) {
        if self.output != ProgressOutput::Quiet {
            eprintln!("Scanning local rollouts...");
        }
    }

    fn update(&mut self, update: RolloutMigrationProgress) {
        if let Some(status) = update.outcome_status {
            self.counts.observe(status);
        }
        match self.output {
            ProgressOutput::Quiet => {}
            ProgressOutput::Tty
                if update.processed_paths == update.total_paths
                    || self.last_rendered_at.elapsed() >= TTY_PROGRESS_INTERVAL =>
            {
                let line = self.line(update);
                let mut stderr = io::stderr().lock();
                let _ = write!(stderr, "\r\x1b[2K{line}");
                let _ = stderr.flush();
                self.last_rendered_at = Instant::now();
                self.wrote_tty_line = true;
            }
            ProgressOutput::Plain
                if update.processed_paths == update.total_paths
                    || update
                        .processed_paths
                        .saturating_sub(self.last_plain_processed)
                        >= NON_TTY_PROGRESS_INTERVAL =>
            {
                eprintln!("{}", self.line(update));
                self.last_plain_processed = update.processed_paths;
            }
            ProgressOutput::Tty | ProgressOutput::Plain => {}
        }
    }

    fn finish(&mut self) {
        if self.output != ProgressOutput::Tty || !self.wrote_tty_line {
            return;
        }
        let mut stderr = io::stderr().lock();
        let _ = write!(stderr, "\r\x1b[2K");
        let _ = stderr.flush();
        self.wrote_tty_line = false;
    }

    fn elapsed(&self) -> Duration {
        self.started_at.elapsed()
    }

    fn line(&self, update: RolloutMigrationProgress) -> String {
        let percent = update
            .processed_paths
            .saturating_mul(100)
            .checked_div(update.total_paths)
            .unwrap_or(100);
        let action = match self.mode {
            RolloutMigrationMode::DryRun => "Checking",
            RolloutMigrationMode::Apply => "Migrating",
        };
        let status_counts = match self.mode {
            RolloutMigrationMode::DryRun => format!(
                "{} eligible  •  {} already paginated",
                self.counts.eligible, self.counts.already_paginated
            ),
            RolloutMigrationMode::Apply => format!(
                "{} migrated  •  {} already paginated",
                self.counts.migrated, self.counts.already_paginated
            ),
        };
        format!(
            "{action} rollouts  {}/{} ({percent}%)  •  {status_counts}  •  {} skipped  •  {} failed  •  {}",
            update.processed_paths,
            update.total_paths,
            self.counts.skipped(),
            self.counts.failed,
            format_elapsed(self.elapsed()),
        )
    }
}

#[derive(Default)]
struct MigrationCounts {
    eligible: usize,
    migrated: usize,
    already_paginated: usize,
    skipped_empty: usize,
    skipped_busy: usize,
    failed: usize,
}

impl MigrationCounts {
    fn observe(&mut self, status: RolloutMigrationStatus) {
        match status {
            RolloutMigrationStatus::Eligible => self.eligible += 1,
            RolloutMigrationStatus::Migrated => self.migrated += 1,
            RolloutMigrationStatus::AlreadyPaginated => self.already_paginated += 1,
            RolloutMigrationStatus::SkippedEmpty => self.skipped_empty += 1,
            RolloutMigrationStatus::SkippedBusy => self.skipped_busy += 1,
            RolloutMigrationStatus::Failed => self.failed += 1,
        }
    }

    fn skipped(&self) -> usize {
        self.skipped_empty + self.skipped_busy
    }
}

fn format_elapsed(elapsed: Duration) -> String {
    let seconds = elapsed.as_secs();
    let minutes = seconds / 60;
    if minutes == 0 {
        return format!("{seconds}s");
    }
    let hours = minutes / 60;
    if hours == 0 {
        return format!("{minutes}m{:02}s", seconds % 60);
    }
    format!("{hours}h{:02}m{:02}s", minutes % 60, seconds % 60)
}

async fn thread_storage_bytes(codex_home: &Path, thread_history_db_path: &Path) -> io::Result<u64> {
    let mut bytes = 0_u64;
    let mut directories = vec![
        codex_home.join(codex_rollout::SESSIONS_SUBDIR),
        codex_home.join(codex_rollout::ARCHIVED_SESSIONS_SUBDIR),
    ];
    while let Some(directory) = directories.pop() {
        let mut entries = match tokio::fs::read_dir(&directory).await {
            Ok(entries) => entries,
            Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
            Err(error) => return Err(error),
        };
        while let Some(entry) = entries.next_entry().await? {
            let file_type = entry.file_type().await?;
            if file_type.is_dir() {
                directories.push(entry.path());
            } else if file_type.is_file() {
                bytes = bytes.saturating_add(entry.metadata().await?.len());
            }
        }
    }

    for suffix in ["", "-wal", "-shm"] {
        let mut path = thread_history_db_path.as_os_str().to_owned();
        path.push(suffix);
        match tokio::fs::metadata(path).await {
            Ok(metadata) => bytes = bytes.saturating_add(metadata.len()),
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
    }
    Ok(bytes)
}

fn format_bytes(bytes: u64) -> String {
    const KIB: f64 = 1024.0;
    const MIB: f64 = KIB * 1024.0;
    const GIB: f64 = MIB * 1024.0;

    let bytes = bytes as f64;
    if bytes >= GIB {
        format!("{:.1} GB", bytes / GIB)
    } else if bytes >= MIB {
        format!("{:.1} MB", bytes / MIB)
    } else if bytes >= KIB {
        format!("{:.1} KB", bytes / KIB)
    } else {
        format!("{} B", bytes as u64)
    }
}

#[cfg(test)]
#[path = "migrate_rollouts_tests.rs"]
mod tests;
