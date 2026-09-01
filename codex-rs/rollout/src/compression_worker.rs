//! Background discovery, throttling, and metrics for cold rollout compression.

use std::io;
use std::io::Write;
use std::path::Path;
use std::path::PathBuf;
use std::time::Duration;
use std::time::Instant;
use std::time::SystemTime;

use tokio::task::JoinSet;
use tracing::debug;
use tracing::info;
use tracing::warn;

use super::RolloutCompressionMode;
use super::RolloutFile;
use super::compression_capabilities::RolloutCompressionCapabilities;
use super::compression_cleanup;
use super::compression_writer::CompressionMeasurement;
use super::compression_writer::CompressionOutcome;
use super::compression_writer::compress_rollout_if_cold;
use super::metrics;
use crate::RolloutReferenceIndex;

const RUN_MARKER_STALE_AFTER: Duration = Duration::from_secs(6 * 60 * 60);
const WORKER_MAX_RUNTIME: Duration = Duration::from_secs(5 * 60 * 60);
const RUN_MARKER_FILE_NAME: &str = "rollout-compression.lock";
const MAX_CONCURRENT_COMPRESSION_JOBS: usize = 2;

#[derive(Default)]
struct CompressionStats {
    scanned: usize,
    compressed: usize,
    skipped: usize,
    failed: usize,
}

pub(super) struct CompressionRunMarker {
    path: PathBuf,
    remove_on_drop: bool,
}

impl CompressionRunMarker {
    pub(super) fn try_claim(codex_home: &Path) -> io::Result<Option<Self>> {
        let marker_dir = codex_home.join(".tmp");
        std::fs::create_dir_all(marker_dir.as_path())?;
        let path = marker_dir.join(RUN_MARKER_FILE_NAME);
        match create_run_marker_file(path.as_path()) {
            Ok(()) => return Ok(Some(Self::new(path))),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(error),
        }
        let stale = std::fs::metadata(path.as_path())
            .and_then(|metadata| metadata.modified())
            .ok()
            .and_then(|modified| SystemTime::now().duration_since(modified).ok())
            .is_some_and(|age| age >= RUN_MARKER_STALE_AFTER);
        if !stale {
            return Ok(None);
        }
        match std::fs::remove_file(path.as_path()) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
        match create_run_marker_file(path.as_path()) {
            Ok(()) => Ok(Some(Self::new(path))),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => Ok(None),
            Err(error) => Err(error),
        }
    }

    fn new(path: PathBuf) -> Self {
        Self {
            path,
            remove_on_drop: true,
        }
    }

    pub(super) fn persist(mut self) {
        self.remove_on_drop = false;
    }
}

impl Drop for CompressionRunMarker {
    fn drop(&mut self) {
        if self.remove_on_drop {
            let _ = std::fs::remove_file(self.path.as_path());
        }
    }
}

pub(super) fn spawn(
    codex_home: PathBuf,
    mode: RolloutCompressionMode,
    capabilities: RolloutCompressionCapabilities,
) {
    let Ok(handle) = tokio::runtime::Handle::try_current() else {
        metrics::run("skipped_no_runtime");
        warn!(
            "failed to start rollout compression worker for {}: no Tokio runtime",
            codex_home.display()
        );
        return;
    };
    handle.spawn(async move {
        if let Err(error) = run_with_capabilities(codex_home.clone(), mode, capabilities).await {
            warn!(
                "rollout compression worker failed for {}: {error}",
                codex_home.display()
            );
        }
    });
}

#[cfg(test)]
pub(super) async fn run(codex_home: PathBuf, mode: RolloutCompressionMode) -> io::Result<()> {
    run_with_capabilities(codex_home, mode, RolloutCompressionCapabilities::default()).await
}

