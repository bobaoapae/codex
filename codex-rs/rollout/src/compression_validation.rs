//! Lossless validation for rollout representation changes.
//!
//! Compression is a representation change, not a history rewrite.  The source and candidate
//! therefore have to agree both as raw JSONL records and as the decoded rollout metadata that
//! callers use for paging and lineage.  Keeping this check in the rollout crate lets migration
//! and cold compression share the same publication invariant.

use std::fs::File;
use std::io;
use std::io::BufRead;
use std::io::BufReader;
use std::path::Path;

use codex_protocol::ThreadId;

use super::path;
use crate::RolloutItem;
use crate::RolloutLine;

/// Bounded maximum for one logical JSONL record during a representation check.
pub(crate) const MAX_VALIDATION_LINE_BYTES: usize = 16 * 1024 * 1024;

/// Safe facts collected while validating a rollout replacement.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RolloutValidationSummary {
    /// Number of physical JSONL records, including any empty or legacy prefix records.
    pub line_count: u64,
    /// Bytes in the canonical source representation.
    pub source_bytes: u64,
    /// Final logical offset after consuming the candidate representation.
    pub candidate_bytes: u64,
    /// Session identity read from the canonical metadata item.
    pub session_id: ThreadId,
    /// First ordinal present in the decoded records.
    pub first_ordinal: Option<u64>,
    /// Last ordinal present in the decoded records.
    pub last_ordinal: Option<u64>,
    /// Timestamp of the first decoded record.
    pub first_timestamp: Option<String>,
    /// Timestamp of the last decoded record.
    pub last_timestamp: Option<String>,
}

/// Compare a plain or compressed source with a plain or compressed candidate.
///
/// Every record is compared byte-for-byte before it is decoded.  Decoded timestamps, ordinals,
/// rollout items, the SessionMeta identity, and both final offsets are checked as a second layer.
/// This rejects a valid zstd stream that contains a different but otherwise parseable rollout.
pub fn validate_rollout_replacement(
    source_path: &Path,
    candidate_path: &Path,
) -> io::Result<RolloutValidationSummary> {
    let mut source = open_reader(source_path)?;
    let mut candidate = open_reader(candidate_path)?;
    let mut source_offset = 0_u64;
    let mut candidate_offset = 0_u64;
    let mut line_count = 0_u64;
    let mut session_id = None;
    let mut first_ordinal = None;
    let mut last_ordinal = None;
    let mut first_timestamp = None;
    let mut last_timestamp = None;

    loop {
        let source_line = read_line(&mut source, &mut source_offset)?;
        let candidate_line = read_line(&mut candidate, &mut candidate_offset)?;
        match (source_line, candidate_line) {
            (None, None) => break,
            (Some(source_line), Some(candidate_line)) => {
                line_count = line_count.saturating_add(1);
                if source_line != candidate_line {
                    return Err(invalid_record(line_count, "logical bytes differ"));
                }
                let source_record = parse_decoded_record(&source_line)?;
                let candidate_record = parse_decoded_record(&candidate_line)?;
                if source_record != candidate_record {
                    return Err(invalid_record(line_count, "decoded record differs"));
                }
                if let Some(record) = source_record {
                    apply_decoded_record(
                        record,
                        line_count,
                        &mut session_id,
                        &mut first_ordinal,
                        &mut last_ordinal,
                        &mut first_timestamp,
                        &mut last_timestamp,
                    )?;
                }
            }
            (Some(_), None) => {
                return Err(invalid_record(
                    line_count.saturating_add(1),
                    "candidate ended early",
                ));
            }
            (None, Some(_)) => {
                return Err(invalid_record(
                    line_count.saturating_add(1),
                    "candidate has trailing records",
                ));
            }
        }
    }

    let session_id = session_id.ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            "rollout replacement has no session metadata",
        )
    })?;
    Ok(RolloutValidationSummary {
        line_count,
        source_bytes: source_offset,
        candidate_bytes: candidate_offset,
        session_id,
        first_ordinal,
        last_ordinal,
        first_timestamp,
        last_timestamp,
    })
}

