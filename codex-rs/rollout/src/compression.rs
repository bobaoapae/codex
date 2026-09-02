use std::ffi::OsStr;
use std::fs::File;
use std::fs::Permissions;
use std::io;
use std::io::Read;
use std::io::Write;
use std::path::Path;
use std::path::PathBuf;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;
use std::time::Duration;

#[path = "compression_cleanup.rs"]
mod compression_cleanup;
#[path = "compression_journal.rs"]
mod compression_journal;
#[path = "compression_validation.rs"]
mod compression_validation;
#[path = "compression_writer.rs"]
mod compression_writer;

pub use compression_validation::RolloutValidationSummary;
pub use compression_validation::validate_rollout_replacement;

#[cfg(unix)]
use std::os::unix::fs::OpenOptionsExt;
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;

const COMPRESSED_SUFFIX: &str = ".zst";
const MAX_NOT_FOUND_RETRIES: usize = 3;
const OPEN_ROLLOUT_LINE_READER_RETRY_DELAY: Duration = Duration::from_millis(50);
const TEMP_SUFFIX: &str = ".tmp";
static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

/// Starts a best-effort background job that compresses cold local rollout files.
///
/// The worker is fire-and-forget: failures are logged, startup is not blocked,
/// and a run marker under `codex_home` prevents overlapping or too-frequent
/// compression runs from the same local store.
pub fn spawn_rollout_compression_worker(codex_home: PathBuf) {
    worker::spawn(codex_home)
}

/// Returns the modified time for the existing plain or compressed rollout file.
pub(crate) async fn file_modified_time(path: &Path) -> io::Result<Option<time::OffsetDateTime>> {
    Ok(path::existing_rollout_with_metadata(path)
        .await
        .and_then(|(_, metadata)| metadata.modified().ok())
        .map(time::OffsetDateTime::from))
}

/// Opens a rollout line reader that transparently handles plain `.jsonl` and `.jsonl.zst` files.
///
/// If the requested path disappears during a representation transition, this briefly retries
/// resolution so callers do not need to know which representation is on disk.
pub async fn open_rollout_line_reader(path: &Path) -> io::Result<RolloutLineReader> {
    for _ in 0..MAX_NOT_FOUND_RETRIES {
        match reader::open_once(path).await {
            Ok(reader) => return Ok(reader),
            Err(err) if err.kind() == io::ErrorKind::NotFound => {
                tokio::time::sleep(OPEN_ROLLOUT_LINE_READER_RETRY_DELAY).await;
            }
            Err(err) => return Err(err),
        }
    }
    reader::open_once(path).await
}

/// Returns the compressed `.jsonl.zst` path for a rollout path.
#[cfg(test)]
pub(crate) fn compressed_rollout_path(path: &Path) -> PathBuf {
    path::compressed_rollout_path(path)
}

/// Materializes a compressed rollout back to plain `.jsonl` for async append paths.
pub(crate) async fn materialize_rollout_for_append(path: &Path) -> io::Result<PathBuf> {
    let path = path.to_path_buf();
    tokio::task::spawn_blocking(move || materialize_rollout_for_append_blocking(path.as_path()))
        .await
        .map_err(io::Error::other)?
}

