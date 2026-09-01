//! Recovery of interrupted rollout compression publications and stale temps.

use std::ffi::OsStr;
use std::io;
use std::path::Path;
use std::time::Duration;
use std::time::SystemTime;

use tracing::warn;

use super::compression_journal;
use super::metrics;

const TEMP_SUFFIX: &str = ".tmp";
const TEMP_FILE_STALE_AFTER: Duration = Duration::from_secs(6 * 60 * 60);

pub(super) async fn cleanup_stale_temps(codex_home: &Path) -> io::Result<()> {
    for root in [
        codex_home.join(crate::SESSIONS_SUBDIR),
        codex_home.join(crate::ARCHIVED_SESSIONS_SUBDIR),
    ] {
        cleanup_stale_temps_in_root(root.as_path()).await?;
    }
    Ok(())
}

async fn cleanup_stale_temps_in_root(root: &Path) -> io::Result<()> {
    if !tokio::fs::try_exists(root).await.unwrap_or(false) {
        return Ok(());
    }
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let mut read_dir = match tokio::fs::read_dir(dir.as_path()).await {
            Ok(read_dir) => read_dir,
            Err(error) => {
                warn!(
                    "failed to read rollout temp cleanup directory {}: {error}",
                    dir.display()
                );
                continue;
            }
        };
        while let Some(entry) = read_dir.next_entry().await? {
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
            if compression_journal::is_journal_path(path.as_path()) {
                match compression_journal::recover(path.as_path()) {
                    Ok(compression_journal::RecoveryOutcome::PlainWins) => {
                        metrics::temp_cleanup("journal_plain_wins");
                    }
                    Ok(compression_journal::RecoveryOutcome::CompressedWins) => {
                        metrics::temp_cleanup("journal_compressed_wins");
                    }
                    Err(error) => {
                        metrics::temp_cleanup("journal_recovery_failed");
                        warn!(
                            "failed to recover rollout compression journal {}: {error}",
                            path.display()
                        );
                    }
                }
                continue;
            }
            let Some(name) = path.file_name().and_then(OsStr::to_str) else {
                continue;
            };
            if !name.ends_with(TEMP_SUFFIX) {
                continue;
            }
            let stale = entry
                .metadata()
                .await
                .ok()
                .and_then(|metadata| metadata.modified().ok())
                .and_then(|modified| SystemTime::now().duration_since(modified).ok())
                .is_some_and(|age| age >= TEMP_FILE_STALE_AFTER);
            if !stale {
                continue;
            }
            match tokio::fs::remove_file(path.as_path()).await {
                Ok(()) => metrics::temp_cleanup("removed"),
                Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                Err(error) => {
                    metrics::temp_cleanup("failed");
                    warn!(
                        "failed to remove stale rollout temp {}: {error}",
                        path.display()
                    );
                }
            }
        }
    }
    Ok(())
}