pub(super) async fn run_with_capabilities(
    codex_home: PathBuf,
    mode: RolloutCompressionMode,
    capabilities: RolloutCompressionCapabilities,
) -> io::Result<()> {
    let capability_blocked =
        mode == RolloutCompressionMode::IncludeShared && !capabilities.all_readers_support_shared();
    let Some(_maintenance_guard) =
        crate::try_acquire_rollout_maintenance_lock(codex_home.as_path())?
    else {
        metrics::run("skipped_maintenance");
        debug!(
            "rollout maintenance is already running for {}",
            codex_home.display()
        );
        return Ok(());
    };
    let marker = match CompressionRunMarker::try_claim(codex_home.as_path()) {
        Ok(Some(marker)) => marker,
        Ok(None) => {
            metrics::run("skipped_already_running");
            debug!(
                "rollout compression worker recently ran or is already running for {}",
                codex_home.display()
            );
            return Ok(());
        }
        Err(error) => {
            metrics::run("failed");
            return Err(error);
        }
    };

    metrics::run("started");
    let started_at = Instant::now();
    let result = async {
        compression_cleanup::cleanup_stale_temps(codex_home.as_path()).await?;
        if capability_blocked {
            metrics::run("skipped_capability_gate");
            debug!("{}", capabilities.shared_compression_diagnostic());
            return Ok(CompressionStats::default());
        }
        let Some(reference_index) =
            RolloutReferenceIndex::scan_until(codex_home.as_path(), started_at, WORKER_MAX_RUNTIME)
                .await?
        else {
            return Ok(CompressionStats::default());
        };
        let mut stats = CompressionStats::default();
        for root in [
            codex_home.join(crate::ARCHIVED_SESSIONS_SUBDIR),
            codex_home.join(crate::SESSIONS_SUBDIR),
        ] {
            if started_at.elapsed() >= WORKER_MAX_RUNTIME {
                break;
            }
            compress_rollouts_in_root(
                root.as_path(),
                started_at,
                &reference_index,
                mode,
                &mut stats,
            )
            .await?;
        }
        Ok::<_, io::Error>(stats)
    }
    .await;
    let stats = match result {
        Ok(stats) => stats,
        Err(error) => {
            metrics::run("failed");
            metrics::run_duration("failed", started_at.elapsed());
            return Err(error);
        }
    };
    info!(
        "rollout compression worker finished: scanned={}, compressed={}, skipped={}, failed={}",
        stats.scanned, stats.compressed, stats.skipped, stats.failed
    );
    metrics::run("completed");
    metrics::run_duration("completed", started_at.elapsed());
    if !capability_blocked {
        marker.persist();
    }
    Ok(())
}

fn create_run_marker_file(path: &Path) -> io::Result<()> {
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)?;
    writeln!(
        file,
        "pid={} started_at={:?}",
        std::process::id(),
        SystemTime::now()
    )?;
    Ok(())
}

