//! Cold rollout encoding and publication primitives.

use std::fs::File;
use std::fs::FileTimes;
use std::fs::Permissions;
use std::io;
use std::io::Write;
use std::path::Path;
use std::time::Duration;
use std::time::SystemTime;

use super::compression_journal;
use super::compression_validation;
use super::path;

const COMPRESSION_LEVEL: i32 = 3;
const MIN_ROLLOUT_AGE: Duration = Duration::from_secs(7 * 24 * 60 * 60);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum CompressionOutcome {
    Compressed,
    SkippedNotCold,
    SkippedChanged,
    SkippedAlreadyCompressed,
}

impl CompressionOutcome {
    pub(super) fn tag(self) -> &'static str {
        match self {
            Self::Compressed => "compressed",
            Self::SkippedNotCold => "skipped_not_cold",
            Self::SkippedChanged => "skipped_changed",
            Self::SkippedAlreadyCompressed => "skipped_already_compressed",
        }
    }
}

pub(super) struct CompressionMeasurement {
    pub(super) outcome: CompressionOutcome,
    pub(super) source_bytes: Option<u64>,
    pub(super) compressed_bytes: Option<u64>,
}

impl CompressionMeasurement {
    fn new(
        outcome: CompressionOutcome,
        source_bytes: Option<u64>,
        compressed_bytes: Option<u64>,
    ) -> Self {
        Self {
            outcome,
            source_bytes,
            compressed_bytes,
        }
    }
}

enum ColdFileState {
    Cold(FileState),
    NotCold(Option<FileState>),
}

pub(super) fn compress_rollout_if_cold(path: &Path) -> io::Result<CompressionMeasurement> {
    let before = match cold_file_state(path)? {
        ColdFileState::Cold(state) => state,
        ColdFileState::NotCold(state) => {
            return Ok(CompressionMeasurement::new(
                CompressionOutcome::SkippedNotCold,
                state.map(|state| state.len),
                None,
            ));
        }
    };
    let source_bytes = Some(before.len);
    let compressed_path = path::compressed_rollout_path(path);
    if compressed_path.exists() {
        return Ok(CompressionMeasurement::new(
            CompressionOutcome::SkippedAlreadyCompressed,
            source_bytes,
            None,
        ));
    }

    let temp_dir = compressed_path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    std::fs::create_dir_all(temp_dir)?;
    let mut temp_file = tempfile::Builder::new()
        .prefix("rollout-compress-")
        .suffix(".jsonl.zst.tmp")
        .tempfile_in(temp_dir)?;
    encode_zstd_to_writer(path, temp_file.as_file_mut())?;
    temp_file.as_file_mut().flush()?;
    let validation = compression_validation::validate_rollout_replacement(path, temp_file.path())?;
    if !same_file_state(path, &before)? {
        return Ok(CompressionMeasurement::new(
            CompressionOutcome::SkippedChanged,
            source_bytes,
            None,
        ));
    }
    set_file_metadata(temp_file.as_file(), before.modified, &before.permissions)?;
    temp_file.as_file().sync_all()?;
    let compressed_bytes = temp_file.as_file().metadata()?.len();
    let journal_path = compression_journal::journal_path(path);
    compression_journal::write(journal_path.as_path(), &validation)?;

    match temp_file.persist_noclobber(compressed_path.as_path()) {
        Ok(_) => {}
        Err(error) if error.error.kind() == io::ErrorKind::AlreadyExists => {
            let _ = std::fs::remove_file(journal_path.as_path());
            return Ok(CompressionMeasurement::new(
                CompressionOutcome::SkippedAlreadyCompressed,
                source_bytes,
                None,
            ));
        }
        Err(error) => return Err(error.error),
    }
    if !same_file_state(path, &before)? {
        let _ = std::fs::remove_file(compressed_path.as_path());
        let _ = std::fs::remove_file(journal_path.as_path());
        return Ok(CompressionMeasurement::new(
            CompressionOutcome::SkippedChanged,
            source_bytes,
            None,
        ));
    }
    compression_validation::validate_rollout_replacement(path, compressed_path.as_path())
        .inspect_err(|_error| {
            let _ = std::fs::remove_file(compressed_path.as_path());
            let _ = std::fs::remove_file(journal_path.as_path());
        })?;
    std::fs::remove_file(path)?;
    match std::fs::remove_file(journal_path.as_path()) {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }
    Ok(CompressionMeasurement::new(
        CompressionOutcome::Compressed,
        source_bytes,
        Some(compressed_bytes),
    ))
}

struct FileState {
    len: u64,
    modified: SystemTime,
    permissions: Permissions,
}

fn cold_file_state(path: &Path) -> io::Result<ColdFileState> {
    let metadata = match std::fs::metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return Ok(ColdFileState::NotCold(None));
        }
        Err(error) => return Err(error),
    };
    if !metadata.is_file() {
        return Ok(ColdFileState::NotCold(None));
    }
    let modified = metadata.modified()?;
    let state = FileState {
        len: metadata.len(),
        modified,
        permissions: metadata.permissions(),
    };
    let age = SystemTime::now()
        .duration_since(modified)
        .unwrap_or(Duration::ZERO);
    if age < MIN_ROLLOUT_AGE {
        return Ok(ColdFileState::NotCold(Some(state)));
    }
    Ok(ColdFileState::Cold(state))
}

fn same_file_state(path: &Path, expected: &FileState) -> io::Result<bool> {
    match std::fs::metadata(path) {
        Ok(metadata) => Ok(metadata.len() == expected.len
            && metadata.modified()? == expected.modified
            && metadata.permissions() == expected.permissions),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error),
    }
}

fn encode_zstd_to_writer(source: &Path, output: impl Write) -> io::Result<()> {
    let mut input = File::open(source)?;
    let mut encoder = zstd::stream::write::Encoder::new(output, COMPRESSION_LEVEL)?;
    encoder.set_pledged_src_size(Some(input.metadata()?.len()))?;
    io::copy(&mut input, &mut encoder)?;
    encoder.finish()?;
    Ok(())
}

fn set_file_metadata(
    file: &File,
    modified: SystemTime,
    permissions: &Permissions,
) -> io::Result<()> {
    file.set_times(FileTimes::new().set_modified(modified))?;
    file.set_permissions(permissions.clone())
}
