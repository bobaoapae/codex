//! Durable handoff markers for cold rollout representation changes.

use std::fs::File;
use std::io;
use std::io::Read;
use std::io::Write;
use std::path::Path;
use std::path::PathBuf;
use std::sync::atomic::Ordering;

use codex_protocol::ThreadId;

use super::compression_validation::RolloutValidationSummary;
use super::compression_validation::inspect_rollout;
use super::path;

const JOURNAL_PREFIX: &str = ".";
const JOURNAL_SUFFIX: &str = ".pending";
const JOURNAL_VERSION: &str = "1";
const MAX_JOURNAL_BYTES: usize = 1024;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct CompressionJournal {
    pub(super) source_bytes: u64,
    pub(super) line_count: u64,
    pub(super) session_id: ThreadId,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum RecoveryOutcome {
    /// The canonical plain source was retained and an incomplete sibling was removed.
    PlainWins,
    /// The source had already been removed after validation; the compressed sibling is retained.
    CompressedWins,
}

pub(super) fn journal_path(plain_path: &Path) -> PathBuf {
    let compressed_path = path::compressed_rollout_path(plain_path);
    let filename = compressed_path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("rollout.jsonl.zst");
    compressed_path.with_file_name(format!("{JOURNAL_PREFIX}{filename}{JOURNAL_SUFFIX}"))
}

pub(super) fn is_journal_path(path: &Path) -> bool {
    path.file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| {
            name.starts_with(JOURNAL_PREFIX)
                && name.ends_with(JOURNAL_SUFFIX)
                && name.contains(".jsonl.zst")
        })
}

pub(super) fn write(path: &Path, summary: &RolloutValidationSummary) -> io::Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| io::Error::other("compression journal has no parent"))?;
    std::fs::create_dir_all(parent)?;
    let temp_path = path.with_file_name(format!(
        "{}.{}.tmp",
        path.file_name().unwrap_or_default().to_string_lossy(),
        super::TEMP_COUNTER.fetch_add(1, Ordering::Relaxed)
    ));
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(temp_path.as_path())?;
    write_contents(&mut file, summary)?;
    file.sync_all()?;
    drop(file);
    replace_file(temp_path.as_path(), path)?;
    sync_parent(path)
}

pub(super) fn read(path: &Path) -> io::Result<CompressionJournal> {
    let mut file = File::open(path)?;
    let mut contents = Vec::new();
    std::io::Read::by_ref(&mut file)
        .take(MAX_JOURNAL_BYTES as u64 + 1)
        .read_to_end(&mut contents)?;
    if contents.len() > MAX_JOURNAL_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "compression journal exceeds size limit",
        ));
    }
    let mut version = None;
    let mut source_bytes = None;
    let mut line_count = None;
    let mut session_id = None;
    for line in contents.split(|byte| *byte == b'\n') {
        let Some(separator) = line.iter().position(|byte| *byte == b'=') else {
            continue;
        };
        let (key, value) = line.split_at(separator);
        let value = &value[1..];
        let key = std::str::from_utf8(key).map_err(|error| invalid_journal(error.to_string()))?;
        let value =
            std::str::from_utf8(value).map_err(|error| invalid_journal(error.to_string()))?;
        match key {
            "version" => version = Some(value.to_string()),
            "source_bytes" => {
                source_bytes = Some(
                    value
                        .parse::<u64>()
                        .map_err(|error| invalid_journal(error.to_string()))?,
                )
            }
            "line_count" => {
                line_count = Some(
                    value
                        .parse::<u64>()
                        .map_err(|error| invalid_journal(error.to_string()))?,
                )
            }
            "session_id" => {
                session_id = Some(
                    ThreadId::from_string(value)
                        .map_err(|error| invalid_journal(error.to_string()))?,
                )
            }
            _ => {}
        }
    }
    if version.as_deref() != Some(JOURNAL_VERSION) {
        return Err(invalid_journal("unsupported compression journal version"));
    }
    Ok(CompressionJournal {
        source_bytes: source_bytes
            .ok_or_else(|| invalid_journal("journal has no source length"))?,
        line_count: line_count.ok_or_else(|| invalid_journal("journal has no line count"))?,
        session_id: session_id.ok_or_else(|| invalid_journal("journal has no session id"))?,
    })
}

pub(super) fn recover(path: &Path) -> io::Result<RecoveryOutcome> {
    let journal = read(path)?;
    let compressed_path = compressed_path_from_journal(path)?;
    let plain_path = path::plain_rollout_path(compressed_path.as_path());
    if plain_path.exists() {
        remove_if_present(compressed_path.as_path())?;
        remove_if_present(path)?;
        return Ok(RecoveryOutcome::PlainWins);
    }
    if !compressed_path.exists() {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            "compression journal has no source or published representation",
        ));
    }
    let summary = inspect_rollout(compressed_path.as_path())?;
    if summary.source_bytes != journal.source_bytes
        || summary.line_count != journal.line_count
        || summary.session_id != journal.session_id
    {
        return Err(invalid_journal(
            "published rollout does not match its journal",
        ));
    }
    remove_if_present(path)?;
    Ok(RecoveryOutcome::CompressedWins)
}

fn write_contents(file: &mut File, summary: &RolloutValidationSummary) -> io::Result<()> {
    write!(
        file,
        "version={JOURNAL_VERSION}\nsource_bytes={}\nline_count={}\nsession_id={}\n",
        summary.source_bytes, summary.line_count, summary.session_id
    )
}

fn compressed_path_from_journal(path: &Path) -> io::Result<PathBuf> {
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| invalid_journal("compression journal has no valid filename"))?;
    let compressed_name = name
        .strip_prefix(JOURNAL_PREFIX)
        .and_then(|name| name.strip_suffix(JOURNAL_SUFFIX))
        .filter(|name| name.ends_with(".jsonl.zst"))
        .ok_or_else(|| invalid_journal("compression journal filename is invalid"))?;
    Ok(path.with_file_name(compressed_name))
}

fn replace_file(source: &Path, destination: &Path) -> io::Result<()> {
    match std::fs::rename(source, destination) {
        Ok(()) => Ok(()),
        Err(error) => Err(error),
    }
}

fn remove_if_present(path: &Path) -> io::Result<()> {
    match std::fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}

fn sync_parent(path: &Path) -> io::Result<()> {
    #[cfg(unix)]
    {
        let parent = path
            .parent()
            .ok_or_else(|| io::Error::other("compression journal has no parent"))?;
        File::open(parent)?.sync_all()
    }
    #[cfg(not(unix))]
    {
        let _ = path;
        Ok(())
    }
}

fn invalid_journal(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message.into())
}