/// Materializes a compressed rollout back to plain `.jsonl` for blocking append paths.
pub(crate) fn materialize_rollout_for_append_blocking(path: &Path) -> io::Result<PathBuf> {
    let plain_path = plain_rollout_path(path);
    if plain_path.exists() {
        metrics::materialize("plain_exists");
        return Ok(plain_path);
    }
    let compressed_path = path::compressed_rollout_path(plain_path.as_path());
    if !compressed_path.exists() {
        metrics::materialize("missing");
        return Ok(plain_path);
    }

    let temp_path = temp_path_for(plain_path.as_path(), "decompress");
    if let Some(parent) = plain_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let result: io::Result<()> = (|| {
        let metadata = std::fs::metadata(compressed_path.as_path())?;
        let permissions = metadata.permissions();
        let mut output = create_file_with_permissions(temp_path.as_path(), &permissions)?;
        {
            let input = File::open(compressed_path.as_path())?;
            let mut decoder = zstd::stream::read::Decoder::new(input)?;
            io::copy(&mut decoder, &mut output)?;
        }
        output.flush()?;
        output.sync_all()?;
        match std::fs::hard_link(temp_path.as_path(), plain_path.as_path()) {
            Ok(()) => {}
            Err(err) if err.kind() == io::ErrorKind::AlreadyExists => {}
            Err(_) => persist_temp_file_noclobber(temp_path.as_path(), plain_path.as_path())?,
        }
        output.set_times(std::fs::FileTimes::new().set_modified(metadata.modified()?))?;
        output.sync_all()?;
        drop(output);
        let _ = std::fs::remove_file(temp_path.as_path());
        match std::fs::remove_file(compressed_path.as_path()) {
            Ok(()) => {}
            Err(err) if err.kind() == io::ErrorKind::NotFound => {}
            Err(err) => return Err(err),
        }
        Ok(())
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(temp_path.as_path());
        metrics::materialize("failed");
    }
    result?;
    metrics::materialize("decompressed");
    Ok(plain_path)
}

fn persist_temp_file_noclobber(temp_path: &Path, destination: &Path) -> io::Result<()> {
    let temp_path = tempfile::TempPath::try_from_path(temp_path)?;
    match temp_path.persist_noclobber(destination) {
        Ok(()) => Ok(()),
        Err(err) if err.error.kind() == io::ErrorKind::AlreadyExists => Ok(()),
        Err(err) => Err(err.error),
    }
}

/// Returns the plain `.jsonl` path for a plain or compressed rollout path.
pub fn plain_rollout_path(path: &Path) -> PathBuf {
    path::plain_rollout_path(path)
}

/// Parses a rollout file name, returning its plain `.jsonl` name when valid.
pub(crate) fn parse_rollout_file_name(name: &str) -> Option<&str> {
    file_name::parse_rollout_file_name(name)
}

/// A discovered rollout file, represented by exactly one physical path.
///
/// This keeps directory walkers from reimplementing the plain/compressed
/// precedence rules. The physical path may point at either `.jsonl` or
/// `.jsonl.zst`, while `plain_file_name` is always the canonical `.jsonl`
/// filename used for timestamp and id parsing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RolloutFile {
    path: PathBuf,
    plain_file_name: String,
}

impl RolloutFile {
    /// Creates a logical rollout file from a physical path found during discovery.
    ///
    /// Returns `None` for non-rollout names and for compressed siblings hidden by
    /// an existing plain `.jsonl` file.
    pub(crate) fn from_path(path: PathBuf) -> Option<Self> {
        let file_name = path.file_name().and_then(|name| name.to_str())?;
        let plain_file_name = file_name::parse_rollout_file_name(file_name)?.to_string();
        if path::should_skip_compressed_sibling(path.as_path()) {
            return None;
        }

        Some(Self {
            path,
            plain_file_name,
        })
    }

    /// Returns the physical path that should be opened for reads.
    pub(crate) fn path(&self) -> &Path {
        self.path.as_path()
    }

    /// Returns the canonical `.jsonl` filename for timestamp and id parsing.
    pub(crate) fn plain_file_name(&self) -> &str {
        self.plain_file_name.as_str()
    }

    /// Returns whether the physical path is the compressed representation.
    pub(crate) fn is_compressed(&self) -> bool {
        path::is_compressed_rollout_path(self.path.as_path())
    }

    /// Consumes the entry and returns the physical path that should be read.
    pub(crate) fn into_path(self) -> PathBuf {
        self.path
    }
}

/// Line-oriented rollout reader returned by [`open_rollout_line_reader`].
pub struct RolloutLineReader {
    inner: RolloutLineReaderInner,
}

enum RolloutLineReaderInner {
    Plain(tokio::io::Lines<tokio::io::BufReader<tokio::fs::File>>),
    Blocking(Option<BlockingLineReader>),
}

impl RolloutLineReader {
    /// Reads the next JSONL record from the rollout.
    pub async fn next_line(&mut self) -> io::Result<Option<String>> {
        match &mut self.inner {
            RolloutLineReaderInner::Plain(lines) => lines.next_line().await,
            RolloutLineReaderInner::Blocking(slot) => {
                let Some(mut reader) = slot.take() else {
                    return Err(io::Error::other("compressed rollout reader is busy"));
                };
                let (line, reader) =
                    tokio::task::spawn_blocking(move || (reader.next().transpose(), reader))
                        .await
                        .map_err(io::Error::other)?;
                *slot = Some(reader);
                line
            }
        }
    }
}

type BlockingLineReader = std::io::Lines<std::io::BufReader<Box<dyn Read + Send>>>;

#[path = "compression_worker.rs"]
mod worker;
mod metrics {
    use std::time::Duration;

