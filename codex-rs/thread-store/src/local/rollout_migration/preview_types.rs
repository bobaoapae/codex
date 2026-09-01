use codex_protocol::ThreadId;
use codex_protocol::protocol::HistoryPosition;
use codex_protocol::protocol::SessionSource;
use codex_protocol::protocol::ThreadHistoryMode;
use codex_protocol::protocol::ThreadSource;
use serde::Deserialize;
use serde::Serialize;
use std::path::PathBuf;

/// Version of the on-disk frozen rollout preview contract.
pub const FROZEN_PREVIEW_SCHEMA_VERSION: u32 = 1;

/// Explicit classification used by rollout migration preview.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum RolloutMigrationPreviewThreadClass {
    Interactive,
    SubAgent,
    TransientJob,
    Internal,
    LegacyExec,
}

/// Read-only status of one discovered physical rollout.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RolloutMigrationPreviewStatus {
    Eligible,
    AlreadyPaginated,
    Skipped,
    Busy,
    Invalid,
    Malformed,
    SkippedInternalReceipt,
}

/// Physical representation captured by a frozen preview.
///
/// The logical rollout identity is the plain `.jsonl` path plus rollout ID. A representation
/// transition can therefore be accepted only when the signed preview explicitly permits it.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RolloutMigrationPreviewRepresentation {
    Plain,
    Zstd,
}

/// Frozen discovery watermark ordered by filename creation time and rollout identity.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RolloutMigrationPreviewWatermark {
    pub created_at: String,
    pub rollout_id: Option<ThreadId>,
}

/// Preview options that cannot mutate rollout or state storage.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct RolloutMigrationPreviewOptions {
    pub thread_ids: Vec<ThreadId>,
    pub max_mib_per_second: Option<u64>,
}

/// Read-only report for one selected physical rollout representation.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RolloutMigrationPreviewEntry {
    pub rollout_path: PathBuf,
    pub plain_path: PathBuf,
    /// Size of the physical source at preview time.
    pub source_size_bytes: Option<u64>,
    /// Millisecond-resolution modified time of the physical source at preview time.
    pub source_mtime_ms: Option<i64>,
    /// Digest of the logical line stream, allowing a permitted representation transition to
    /// reattest content even though compressed and plain physical sizes differ.
    pub source_content_digest: Option<String>,
    /// Physical source representation at preview time.
    pub representation: RolloutMigrationPreviewRepresentation,
    /// Whether a plain-to-zstd (or zstd-to-plain) transition is part of this report's identity.
    /// This is digest-bound and prevents apply from guessing a replacement path.
    pub representation_transition_allowed: bool,
    pub rollout_id: Option<ThreadId>,
    pub thread_id: Option<ThreadId>,
    pub class: Option<RolloutMigrationPreviewThreadClass>,
    pub source: Option<SessionSource>,
    pub thread_source: Option<ThreadSource>,
    pub history_mode: Option<ThreadHistoryMode>,
    pub forked_from_id: Option<ThreadId>,
    pub history_base: Option<HistoryPosition>,
    pub status: RolloutMigrationPreviewStatus,
    pub plain_bytes: u64,
    pub zst_bytes: u64,
    pub canonical_bytes: u64,
    pub estimated_temp_space_bytes: u64,
    pub indexable_allowlisted_items: usize,
    pub excluded_items: usize,
    pub malformed_items: usize,
    pub trailing_partial_items: usize,
    pub skipped_items: usize,
    pub pending_marker: bool,
    pub busy: bool,
    pub message: Option<String>,
}

/// Aggregate counts from a migration preview.
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RolloutMigrationPreviewCounts {
    pub interactive: usize,
    pub sub_agent: usize,
    pub transient_job: usize,
    pub internal: usize,
    pub legacy_exec: usize,
    pub eligible: usize,
    pub skipped: usize,
    pub busy: usize,
    pub invalid: usize,
    pub malformed: usize,
    pub skipped_internal_receipts: usize,
}

/// Complete dry-run report. No canonical rollout, SQLite state, or journal is written.
///
/// The ordered entries, watermark, and provenance digest form the durable input for an explicit
/// apply. Aggregate counters remain informational and are not used to rediscover sources.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RolloutMigrationPreviewReport {
    pub schema_version: u32,
    pub watermark: Option<RolloutMigrationPreviewWatermark>,
    pub entries: Vec<RolloutMigrationPreviewEntry>,
    pub counts: RolloutMigrationPreviewCounts,
    pub plain_bytes: u64,
    pub zst_bytes: u64,
    pub canonical_bytes: u64,
    pub estimated_temp_space_bytes: u64,
    pub indexable_allowlisted_items: usize,
    pub excluded_items: usize,
    pub malformed_items: usize,
    pub trailing_partial_items: usize,
    pub skipped_items: usize,
    pub pending_markers: usize,
    pub skipped_internal_receipts: usize,
    pub estimated_duration_ms: Option<u64>,
    /// Digest over the schema version, watermark, and ordered physical entries.
    pub provenance_digest: Option<String>,
    /// Whether this report can be consumed by an apply operation before source attestation.
    pub can_recover: bool,
    /// Safe explanation when a report was structurally known to be stale or unusable.
    pub stale_reason: Option<String>,
}