/// Validate a compressed rollout when the original plain source is no longer available.
///
/// This is used only while recovering a journal that reached the published phase.  It verifies
/// that the retained representation is a complete, parseable rollout; equivalence to the source
/// was already established before the source was removed.
pub(crate) fn inspect_rollout(path: &Path) -> io::Result<RolloutValidationSummary> {
    let mut reader = open_reader(path)?;
    let mut offset = 0_u64;
    let mut line_count = 0_u64;
    let mut session_id = None;
    let mut first_ordinal = None;
    let mut last_ordinal = None;
    let mut first_timestamp = None;
    let mut last_timestamp = None;
    while let Some(line) = read_line(&mut reader, &mut offset)? {
        line_count = line_count.saturating_add(1);
        let Some(record) = parse_decoded_record(&line)? else {
            continue;
        };
        apply_decoded_record(
            record,
            line_count,
            &mut session_id,
            &mut first_ordinal,
            &mut last_ordinal,
            &mut first_timestamp,
            &mut last_timestamp,
        )?;
    }
    let session_id = session_id.ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            "rollout representation has no session metadata",
        )
    })?;
    Ok(RolloutValidationSummary {
        line_count,
        source_bytes: offset,
        candidate_bytes: offset,
        session_id,
        first_ordinal,
        last_ordinal,
        first_timestamp,
        last_timestamp,
    })
}

fn open_reader(path: &Path) -> io::Result<Box<dyn BufRead>> {
    let input = File::open(path)?;
    if path::is_compressed_rollout_path(path)
        || path
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| name.ends_with(".zst") || name.ends_with(".zst.tmp"))
    {
        let decoder = zstd::stream::read::Decoder::new(input)?;
        return Ok(Box::new(BufReader::new(decoder)));
    }
    Ok(Box::new(BufReader::new(input)))
}

fn read_line(reader: &mut dyn BufRead, offset: &mut u64) -> io::Result<Option<Vec<u8>>> {
    let mut line = Vec::new();
    let read = reader.read_until(b'\n', &mut line)?;
    if read == 0 {
        return Ok(None);
    }
    if line.len() > MAX_VALIDATION_LINE_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "rollout record exceeds the validation size limit",
        ));
    }
    *offset = offset.saturating_add(read as u64);
    Ok(Some(line))
}

#[derive(Debug, PartialEq)]
struct DecodedRecord {
    timestamp: String,
    ordinal: Option<u64>,
    item: serde_json::Value,
    session_id: Option<ThreadId>,
}

fn parse_decoded_record(bytes: &[u8]) -> io::Result<Option<DecodedRecord>> {
    let trimmed = bytes.strip_suffix(b"\n").unwrap_or(bytes);
    let trimmed = trimmed.strip_suffix(b"\r").unwrap_or(trimmed);
    if trimmed.is_empty() {
        return Ok(None);
    }
    let value = match serde_json::from_slice::<serde_json::Value>(trimmed) {
        Ok(value) => value,
        Err(_) => return Ok(None),
    };
    let line: RolloutLine = match crate::decode_rollout_line(value) {
        Ok(line) => line,
        Err(_) => return Ok(None),
    };
    let session_id = match &line.item {
        RolloutItem::SessionMeta(meta) => Some(meta.meta.id),
        _ => None,
    };
    Ok(Some(DecodedRecord {
        timestamp: line.timestamp,
        ordinal: line.ordinal,
        item: serde_json::to_value(&line.item).map_err(io::Error::other)?,
        session_id,
    }))
}

fn apply_decoded_record(
    record: DecodedRecord,
    line_number: u64,
    session_id: &mut Option<ThreadId>,
    first_ordinal: &mut Option<u64>,
    last_ordinal: &mut Option<u64>,
    first_timestamp: &mut Option<String>,
    last_timestamp: &mut Option<String>,
) -> io::Result<()> {
    if first_timestamp.is_none() {
        *first_timestamp = Some(record.timestamp.clone());
    }
    *last_timestamp = Some(record.timestamp);
    if first_ordinal.is_none() {
        *first_ordinal = record.ordinal;
    }
    *last_ordinal = record.ordinal.or(*last_ordinal);
    if let Some(record_session_id) = record.session_id {
        if let Some(existing) = session_id {
            if *existing != record_session_id {
                return Err(invalid_record(
                    line_number,
                    "session metadata identity changed",
                ));
            }
        } else {
            *session_id = Some(record_session_id);
        }
    }
    Ok(())
}

fn invalid_record(line_number: u64, reason: &str) -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidData,
        format!("rollout replacement differs at logical record {line_number}: {reason}"),
    )
}