async fn compress_rollouts_in_root(
    root: &Path,
    started_at: Instant,
    reference_index: &RolloutReferenceIndex,
    mode: RolloutCompressionMode,
    stats: &mut CompressionStats,
) -> io::Result<()> {
    if !tokio::fs::try_exists(root).await.unwrap_or(false) {
        return Ok(());
    }
    let mut stack = vec![root.to_path_buf()];
    let mut jobs = JoinSet::new();
    while let Some(dir) = stack.pop() {
        if started_at.elapsed() >= WORKER_MAX_RUNTIME {
            break;
        }
        let mut read_dir = match tokio::fs::read_dir(dir.as_path()).await {
            Ok(read_dir) => read_dir,
            Err(error) => {
                warn!(
                    "failed to read rollout compression directory {}: {error}",
                    dir.display()
                );
                continue;
            }
        };
        loop {
            let entry = match read_dir.next_entry().await {
                Ok(Some(entry)) => entry,
                Ok(None) => break,
                Err(error) => {
                    drain_compression_jobs(&mut jobs, stats).await;
                    return Err(error);
                }
            };
            if started_at.elapsed() >= WORKER_MAX_RUNTIME {
                break;
            }
            let path = entry.path();
            let file_type = match entry.file_type().await {
                Ok(file_type) => file_type,
                Err(error) => {
                    warn!(
                        "failed to read rollout compression file type {}: {error}",
                        path.display()
                    );
                    continue;
                }
            };
            if file_type.is_dir() {
                stack.push(path);
                continue;
            }
            if !file_type.is_file() {
                continue;
            }
            let Some(rollout_file) = RolloutFile::from_path(path) else {
                continue;
            };
            if rollout_file.is_compressed() {
                continue;
            }
            let path = rollout_file.into_path();
            let Some(rollout_id) = crate::rollout_id_from_path(path.as_path()) else {
                stats.skipped = stats.skipped.saturating_add(1);
                metrics::file("skipped_unreadable_meta");
                continue;
            };
            let Ok(meta) = crate::read_session_meta_line(path.as_path()).await else {
                stats.skipped = stats.skipped.saturating_add(1);
                metrics::file("skipped_unreadable_meta");
                continue;
            };
            if mode == RolloutCompressionMode::Standalone
                && reference_index.reference_count(rollout_id) > 0
            {
                stats.skipped = stats.skipped.saturating_add(1);
                metrics::file("skipped_referenced");
                continue;
            }
            if mode == RolloutCompressionMode::Standalone && meta.meta.history_base.is_some() {
                stats.skipped = stats.skipped.saturating_add(1);
                metrics::file("skipped_fork_pointer");
                continue;
            }
            stats.scanned = stats.scanned.saturating_add(1);
            metrics::file("scanned");
            while jobs.len() >= MAX_CONCURRENT_COMPRESSION_JOBS {
                collect_next_compression_job(&mut jobs, stats).await;
            }
            jobs.spawn_blocking(move || {
                let started_at = Instant::now();
                let result = compress_rollout_if_cold(path.as_path());
                let duration = started_at.elapsed();
                (path, duration, result)
            });
        }
    }
    drain_compression_jobs(&mut jobs, stats).await;
    Ok(())
}

type CompressionJobResult = (PathBuf, Duration, io::Result<CompressionMeasurement>);

async fn drain_compression_jobs(
    jobs: &mut JoinSet<CompressionJobResult>,
    stats: &mut CompressionStats,
) {
    while !jobs.is_empty() {
        collect_next_compression_job(jobs, stats).await;
    }
}

async fn collect_next_compression_job(
    jobs: &mut JoinSet<CompressionJobResult>,
    stats: &mut CompressionStats,
) {
    let Some(result) = jobs.join_next().await else {
        return;
    };
    match result {
        Ok((_, duration, Ok(measurement))) => {
            let outcome = measurement.outcome;
            match outcome {
                CompressionOutcome::Compressed => {
                    stats.compressed = stats.compressed.saturating_add(1);
                }
                CompressionOutcome::SkippedNotCold
                | CompressionOutcome::SkippedChanged
                | CompressionOutcome::SkippedAlreadyCompressed => {
                    stats.skipped = stats.skipped.saturating_add(1);
                }
            }
            metrics::file(outcome.tag());
            metrics::file_duration(outcome.tag(), duration);
            if let Some(source_bytes) = measurement.source_bytes {
                metrics::source_bytes(outcome.tag(), source_bytes);
            }
            if let Some(compressed_bytes) = measurement.compressed_bytes {
                metrics::compressed_bytes(outcome.tag(), compressed_bytes);
                if let Some(source_bytes) = measurement.source_bytes {
                    metrics::compression_ratio(outcome.tag(), source_bytes, compressed_bytes);
                }
            }
        }
        Ok((path, duration, Err(error))) => {
            stats.failed = stats.failed.saturating_add(1);
            metrics::file("failed");
            metrics::file_duration("failed", duration);
            warn!("failed to compress rollout {}: {error}", path.display());
        }
        Err(error) => {
            stats.failed = stats.failed.saturating_add(1);
            metrics::file("failed");
            warn!("rollout compression task failed: {error}");
        }
    }
}