    const FILE_COMPRESSED_BYTES_HISTOGRAM: &str = "codex.rollout_compression.file.compressed_bytes";
    const FILE_COUNTER: &str = "codex.rollout_compression.file";
    const FILE_DURATION_HISTOGRAM: &str = "codex.rollout_compression.file.duration_ms";
    const FILE_SOURCE_BYTES_HISTOGRAM: &str = "codex.rollout_compression.file.source_bytes";
    const FILE_COMPRESSION_RATIO_HISTOGRAM: &str =
        "codex.rollout_compression.file.compression_ratio";
    const MATERIALIZE_COUNTER: &str = "codex.rollout_compression.materialize";
    const RUN_COUNTER: &str = "codex.rollout_compression.run";
    const RUN_DURATION_HISTOGRAM: &str = "codex.rollout_compression.run.duration_ms";
    const RATIO_BASIS_POINTS: u128 = 10_000;
    const TEMP_CLEANUP_COUNTER: &str = "codex.rollout_compression.temp_cleanup";

    pub(super) fn file(outcome: &'static str) {
        counter(FILE_COUNTER, &[("outcome", outcome)]);
    }

    pub(super) fn file_duration(outcome: &'static str, duration: Duration) {
        duration_histogram(FILE_DURATION_HISTOGRAM, duration, &[("outcome", outcome)]);
    }

    pub(super) fn source_bytes(outcome: &'static str, bytes: u64) {
        histogram(
            FILE_SOURCE_BYTES_HISTOGRAM,
            saturating_i64(bytes),
            &[("outcome", outcome)],
        );
    }

    pub(super) fn compressed_bytes(outcome: &'static str, bytes: u64) {
        histogram(
            FILE_COMPRESSED_BYTES_HISTOGRAM,
            saturating_i64(bytes),
            &[("outcome", outcome)],
        );
    }

    pub(super) fn compression_ratio(
        outcome: &'static str,
        source_bytes: u64,
        compressed_bytes: u64,
    ) {
        if source_bytes == 0 {
            return;
        }
        // Keep the ratio histogram integer-valued while preserving sub-percent precision.
        let ratio = (u128::from(compressed_bytes) * RATIO_BASIS_POINTS) / u128::from(source_bytes);
        histogram(
            FILE_COMPRESSION_RATIO_HISTOGRAM,
            saturating_i64(ratio),
            &[("outcome", outcome)],
        );
    }

    pub(super) fn materialize(outcome: &'static str) {
        counter(MATERIALIZE_COUNTER, &[("outcome", outcome)]);
    }

    pub(super) fn run(status: &'static str) {
        counter(RUN_COUNTER, &[("status", status)]);
    }

    pub(super) fn run_duration(status: &'static str, duration: Duration) {
        duration_histogram(RUN_DURATION_HISTOGRAM, duration, &[("status", status)]);
    }

    pub(super) fn temp_cleanup(outcome: &'static str) {
        counter(TEMP_CLEANUP_COUNTER, &[("outcome", outcome)]);
    }

    fn counter(name: &str, tags: &[(&str, &str)]) {
        let Some(metrics) = codex_otel::global() else {
            return;
        };
        let _ = metrics.counter(name, /*inc*/ 1, tags);
    }

    fn histogram(name: &str, value: i64, tags: &[(&str, &str)]) {
        let Some(metrics) = codex_otel::global() else {
            return;
        };
        let _ = metrics.histogram(name, value, tags);
    }

    fn duration_histogram(name: &str, duration: Duration, tags: &[(&str, &str)]) {
        let Some(metrics) = codex_otel::global() else {
            return;
        };
        let _ = metrics.record_duration(name, duration, tags);
    }

    fn saturating_i64(value: impl TryInto<i64>) -> i64 {
        value.try_into().unwrap_or(i64::MAX)
    }
}

/// Returns the existing rollout path, preferring the plain `.jsonl` file over
/// its `.jsonl.zst` compressed sibling.
pub async fn existing_rollout_path(path: &Path) -> Option<PathBuf> {
    path::existing_rollout_path(path).await
}

mod path {
    use std::ffi::OsStr;
    use std::fs::Metadata;
    use std::path::Path;
    use std::path::PathBuf;

    use super::COMPRESSED_SUFFIX;

    pub(super) fn compressed_rollout_path(path: &Path) -> PathBuf {
        if is_compressed_rollout_path(path) {
            return path.to_path_buf();
        }
        let mut file_name = path
            .file_name()
            .map(OsStr::to_os_string)
            .unwrap_or_else(|| OsStr::new("rollout.jsonl").to_os_string());
        file_name.push(COMPRESSED_SUFFIX);
        path.with_file_name(file_name)
    }