impl Default for RolloutMigrationPreviewReport {
    fn default() -> Self {
        Self {
            schema_version: FROZEN_PREVIEW_SCHEMA_VERSION,
            watermark: None,
            entries: Vec::new(),
            counts: RolloutMigrationPreviewCounts::default(),
            plain_bytes: 0,
            zst_bytes: 0,
            canonical_bytes: 0,
            estimated_temp_space_bytes: 0,
            indexable_allowlisted_items: 0,
            excluded_items: 0,
            malformed_items: 0,
            trailing_partial_items: 0,
            skipped_items: 0,
            pending_markers: 0,
            skipped_internal_receipts: 0,
            estimated_duration_ms: None,
            provenance_digest: None,
            can_recover: true,
            stale_reason: None,
        }
    }
}

/// A durable, digest-bound preview accepted by programmatic rollout apply APIs.
///
/// This alias deliberately keeps the JSON report and the programmatic input identical: callers
/// cannot accidentally reconstruct a preview by rediscovering the rollout directory. Use
/// [`RolloutMigrationPreviewReport::freeze`] to validate report input before applying it.
pub type FrozenPreview = RolloutMigrationPreviewReport;

#[derive(Serialize)]
struct ProvenanceDigestInput<'a> {
    schema_version: u32,
    watermark: &'a Option<RolloutMigrationPreviewWatermark>,
    entries: &'a [RolloutMigrationPreviewEntry],
}

impl RolloutMigrationPreviewReport {
    /// Compute the digest that binds the physical source set and its ordering.
    pub fn compute_provenance_digest(&self) -> Result<String, serde_json::Error> {
        let input = ProvenanceDigestInput {
            schema_version: self.schema_version,
            watermark: &self.watermark,
            entries: self.entries.as_slice(),
        };
        let bytes = serde_json::to_vec(&input)?;
        use sha2::Digest as _;
        Ok(format!("sha256:{:x}", sha2::Sha256::digest(bytes)))
    }

    /// Finalize a read-only report into the digest-bound input consumed by apply.
    pub fn freeze(mut self) -> Result<FrozenPreview, serde_json::Error> {
        self.schema_version = FROZEN_PREVIEW_SCHEMA_VERSION;
        self.provenance_digest = Some(self.compute_provenance_digest()?);
        self.can_recover = self.entries.iter().all(|entry| {
            entry.source_size_bytes.is_some()
                && entry.source_mtime_ms.is_some()
                && entry.source_content_digest.is_some()
        });
        self.stale_reason = (!self.can_recover)
            .then(|| "one or more frozen sources lacks physical metadata".to_string());
        Ok(self)
    }

    /// Validate the durable, self-authenticating portion of a preview before touching state.
    pub fn validate_frozen(&self) -> Result<(), String> {
        if self.schema_version != FROZEN_PREVIEW_SCHEMA_VERSION {
            return Err(format!(
                "unsupported rollout preview schema version {}",
                self.schema_version
            ));
        }
        if !self.can_recover {
            return Err(self
                .stale_reason
                .clone()
                .unwrap_or_else(|| "rollout preview is marked stale".to_string()));
        }
        let Some(expected) = self.provenance_digest.as_deref() else {
            return Err("rollout preview is missing its provenance digest".to_string());
        };
        let actual = self
            .compute_provenance_digest()
            .map_err(|error| format!("failed to compute rollout preview digest: {error}"))?;
        if expected != actual {
            return Err(
                "rollout preview provenance digest does not match its contents".to_string(),
            );
        }
        let mut identities = std::collections::HashSet::new();
        for entry in &self.entries {
            if entry.source_size_bytes.is_none()
                || entry.source_mtime_ms.is_none()
                || entry.source_content_digest.is_none()
            {
                return Err(format!(
                    "rollout preview is missing source metadata for {}",
                    entry.rollout_path.display()
                ));
            }
            let identity = (
                entry.rollout_id,
                entry.plain_path.to_string_lossy().into_owned(),
            );
            if !identities.insert(identity) {
                return Err(format!(
                    "rollout preview contains duplicate source identity for {}",
                    entry.plain_path.display()
                ));
            }
        }
        Ok(())
    }
}
