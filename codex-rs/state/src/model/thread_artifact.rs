use codex_protocol::ThreadId;
use serde_json::Value;

/// Maximum serialized JSON payload accepted for one thread artifact.
pub const MAX_THREAD_ARTIFACT_PAYLOAD_BYTES: usize = 64 * 1024;
/// Maximum byte length of a persisted artifact identifier.
pub const MAX_THREAD_ARTIFACT_ID_BYTES: usize = 128;
/// Maximum byte length of an artifact type.
pub const MAX_THREAD_ARTIFACT_TYPE_BYTES: usize = 128;
/// Maximum byte length of an artifact idempotency key.
pub const MAX_THREAD_ARTIFACT_IDENTITY_KEY_BYTES: usize = 256;
/// Maximum number of rows returned by one artifact listing page.
pub const MAX_THREAD_ARTIFACT_LIST_LIMIT: usize = 200;
/// Default size of one serialized payload read chunk.
pub const DEFAULT_THREAD_ARTIFACT_READ_CHUNK_BYTES: usize = 16 * 1024;
/// Absolute upper bound for one serialized payload read chunk.
pub const MAX_THREAD_ARTIFACT_READ_CHUNK_BYTES: usize = 64 * 1024;

/// A bounded artifact durably associated with one thread.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ThreadArtifact {
    /// Stable, server-assigned UUIDv7 artifact identity.
    pub id: String,
    /// Thread that owns this artifact.
    pub thread_id: ThreadId,
    /// Client-defined artifact category.
    pub artifact_type: String,
    /// Client-defined stable identity within the owning thread and artifact category.
    pub identity_key: String,
    /// Bounded, client-defined artifact metadata.
    pub payload: Value,
    /// Integer Unix timestamp in seconds when the artifact was attached.
    pub created_at: i64,
}

/// Result of attaching one uniquely identified thread artifact.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ThreadArtifactAttachmentOutcome {
    /// A new durable artifact was created.
    Created(ThreadArtifact),
    /// The artifact was already attached; its payload and creation time are unchanged.
    Existing(ThreadArtifact),
}

/// Result of removing a thread artifact.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ThreadArtifactRemovalOutcome {
    /// An attached artifact was removed.
    Removed(ThreadArtifact),
    /// No artifact with the requested identity was attached.
    NotFound,
}

/// One deterministically ordered page of artifacts across selected threads.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ThreadArtifactPage {
    /// Artifacts ordered by thread identity, creation time, and artifact identity.
    pub artifacts: Vec<ThreadArtifact>,
    /// Opaque cursor for the next page, or `None` when the selection is exhausted.
    pub next_cursor: Option<String>,
}

/// Encoding used at the artifact read boundary.
///
/// Chunks contain consecutive UTF-8 bytes from the canonical serialized JSON
/// payload. A caller must concatenate chunks before parsing the complete JSON
/// document; individual chunks are only guaranteed to end on a UTF-8 boundary.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ThreadArtifactReadEncoding {
    JsonUtf8,
}

/// One bounded page of a serialized artifact payload.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ThreadArtifactReadPage {
    /// Artifact whose payload was read.
    pub artifact_id: String,
    /// Byte offset of `chunk` in the serialized payload.
    pub offset: usize,
    /// UTF-8 chunk of the serialized JSON payload.
    pub chunk: String,
    /// Explicit boundary encoding for the chunk.
    pub encoding: ThreadArtifactReadEncoding,
    /// Opaque cursor for the next chunk, or `None` when complete.
    pub next_cursor: Option<String>,
    /// Whether the complete serialized payload was returned by this page.
    pub complete: bool,
    /// Total serialized payload size in bytes.
    pub total_bytes: usize,
}

/// Compatibility alias for callers that call a payload page a read result.
pub type ThreadArtifactReadResult = ThreadArtifactReadPage;