    pub(super) fn plain_rollout_path(path: &Path) -> PathBuf {
        let Some(file_name) = path.file_name().and_then(OsStr::to_str) else {
            return path.to_path_buf();
        };
        let Some(plain_file_name) = file_name.strip_suffix(COMPRESSED_SUFFIX) else {
            return path.to_path_buf();
        };
        path.with_file_name(plain_file_name)
    }

    pub(super) fn is_compressed_rollout_path(path: &Path) -> bool {
        path.file_name()
            .and_then(OsStr::to_str)
            .is_some_and(|name| name.ends_with(".jsonl.zst"))
    }

    pub(super) fn should_skip_compressed_sibling(path: &Path) -> bool {
        is_compressed_rollout_path(path) && plain_rollout_path(path).exists()
    }

    pub(super) async fn existing_rollout_path(path: &Path) -> Option<PathBuf> {
        existing_rollout_with_metadata(path)
            .await
            .map(|(path, _)| path)
    }

    /// Resolves the plain rollout before its compressed sibling and retains the lookup metadata.
    ///
    /// Returning the metadata lets callers inspect the selected file without a second stat.
    pub(super) async fn existing_rollout_with_metadata(path: &Path) -> Option<(PathBuf, Metadata)> {
        let plain_path = plain_rollout_path(path);
        if let Ok(metadata) = tokio::fs::metadata(plain_path.as_path()).await
            && metadata.is_file()
        {
            return Some((plain_path, metadata));
        }
        let compressed_path = compressed_rollout_path(plain_path.as_path());
        if let Ok(metadata) = tokio::fs::metadata(compressed_path.as_path()).await
            && metadata.is_file()
        {
            return Some((compressed_path, metadata));
        }
        None
    }
}

mod file_name {
    use super::COMPRESSED_SUFFIX;

    pub(super) fn parse_rollout_file_name(name: &str) -> Option<&str> {
        let name = name.strip_suffix(COMPRESSED_SUFFIX).unwrap_or(name);
        if name.starts_with("rollout-") && name.ends_with(".jsonl") {
            Some(name)
        } else {
            None
        }
    }
}

mod reader {
    use std::fs::File;
    use std::io;
    use std::io::BufRead;
    use std::io::Read;
    use std::path::Path;

    use super::RolloutLineReader;
    use super::RolloutLineReaderInner;
    use super::path;
    use tokio::io::AsyncBufReadExt;

    pub(super) async fn open_once(path: &Path) -> io::Result<RolloutLineReader> {
        let path = path::existing_rollout_path(path)
            .await
            .unwrap_or_else(|| path.to_path_buf());
        if path::is_compressed_rollout_path(path.as_path()) {
            let reader = tokio::task::spawn_blocking(move || {
                let input = File::open(path.as_path())?;
                let decoder = zstd::stream::read::Decoder::new(input)?;
                Ok::<_, io::Error>(
                    io::BufReader::new(Box::new(decoder) as Box<dyn Read + Send>).lines(),
                )
            })
            .await
            .map_err(io::Error::other)??;
            return Ok(RolloutLineReader {
                inner: RolloutLineReaderInner::Blocking(Some(reader)),
            });
        }
        let file = tokio::fs::File::open(path).await?;
        Ok(RolloutLineReader {
            inner: RolloutLineReaderInner::Plain(tokio::io::BufReader::new(file).lines()),
        })
    }
}

#[cfg(unix)]
fn create_file_with_permissions(path: &Path, permissions: &Permissions) -> io::Result<File> {
    let file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(permissions.mode() & 0o7777)
        .open(path)?;
    file.set_permissions(permissions.clone())?;
    Ok(file)
}

#[cfg(not(unix))]
fn create_file_with_permissions(path: &Path, permissions: &Permissions) -> io::Result<File> {
    let file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)?;
    file.set_permissions(permissions.clone())?;
    Ok(file)
}

fn temp_path_for(path: &Path, operation: &str) -> PathBuf {
    let mut file_name = path
        .file_name()
        .map(OsStr::to_os_string)
        .unwrap_or_else(|| OsStr::new("rollout").to_os_string());
    let counter = TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
    file_name.push(format!(
        ".{operation}.{}.{counter}{TEMP_SUFFIX}",
        std::process::id()
    ));
    path.with_file_name(file_name)
}

#[cfg(test)]
#[path = "compression_tests.rs"]
mod tests;
