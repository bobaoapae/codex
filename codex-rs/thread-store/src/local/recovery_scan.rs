//! Strict, offset-preserving JSONL scanning for recovery.

use std::io;
use std::io::BufRead;
use std::io::BufReader;

use codex_protocol::protocol::SessionMetaLine;
use codex_rollout::RolloutItem;
use codex_rollout::RolloutLine;

use crate::RecoveryLimits;

pub(super) const MAX_BUFFER_BYTES: u64 = 64 * 1024 * 1024;
const MAX_RECORD_BYTES: usize = 16 * 1024 * 1024;

pub(crate) struct RolloutScan {
    pub(crate) meta: Option<SessionMetaLine>,
    pub(crate) records: Vec<RecoveryRecord>,
    pub(crate) item_count: usize,
    pub(crate) buffer_limit_exceeded: bool,
    pub(crate) next_ordinal: u64,
    pub(crate) end_byte_offset: u64,
}

#[derive(Clone)]
pub(crate) struct RecoveryRecord {
    pub(crate) ordinal: u64,
    pub(crate) start_byte_offset: u64,
    pub(crate) end_byte_offset: u64,
    pub(crate) item: RolloutItem,
}

/// Reads every non-empty rollout record and preserves its logical JSONL byte span.
///
/// `open_rollout_seekable_reader` decodes compressed rollouts into an anonymous temporary file,
/// so all offsets returned here address the same logical bytes used by paginated history.
pub(crate) fn scan_rollout(
    path: &std::path::Path,
    thread_id: codex_protocol::ThreadId,
    limits: RecoveryLimits,
) -> io::Result<RolloutScan> {
    let reader = codex_rollout::open_rollout_seekable_reader(path)?;
    let mut reader = BufReader::new(reader);
    let mut records = Vec::new();
    let mut next_ordinal = 0_u64;
    let mut byte_offset = 0_u64;
    let mut line_bytes = Vec::new();
    let mut item_count = 0_usize;
    let mut meta = None;
    let mut buffer_limit_exceeded = false;

    loop {
        line_bytes.clear();
        let bytes_read = reader.read_until(b'\n', &mut line_bytes)?;
        if bytes_read == 0 {
            break;
        }
        if bytes_read > MAX_RECORD_BYTES {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "rollout record exceeds the recovery record-size limit",
            ));
        }
        let start_byte_offset = byte_offset;
        byte_offset = byte_offset
            .checked_add(u64::try_from(bytes_read).map_err(|_| {
                io::Error::new(io::ErrorKind::InvalidData, "rollout line is too large")
            })?)
            .ok_or_else(|| {
                io::Error::new(io::ErrorKind::InvalidData, "rollout byte offset overflow")
            })?;
        if line_bytes.iter().all(u8::is_ascii_whitespace) {
            continue;
        }

        let value: serde_json::Value = serde_json::from_slice(&line_bytes).map_err(|_| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!("invalid rollout JSON near byte offset {start_byte_offset}"),
            )
        })?;
        let line: RolloutLine = codex_rollout::decode_rollout_line(value).map_err(|_| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!("invalid rollout item near byte offset {start_byte_offset}"),
            )
        })?;
        let ordinal = line.ordinal.unwrap_or(next_ordinal);
        if ordinal != next_ordinal {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("rollout ordinal mismatch near byte offset {start_byte_offset}"),
            ));
        }
        next_ordinal = next_ordinal.checked_add(1).ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidData, "rollout ordinal overflow")
        })?;
        if item_count == 0 {
            let RolloutItem::SessionMeta(session_meta) = &line.item else {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("rollout for thread {thread_id} has no canonical session metadata"),
                ));
            };
            meta = Some(session_meta.clone());
        }
        item_count = item_count.checked_add(1).ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidData, "rollout item count overflow")
        })?;
        if ordinal == 0 && !matches!(&line.item, RolloutItem::SessionMeta(_)) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("rollout for thread {thread_id} does not start with session metadata"),
            ));
        }
        if !buffer_limit_exceeded
            && (item_count > limits.max_items
                || byte_offset > limits.max_serialized_bytes
                || byte_offset > MAX_BUFFER_BYTES)
        {
            buffer_limit_exceeded = true;
        }
        if !buffer_limit_exceeded {
            records.push(RecoveryRecord {
                ordinal,
                start_byte_offset,
                end_byte_offset: byte_offset,
                item: line.item,
            });
        }
    }

    if meta.is_none() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("rollout for thread {thread_id} has no canonical session metadata"),
        ));
    }

    Ok(RolloutScan {
        meta,
        records,
        item_count,
        buffer_limit_exceeded,
        next_ordinal,
        end_byte_offset: byte_offset,
    })
}
