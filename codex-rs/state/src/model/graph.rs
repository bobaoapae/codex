use codex_protocol::ThreadId;
use strum::AsRefStr;
use strum::Display;
use strum::EnumString;

/// Status attached to a directional thread-spawn edge.
#[derive(Debug, Clone, Copy, PartialEq, Eq, AsRefStr, Display, EnumString)]
#[strum(serialize_all = "snake_case")]
pub enum DirectionalThreadSpawnEdgeStatus {
    Open,
    Closed,
}

/// A persisted directional parent/child edge.
///
/// The current schema does not record edge creation time, so `created_at` is
/// reserved for readers that can supply it from a newer schema and remains
/// `None` for the current state database.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DirectionalThreadSpawnEdge {
    pub parent_thread_id: ThreadId,
    pub child_thread_id: ThreadId,
    pub status: DirectionalThreadSpawnEdgeStatus,
    pub created_at: Option<i64>,
}
