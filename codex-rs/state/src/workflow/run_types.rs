//! Durable run and checkpoint value types.

use anyhow::Result;
use anyhow::bail;
use serde::Deserialize;
use serde::Serialize;
use serde_json::Value;
use sqlx::Row;

use super::types::*;

/// Fixed-size equality digest for bounded immutable run parameters.
#[derive(Debug, Clone, Copy, Eq, PartialEq, Hash, Serialize, Deserialize)]
pub struct WorkflowRunParamsDigest([u8; 32]);

impl WorkflowRunParamsDigest {
    pub const fn as_bytes(self) -> [u8; 32] {
        self.0
    }
}

/// Keyset cursor ordered by `(created_at_ms, run_id)` descending.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkflowRunCursor {
    pub created_at_ms: i64,
    pub run_id: String,
}

impl WorkflowRunCursor {
    pub fn new(created_at_ms: i64, run_id: impl Into<String>) -> Result<Self> {
        let cursor = Self {
            created_at_ms,
            run_id: run_id.into(),
        };
        cursor.validate()?;
        Ok(cursor)
    }
    pub fn encode(&self) -> Result<String> {
        self.validate()?;
        Ok(serde_json::to_string(self)?)
    }
    pub fn decode(encoded: &str) -> Result<Self> {
        validate_text(encoded, MAX_JSON_BYTES, "run cursor")?;
        let cursor: Self = serde_json::from_str(encoded)
            .map_err(|error| anyhow::anyhow!("invalid workflow run cursor: {error}"))?;
        cursor.validate()?;
        Ok(cursor)
    }
    fn validate(&self) -> Result<()> {
        validate_nonnegative_i64(self.created_at_ms, "run cursor timestamp")?;
        validate_text(&self.run_id, MAX_ID_BYTES, "run cursor id")
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct WorkflowRunListFilter {
    pub thread_class: Option<WorkflowThreadClass>,
    pub status: Option<String>,
    pub root_thread_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkflowRunListRequest {
    pub filter: WorkflowRunListFilter,
    pub cursor: Option<WorkflowRunCursor>,
    pub limit: u32,
}

impl Default for WorkflowRunListRequest {
    fn default() -> Self {
        Self {
            filter: WorkflowRunListFilter::default(),
            cursor: None,
            limit: 50,
        }
    }
}

impl WorkflowRunListRequest {
    pub fn new(
        filter: WorkflowRunListFilter,
        cursor: Option<WorkflowRunCursor>,
        limit: u32,
    ) -> Result<Self> {
        let request = Self {
            filter,
            cursor,
            limit,
        };
        request.validate()?;
        Ok(request)
    }
    pub(super) fn validate(&self) -> Result<()> {
        validate_page_size(self.limit)?;
        if let Some(status) = self.filter.status.as_deref() {
            validate_nonempty_bounded(status, MAX_STATUS_BYTES, "run status filter")?;
        }
        validate_optional_text(
            self.filter.root_thread_id.as_deref(),
            MAX_ID_BYTES,
            "run root thread id filter",
        )?;
        if let Some(cursor) = &self.cursor {
            cursor.validate()?;
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkflowRunPage {
    pub runs: Vec<WorkflowRun>,
    pub next_cursor: Option<WorkflowRunCursor>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorkflowRunTransitionOutcome {
    Applied,
    AlreadyApplied,
    Stale,
    Missing,
}

/// A durable workflow run and its compare-and-swap version.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkflowRun {
    pub run_id: String,
    pub thread_id: String,
    pub root_thread_id: Option<String>,
    pub parent_run_id: Option<String>,
    pub thread_class: WorkflowThreadClass,
    pub status: String,
    pub outcome: Option<String>,
    pub idempotency_key: Option<String>,
    pub provider: Option<String>,
    pub model: Option<String>,
    pub cwd: Option<String>,
    pub metadata: Option<Value>,
    pub created_at_ms: i64,
    pub updated_at_ms: i64,
    pub started_at_ms: Option<i64>,
    pub finished_at_ms: Option<i64>,
    pub version: i64,
}

/// Input for creating one workflow run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkflowRunCreate {
    pub run_id: String,
    pub thread_id: String,
    pub root_thread_id: Option<String>,
    pub parent_run_id: Option<String>,
    pub thread_class: WorkflowThreadClass,
    pub status: String,
    pub idempotency_key: Option<String>,
    pub provider: Option<String>,
    pub model: Option<String>,
    pub cwd: Option<String>,
    pub metadata: Option<Value>,
}

impl WorkflowRunCreate {
    pub fn immutable_params_digest(&self) -> Result<WorkflowRunParamsDigest> {
        validate_run_create(self)?;
        digest_params(ImmutableWorkflowRunParams::from_create(self))
    }
    pub fn has_same_immutable_params(&self, run: &WorkflowRun) -> Result<bool> {
        Ok(self.immutable_params_digest()? == run.immutable_params_digest()?)
    }
}

impl WorkflowRun {
    pub fn immutable_params_digest(&self) -> Result<WorkflowRunParamsDigest> {
        digest_params(ImmutableWorkflowRunParams::from_run(self))
    }
}

#[derive(Serialize)]
struct ImmutableWorkflowRunParams<'a> {
    root_thread_id: Option<&'a str>,
    parent_run_id: Option<&'a str>,
    thread_class: WorkflowThreadClass,
    provider: Option<&'a str>,
    model: Option<&'a str>,
    cwd: Option<&'a str>,
    metadata: Option<&'a Value>,
}

impl<'a> ImmutableWorkflowRunParams<'a> {
    fn from_create(input: &'a WorkflowRunCreate) -> Self {
        Self {
            root_thread_id: input.root_thread_id.as_deref(),
            parent_run_id: input.parent_run_id.as_deref(),
            thread_class: input.thread_class,
            provider: input.provider.as_deref(),
            model: input.model.as_deref(),
            cwd: input.cwd.as_deref(),
            metadata: input.metadata.as_ref(),
        }
    }
    fn from_run(run: &'a WorkflowRun) -> Self {
        Self {
            root_thread_id: run.root_thread_id.as_deref(),
            parent_run_id: run.parent_run_id.as_deref(),
            thread_class: run.thread_class,
            provider: run.provider.as_deref(),
            model: run.model.as_deref(),
            cwd: run.cwd.as_deref(),
            metadata: run.metadata.as_ref(),
        }
    }
}

fn digest_params(params: ImmutableWorkflowRunParams<'_>) -> Result<WorkflowRunParamsDigest> {
    let encoded = serde_json::to_vec(&params)?;
    if encoded.len() > MAX_JSON_BYTES {
        bail!("run parameters exceed {MAX_JSON_BYTES} bytes");
    }
    Ok(digest_bytes(&encoded))
}

fn digest_bytes(bytes: &[u8]) -> WorkflowRunParamsDigest {
    const OFFSETS: [u64; 4] = [
        0xcbf29ce484222325,
        0x84222325cbf29ce4,
        0x9e3779b185ebca87,
        0x243f6a8885a308d3,
    ];
    const PRIME: u64 = 0x00000100000001b3;
    let mut lanes = OFFSETS;
    for (index, byte) in bytes.iter().copied().enumerate() {
        let lane = index % lanes.len();
        lanes[lane] ^= u64::from(byte);
        lanes[lane] = lanes[lane].wrapping_mul(PRIME);
    }
    let length = bytes.len() as u64;
    for (lane, state) in lanes.iter_mut().enumerate() {
        *state ^= length.rotate_left((lane as u32) * 11);
        *state = state.wrapping_mul(PRIME);
    }
    let mut digest = [0_u8; 32];
    for (lane, state) in lanes.into_iter().enumerate() {
        digest[lane * 8..(lane + 1) * 8].copy_from_slice(&state.to_le_bytes());
    }
    WorkflowRunParamsDigest(digest)
}

/// Input for appending a checkpoint to one run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkflowCheckpointCreate {
    pub run_id: String,
    pub checkpoint_kind: String,
    pub rollout_ordinal: Option<i64>,
    pub rollout_byte_offset: Option<i64>,
    pub payload: Value,
}

/// One ordered, immutable workflow checkpoint.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkflowCheckpoint {
    pub run_id: String,
    pub sequence: i64,
    pub checkpoint_kind: String,
    pub rollout_ordinal: Option<i64>,
    pub rollout_byte_offset: Option<i64>,
    pub payload: Value,
    pub created_at_ms: i64,
}

pub(super) fn validate_run_create(input: &WorkflowRunCreate) -> Result<()> {
    validate_text(&input.run_id, MAX_ID_BYTES, "run id")?;
    validate_text(&input.thread_id, MAX_ID_BYTES, "thread id")?;
    validate_optional_text(
        input.root_thread_id.as_deref(),
        MAX_ID_BYTES,
        "root thread id",
    )?;
    validate_optional_text(
        input.parent_run_id.as_deref(),
        MAX_ID_BYTES,
        "parent run id",
    )?;
    validate_nonempty_bounded(&input.status, MAX_STATUS_BYTES, "status")?;
    validate_optional_text(
        input.idempotency_key.as_deref(),
        MAX_IDEMPOTENCY_KEY_BYTES,
        "idempotency key",
    )?;
    validate_optional_text(input.provider.as_deref(), MAX_ID_BYTES, "provider")?;
    validate_optional_text(input.model.as_deref(), MAX_SEARCH_QUERY_BYTES, "model")?;
    validate_optional_text(input.cwd.as_deref(), MAX_PATH_BYTES, "cwd")?;
    if input.thread_class == WorkflowThreadClass::TransientJob && input.run_id != input.thread_id {
        bail!("transient job run_id, thread_id, and job_id must be identical");
    }
    if input.idempotency_key.is_some() && input.root_thread_id.is_none() {
        bail!("an idempotency key requires a root thread id");
    }
    Ok(())
}

pub(super) fn validate_checkpoint_create(input: &WorkflowCheckpointCreate) -> Result<()> {
    validate_text(&input.run_id, MAX_ID_BYTES, "run id")?;
    validate_nonempty_bounded(&input.checkpoint_kind, MAX_ID_BYTES, "checkpoint kind")?;
    validate_optional_nonnegative_i64(input.rollout_ordinal, "rollout ordinal")?;
    validate_optional_nonnegative_i64(input.rollout_byte_offset, "rollout byte offset")?;
    let payload_json = serde_json::to_string(&input.payload)?;
    validate_json_bytes(&payload_json, "checkpoint payload")
}

pub(super) fn run_matches_create(run: &WorkflowRun, input: &WorkflowRunCreate) -> bool {
    input.has_same_immutable_params(run).unwrap_or(false)
}

pub(super) fn workflow_run_from_row(row: &sqlx::sqlite::SqliteRow) -> Result<WorkflowRun> {
    Ok(WorkflowRun {
        run_id: row.try_get("run_id")?,
        thread_id: row.try_get("thread_id")?,
        root_thread_id: row.try_get("root_thread_id")?,
        parent_run_id: row.try_get("parent_run_id")?,
        thread_class: WorkflowThreadClass::from_str(
            row.try_get::<String, _>("thread_class")?.as_str(),
        )?,
        status: row.try_get("status")?,
        outcome: row.try_get("outcome")?,
        idempotency_key: row.try_get("idempotency_key")?,
        provider: row.try_get("provider")?,
        model: row.try_get("model")?,
        cwd: row.try_get("cwd")?,
        metadata: parse_optional_json(row.try_get("metadata_json")?, "run metadata")?,
        created_at_ms: row.try_get("created_at_ms")?,
        updated_at_ms: row.try_get("updated_at_ms")?,
        started_at_ms: row.try_get("started_at_ms")?,
        finished_at_ms: row.try_get("finished_at_ms")?,
        version: row.try_get("version")?,
    })
}

pub(super) fn workflow_checkpoint_from_row(
    row: &sqlx::sqlite::SqliteRow,
) -> Result<WorkflowCheckpoint> {
    Ok(WorkflowCheckpoint {
        run_id: row.try_get("run_id")?,
        sequence: row.try_get("sequence")?,
        checkpoint_kind: row.try_get("checkpoint_kind")?,
        rollout_ordinal: row.try_get("rollout_ordinal")?,
        rollout_byte_offset: row.try_get("rollout_byte_offset")?,
        payload: serde_json::from_str(&row.try_get::<String, _>("payload_json")?)?,
        created_at_ms: row.try_get("created_at_ms")?,
    })
}
