use codex_protocol::ThreadId;
use serde::Deserialize;
use serde::Serialize;

/// Lifecycle status attached to a directional thread-spawn edge.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ThreadSpawnEdgeStatus {
    /// The child thread is still live or resumable as an open spawned agent.
    Open,
    /// The child thread has been closed from the parent/child graph's perspective.
    Closed,
}

/// A single persisted spawn edge without traversal metadata.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ThreadSpawnEdge {
    pub parent_id: ThreadId,
    pub child_id: ThreadId,
    pub status: ThreadSpawnEdgeStatus,
    /// Reserved for state backends that persist edge creation timestamps.
    pub created_at: Option<i64>,
}

/// A spawn edge annotated with its deterministic position in a root traversal.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ThreadSpawnEdgeDetail {
    pub parent_id: ThreadId,
    pub child_id: ThreadId,
    pub status: ThreadSpawnEdgeStatus,
    pub created_at: Option<i64>,
    /// Number of edges between the root and this child. Direct children are depth one.
    pub depth: u32,
    /// Zero-based breadth-first traversal order, stable for a fixed graph.
    pub order: u64,
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    #[test]
    fn thread_spawn_edge_status_serializes_as_snake_case() {
        assert_eq!(
            serde_json::to_string(&ThreadSpawnEdgeStatus::Open)
                .expect("open status should serialize"),
            "\"open\""
        );
        assert_eq!(
            serde_json::to_string(&ThreadSpawnEdgeStatus::Closed)
                .expect("closed status should serialize"),
            "\"closed\""
        );
        assert_eq!(
            serde_json::from_str::<ThreadSpawnEdgeStatus>("\"open\"")
                .expect("open status should deserialize"),
            ThreadSpawnEdgeStatus::Open
        );
        assert_eq!(
            serde_json::from_str::<ThreadSpawnEdgeStatus>("\"closed\"")
                .expect("closed status should deserialize"),
            ThreadSpawnEdgeStatus::Closed
        );
    }
}
