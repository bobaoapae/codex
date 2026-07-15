//! Validation and state types for materialized resume checkpoints.

use std::path::Path;
use std::path::PathBuf;

use anyhow::Context;
use serde::Deserialize;
use serde::Serialize;
use tokio::io::AsyncReadExt;
use tokio::io::AsyncSeekExt;

pub const CHECKPOINT_SCHEMA_VERSION: i64 = 1;
const BOUNDARY_BYTES: u64 = 4096;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuntimeCheckpoint<T> {
    pub thread_id: String,
    pub rollout_path: PathBuf,
    pub next_rollout_byte_offset: u64,
    pub next_rollout_ordinal: u64,
    pub session_meta_hash: String,
    pub boundary_hash: String,
    pub checkpoint: T,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct StoredRuntimeCheckpoint {
    pub(crate) thread_id: String,
    pub(crate) schema_version: i64,
    pub(crate) rollout_path: String,
    pub(crate) next_rollout_byte_offset: i64,
    pub(crate) next_rollout_ordinal: i64,
    pub(crate) session_meta_hash: String,
    pub(crate) boundary_hash: String,
    pub(crate) checkpoint_json: String,
}

impl<T: Serialize> RuntimeCheckpoint<T> {
    pub(crate) fn into_stored(self) -> anyhow::Result<StoredRuntimeCheckpoint> {
        Ok(StoredRuntimeCheckpoint {
            thread_id: self.thread_id,
            schema_version: CHECKPOINT_SCHEMA_VERSION,
            rollout_path: self.rollout_path.to_string_lossy().into_owned(),
            next_rollout_byte_offset: i64::try_from(self.next_rollout_byte_offset)
                .context("checkpoint byte offset exceeds sqlite integer")?,
            next_rollout_ordinal: i64::try_from(self.next_rollout_ordinal)
                .context("checkpoint ordinal exceeds sqlite integer")?,
            session_meta_hash: self.session_meta_hash,
            boundary_hash: self.boundary_hash,
            checkpoint_json: serde_json::to_string(&self.checkpoint)
                .context("serialize runtime checkpoint")?,
        })
    }
}

impl StoredRuntimeCheckpoint {
    pub(crate) async fn validate<T: for<'de> Deserialize<'de>>(
        self,
        expected_path: &Path,
        expected_session_meta_hash: &str,
    ) -> anyhow::Result<Option<RuntimeCheckpoint<T>>> {
        if self.schema_version != CHECKPOINT_SCHEMA_VERSION
            || self.rollout_path != expected_path.to_string_lossy()
            || self.session_meta_hash != expected_session_meta_hash
        {
            return Ok(None);
        }
        let Ok(offset) = u64::try_from(self.next_rollout_byte_offset) else {
            return Ok(None);
        };
        let Ok(ordinal) = u64::try_from(self.next_rollout_ordinal) else {
            return Ok(None);
        };
        let metadata = tokio::fs::metadata(expected_path)
            .await
            .context("stat checkpoint rollout")?;
        if metadata.len() < offset {
            return Ok(None);
        }
        if boundary_hash(expected_path, offset).await? != self.boundary_hash {
            return Ok(None);
        }
        let checkpoint = match serde_json::from_str(&self.checkpoint_json) {
            Ok(checkpoint) => checkpoint,
            Err(_) => return Ok(None),
        };
        Ok(Some(RuntimeCheckpoint {
            thread_id: self.thread_id,
            rollout_path: expected_path.to_path_buf(),
            next_rollout_byte_offset: offset,
            next_rollout_ordinal: ordinal,
            session_meta_hash: self.session_meta_hash,
            boundary_hash: self.boundary_hash,
            checkpoint,
        }))
    }
}

pub async fn boundary_hash(path: &Path, offset: u64) -> anyhow::Result<String> {
    let start = offset.saturating_sub(BOUNDARY_BYTES);
    let len = usize::try_from(offset - start).context("checkpoint boundary length")?;
    let mut file = tokio::fs::File::open(path)
        .await
        .context("open checkpoint rollout")?;
    file.seek(std::io::SeekFrom::Start(start)).await?;
    let mut bytes = vec![0; len];
    file.read_exact(&mut bytes).await?;
    Ok(blake3::hash(&bytes).to_hex().to_string())
}

pub fn content_hash(bytes: &[u8]) -> String {
    blake3::hash(bytes).to_hex().to_string()
}
