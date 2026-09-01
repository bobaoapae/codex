use codex_protocol::ThreadId;
use codex_state::StateRuntime;
use std::sync::Arc;

use crate::AgentGraphStore;
use crate::AgentGraphStoreError;
use crate::AgentGraphStoreFuture;
use crate::ThreadSpawnEdge;
use crate::ThreadSpawnEdgeDetail;
use crate::ThreadSpawnEdgeStatus;
use std::collections::HashSet;
use std::collections::VecDeque;

const MAX_EDGE_DETAILS: usize = 10_000;
const MAX_EDGE_DEPTH: u32 = 128;

/// SQLite-backed implementation of [`AgentGraphStore`] using an existing state runtime.
#[derive(Clone)]
pub struct LocalAgentGraphStore {
    state_db: Arc<StateRuntime>,
}

impl std::fmt::Debug for LocalAgentGraphStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LocalAgentGraphStore")
            .field("sqlite", self.state_db.sqlite())
            .finish_non_exhaustive()
    }
}

impl LocalAgentGraphStore {
    /// Create a local graph store from an already-initialized state runtime.
    pub fn new(state_db: Arc<StateRuntime>) -> Self {
        Self { state_db }
    }
}

impl AgentGraphStore for LocalAgentGraphStore {
    fn upsert_thread_spawn_edge(
        &self,
        parent_thread_id: ThreadId,
        child_thread_id: ThreadId,
        status: ThreadSpawnEdgeStatus,
    ) -> AgentGraphStoreFuture<'_, ()> {
        Box::pin(async move {
            self.state_db
                .upsert_thread_spawn_edge(
                    parent_thread_id,
                    child_thread_id,
                    to_state_status(status),
                )
                .await
                .map_err(internal_error)
        })
    }

    fn set_thread_spawn_edge_status(
        &self,
        child_thread_id: ThreadId,
        status: ThreadSpawnEdgeStatus,
    ) -> AgentGraphStoreFuture<'_, ()> {
        Box::pin(async move {
            self.state_db
                .set_thread_spawn_edge_status(child_thread_id, to_state_status(status))
                .await
                .map_err(internal_error)
        })
    }

    fn list_thread_spawn_children(
        &self,
        parent_thread_id: ThreadId,
        status_filter: Option<ThreadSpawnEdgeStatus>,
    ) -> AgentGraphStoreFuture<'_, Vec<ThreadId>> {
        Box::pin(async move {
            if let Some(status) = status_filter {
                return self
                    .state_db
                    .list_thread_spawn_children_with_status(
                        parent_thread_id,
                        to_state_status(status),
                    )
                    .await
                    .map_err(internal_error);
            }

            self.state_db
                .list_thread_spawn_children(parent_thread_id)
                .await
                .map_err(internal_error)
        })
    }

    fn list_thread_spawn_descendants(
        &self,
        root_thread_id: ThreadId,
        status_filter: Option<ThreadSpawnEdgeStatus>,
    ) -> AgentGraphStoreFuture<'_, Vec<ThreadId>> {
        Box::pin(async move {
            match status_filter {
                Some(status) => self
                    .state_db
                    .list_thread_spawn_descendants_with_status(
                        root_thread_id,
                        to_state_status(status),
                    )
                    .await
                    .map_err(internal_error),
                None => self
                    .state_db
                    .list_thread_spawn_descendants(root_thread_id)
                    .await
                    .map_err(internal_error),
            }
        })
    }

    fn list_thread_spawn_edge_details(
        &self,
        root_thread_id: ThreadId,
        status_filter: Option<ThreadSpawnEdgeStatus>,
    ) -> AgentGraphStoreFuture<'_, Vec<ThreadSpawnEdgeDetail>> {
        Box::pin(async move {
            let status_filter = status_filter.map(to_state_status);
            let mut queue = VecDeque::from([(root_thread_id, 0_u32, vec![root_thread_id])]);
            let mut seen_edges = HashSet::new();
            let mut details = Vec::new();

            while let Some((parent_id, parent_depth, path)) = queue.pop_front() {
                let edges = self
                    .state_db
                    .list_thread_spawn_edge_records(parent_id, status_filter)
                    .await
                    .map_err(internal_error)?;
                if parent_depth >= MAX_EDGE_DEPTH && !edges.is_empty() {
                    return Err(bounded_error("agent graph depth limit exceeded"));
                }
                for edge in edges {
                    let edge_key = (edge.parent_thread_id, edge.child_thread_id);
                    if !seen_edges.insert(edge_key) || path.contains(&edge.child_thread_id) {
                        return Err(invalid_graph_error("agent graph contains a cycle"));
                    }
                    if details.len() >= MAX_EDGE_DETAILS {
                        return Err(bounded_error("agent graph edge limit exceeded"));
                    }
                    let depth = parent_depth + 1;
                    let child_id = edge.child_thread_id;
                    details.push(ThreadSpawnEdgeDetail {
                        parent_id: edge.parent_thread_id,
                        child_id,
                        status: from_state_status(edge.status),
                        created_at: edge.created_at,
                        depth,
                        order: details.len() as u64,
                    });
                    let mut child_path = path.clone();
                    child_path.push(child_id);
                    queue.push_back((child_id, depth, child_path));
                }
            }
            Ok(details)
        })
    }

    fn get_thread_spawn_edge(
        &self,
        parent_thread_id: ThreadId,
        child_thread_id: ThreadId,
    ) -> AgentGraphStoreFuture<'_, Option<ThreadSpawnEdge>> {
        Box::pin(async move {
            self.state_db
                .get_thread_spawn_edge(parent_thread_id, child_thread_id)
                .await
                .map(|edge| edge.map(from_state_edge))
                .map_err(internal_error)
        })
    }
}

fn to_state_status(status: ThreadSpawnEdgeStatus) -> codex_state::DirectionalThreadSpawnEdgeStatus {
    match status {
        ThreadSpawnEdgeStatus::Open => codex_state::DirectionalThreadSpawnEdgeStatus::Open,
        ThreadSpawnEdgeStatus::Closed => codex_state::DirectionalThreadSpawnEdgeStatus::Closed,
    }
}

fn from_state_status(
    status: codex_state::DirectionalThreadSpawnEdgeStatus,
) -> ThreadSpawnEdgeStatus {
    match status {
        codex_state::DirectionalThreadSpawnEdgeStatus::Open => ThreadSpawnEdgeStatus::Open,
        codex_state::DirectionalThreadSpawnEdgeStatus::Closed => ThreadSpawnEdgeStatus::Closed,
    }
}

fn from_state_edge(edge: codex_state::DirectionalThreadSpawnEdge) -> ThreadSpawnEdge {
    ThreadSpawnEdge {
        parent_id: edge.parent_thread_id,
        child_id: edge.child_thread_id,
        status: from_state_status(edge.status),
        created_at: edge.created_at,
    }
}

fn invalid_graph_error(message: &str) -> AgentGraphStoreError {
    AgentGraphStoreError::InvalidRequest {
        message: message.to_string(),
    }
}

fn bounded_error(message: &str) -> AgentGraphStoreError {
    AgentGraphStoreError::InvalidRequest {
        message: message.to_string(),
    }
}

fn internal_error(err: impl std::fmt::Display) -> AgentGraphStoreError {
    AgentGraphStoreError::Internal {
        message: err.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use codex_state::DirectionalThreadSpawnEdgeStatus;
    use codex_utils_absolute_path::test_support::PathExt;
    use pretty_assertions::assert_eq;
    use tempfile::TempDir;

    struct TestRuntime {
        state_db: Arc<StateRuntime>,
        _codex_home: TempDir,
    }

    fn thread_id(suffix: u128) -> ThreadId {
        ThreadId::from_string(&format!("00000000-0000-0000-0000-{suffix:012}"))
            .expect("valid thread id")
    }

    async fn state_runtime() -> TestRuntime {
        let codex_home = TempDir::new().expect("tempdir should be created");
        let state_db = StateRuntime::init(
            codex_state::SqliteConfig::new_for_testing(codex_home.path().abs()),
            "test-provider".to_string(),
        )
        .await
        .expect("state db should initialize");
        TestRuntime {
            state_db,
            _codex_home: codex_home,
        }
    }

    #[tokio::test]
    async fn local_store_upserts_and_lists_direct_children_with_status_filters() {
        let fixture = state_runtime().await;
        let state_db = fixture.state_db;
        let store = LocalAgentGraphStore::new(state_db.clone());
        let parent_thread_id = thread_id(/*suffix*/ 1);
        let first_child_thread_id = thread_id(/*suffix*/ 2);
        let second_child_thread_id = thread_id(/*suffix*/ 3);

        store
            .upsert_thread_spawn_edge(
                parent_thread_id,
                second_child_thread_id,
                ThreadSpawnEdgeStatus::Closed,
            )
            .await
            .expect("closed child edge should insert");
        store
            .upsert_thread_spawn_edge(
                parent_thread_id,
                first_child_thread_id,
                ThreadSpawnEdgeStatus::Open,
            )
            .await
            .expect("open child edge should insert");

        let all_children = store
            .list_thread_spawn_children(parent_thread_id, /*status_filter*/ None)
            .await
            .expect("all children should load");
        assert_eq!(
            all_children,
            vec![first_child_thread_id, second_child_thread_id]
        );

        let open_children = store
            .list_thread_spawn_children(parent_thread_id, Some(ThreadSpawnEdgeStatus::Open))
            .await
            .expect("open children should load");
        let state_open_children = state_db
            .list_thread_spawn_children_with_status(
                parent_thread_id,
                DirectionalThreadSpawnEdgeStatus::Open,
            )
            .await
            .expect("state open children should load");
        assert_eq!(open_children, state_open_children);
        assert_eq!(open_children, vec![first_child_thread_id]);

        let closed_children = store
            .list_thread_spawn_children(parent_thread_id, Some(ThreadSpawnEdgeStatus::Closed))
            .await
            .expect("closed children should load");
        assert_eq!(closed_children, vec![second_child_thread_id]);
    }

    #[tokio::test]
    async fn local_store_updates_edge_status() {
        let fixture = state_runtime().await;
        let state_db = fixture.state_db;
        let store = LocalAgentGraphStore::new(state_db);
        let parent_thread_id = thread_id(/*suffix*/ 10);
        let child_thread_id = thread_id(/*suffix*/ 11);

        store
            .upsert_thread_spawn_edge(
                parent_thread_id,
                child_thread_id,
                ThreadSpawnEdgeStatus::Open,
            )
            .await
            .expect("child edge should insert");
        store
            .set_thread_spawn_edge_status(child_thread_id, ThreadSpawnEdgeStatus::Closed)
            .await
            .expect("child edge should close");

        let open_children = store
            .list_thread_spawn_children(parent_thread_id, Some(ThreadSpawnEdgeStatus::Open))
            .await
            .expect("open children should load");
        assert_eq!(open_children, Vec::<ThreadId>::new());

        let closed_children = store
            .list_thread_spawn_children(parent_thread_id, Some(ThreadSpawnEdgeStatus::Closed))
            .await
            .expect("closed children should load");
        assert_eq!(closed_children, vec![child_thread_id]);
    }

    #[tokio::test]
    async fn local_store_lists_descendants_breadth_first_with_status_filters() {
        let fixture = state_runtime().await;
        let state_db = fixture.state_db;
        let store = LocalAgentGraphStore::new(state_db.clone());
        let root_thread_id = thread_id(/*suffix*/ 20);
        let later_child_thread_id = thread_id(/*suffix*/ 22);
        let earlier_child_thread_id = thread_id(/*suffix*/ 21);
        let closed_grandchild_thread_id = thread_id(/*suffix*/ 23);
        let open_grandchild_thread_id = thread_id(/*suffix*/ 24);
        let closed_child_thread_id = thread_id(/*suffix*/ 25);
        let closed_great_grandchild_thread_id = thread_id(/*suffix*/ 26);

        for (parent_thread_id, child_thread_id, status) in [
            (
                root_thread_id,
                later_child_thread_id,
                ThreadSpawnEdgeStatus::Open,
            ),
            (
                root_thread_id,
                earlier_child_thread_id,
                ThreadSpawnEdgeStatus::Open,
            ),
            (
                earlier_child_thread_id,
                open_grandchild_thread_id,
                ThreadSpawnEdgeStatus::Open,
            ),
            (
                later_child_thread_id,
                closed_grandchild_thread_id,
                ThreadSpawnEdgeStatus::Closed,
            ),
            (
                root_thread_id,
                closed_child_thread_id,
                ThreadSpawnEdgeStatus::Closed,
            ),
            (
                closed_child_thread_id,
                closed_great_grandchild_thread_id,
                ThreadSpawnEdgeStatus::Closed,
            ),
        ] {
            store
                .upsert_thread_spawn_edge(parent_thread_id, child_thread_id, status)
                .await
                .expect("edge should insert");
        }

        let all_descendants = store
            .list_thread_spawn_descendants(root_thread_id, /*status_filter*/ None)
            .await
            .expect("all descendants should load");
        assert_eq!(
            all_descendants,
            vec![
                earlier_child_thread_id,
                later_child_thread_id,
                closed_child_thread_id,
                closed_grandchild_thread_id,
                open_grandchild_thread_id,
                closed_great_grandchild_thread_id,
            ]
        );

        let open_descendants = store
            .list_thread_spawn_descendants(root_thread_id, Some(ThreadSpawnEdgeStatus::Open))
            .await
            .expect("open descendants should load");
        let state_open_descendants = state_db
            .list_thread_spawn_descendants_with_status(
                root_thread_id,
                DirectionalThreadSpawnEdgeStatus::Open,
            )
            .await
            .expect("state open descendants should load");
        assert_eq!(open_descendants, state_open_descendants);
        assert_eq!(
            open_descendants,
            vec![
                earlier_child_thread_id,
                later_child_thread_id,
                open_grandchild_thread_id,
            ]
        );

        let closed_descendants = store
            .list_thread_spawn_descendants(root_thread_id, Some(ThreadSpawnEdgeStatus::Closed))
            .await
            .expect("closed descendants should load");
        assert_eq!(
            closed_descendants,
            vec![closed_child_thread_id, closed_great_grandchild_thread_id]
        );
    }

    #[tokio::test]
    async fn local_store_lists_detailed_edges_with_stable_depth_and_order() {
        let fixture = state_runtime().await;
        let state_db = fixture.state_db;
        let store = LocalAgentGraphStore::new(state_db);
        let root_thread_id = thread_id(/*suffix*/ 30);
        let first_child_thread_id = thread_id(/*suffix*/ 31);
        let second_child_thread_id = thread_id(/*suffix*/ 32);
        let first_grandchild_thread_id = thread_id(/*suffix*/ 33);
        let second_grandchild_thread_id = thread_id(/*suffix*/ 34);

        for (parent_thread_id, child_thread_id, status) in [
            (
                root_thread_id,
                second_child_thread_id,
                ThreadSpawnEdgeStatus::Open,
            ),
            (
                root_thread_id,
                first_child_thread_id,
                ThreadSpawnEdgeStatus::Open,
            ),
            (
                first_child_thread_id,
                first_grandchild_thread_id,
                ThreadSpawnEdgeStatus::Closed,
            ),
            (
                second_child_thread_id,
                second_grandchild_thread_id,
                ThreadSpawnEdgeStatus::Open,
            ),
        ] {
            store
                .upsert_thread_spawn_edge(parent_thread_id, child_thread_id, status)
                .await
                .expect("edge should insert");
        }

        let details = store
            .list_thread_spawn_edge_details(root_thread_id, None)
            .await
            .expect("detailed graph should load");
        assert_eq!(
            details,
            vec![
                ThreadSpawnEdgeDetail {
                    parent_id: root_thread_id,
                    child_id: first_child_thread_id,
                    status: ThreadSpawnEdgeStatus::Open,
                    created_at: None,
                    depth: 1,
                    order: 0,
                },
                ThreadSpawnEdgeDetail {
                    parent_id: root_thread_id,
                    child_id: second_child_thread_id,
                    status: ThreadSpawnEdgeStatus::Open,
                    created_at: None,
                    depth: 1,
                    order: 1,
                },
                ThreadSpawnEdgeDetail {
                    parent_id: first_child_thread_id,
                    child_id: first_grandchild_thread_id,
                    status: ThreadSpawnEdgeStatus::Closed,
                    created_at: None,
                    depth: 2,
                    order: 2,
                },
                ThreadSpawnEdgeDetail {
                    parent_id: second_child_thread_id,
                    child_id: second_grandchild_thread_id,
                    status: ThreadSpawnEdgeStatus::Open,
                    created_at: None,
                    depth: 2,
                    order: 3,
                },
            ]
        );

        let open_details = store
            .list_thread_spawn_edge_details(root_thread_id, Some(ThreadSpawnEdgeStatus::Open))
            .await
            .expect("open detailed graph should load");
        assert_eq!(
            open_details
                .iter()
                .map(|edge| edge.child_id)
                .collect::<Vec<_>>(),
            vec![
                first_child_thread_id,
                second_child_thread_id,
                second_grandchild_thread_id,
            ]
        );
    }

    #[tokio::test]
    async fn local_store_reads_one_edge_and_does_not_fabricate_orphans() {
        let fixture = state_runtime().await;
        let store = LocalAgentGraphStore::new(fixture.state_db);
        let parent_thread_id = thread_id(/*suffix*/ 40);
        let child_thread_id = thread_id(/*suffix*/ 41);

        store
            .upsert_thread_spawn_edge(
                parent_thread_id,
                child_thread_id,
                ThreadSpawnEdgeStatus::Open,
            )
            .await
            .expect("edge should insert");

        assert_eq!(
            store
                .get_thread_spawn_edge(parent_thread_id, child_thread_id)
                .await
                .expect("edge should load"),
            Some(ThreadSpawnEdge {
                parent_id: parent_thread_id,
                child_id: child_thread_id,
                status: ThreadSpawnEdgeStatus::Open,
                created_at: None,
            })
        );
        assert_eq!(
            store
                .get_thread_spawn_edge(thread_id(/*suffix*/ 42), child_thread_id)
                .await
                .expect("missing edge should load as none"),
            None
        );
    }

    #[tokio::test]
    async fn local_store_keeps_closed_edges_closed_on_reopen_attempts() {
        let fixture = state_runtime().await;
        let store = LocalAgentGraphStore::new(fixture.state_db);
        let parent_thread_id = thread_id(/*suffix*/ 50);
        let child_thread_id = thread_id(/*suffix*/ 51);

        store
            .upsert_thread_spawn_edge(
                parent_thread_id,
                child_thread_id,
                ThreadSpawnEdgeStatus::Closed,
            )
            .await
            .expect("closed edge should insert");
        store
            .upsert_thread_spawn_edge(
                parent_thread_id,
                child_thread_id,
                ThreadSpawnEdgeStatus::Open,
            )
            .await
            .expect("reopen attempt should remain harmless");
        store
            .set_thread_spawn_edge_status(child_thread_id, ThreadSpawnEdgeStatus::Open)
            .await
            .expect("reopen attempt should remain harmless");

        assert_eq!(
            store
                .get_thread_spawn_edge(parent_thread_id, child_thread_id)
                .await
                .expect("edge should load")
                .expect("closed edge should remain persisted")
                .status,
            ThreadSpawnEdgeStatus::Closed
        );
    }

    #[tokio::test]
    async fn local_store_rejects_cycles_and_depth_overflow() {
        let fixture = state_runtime().await;
        let state_db = fixture.state_db;
        let store = LocalAgentGraphStore::new(state_db.clone());
        let root_thread_id = thread_id(/*suffix*/ 60);
        let first_child_thread_id = thread_id(/*suffix*/ 61);

        store
            .upsert_thread_spawn_edge(
                root_thread_id,
                first_child_thread_id,
                ThreadSpawnEdgeStatus::Open,
            )
            .await
            .expect("edge should insert");
        store
            .upsert_thread_spawn_edge(
                first_child_thread_id,
                root_thread_id,
                ThreadSpawnEdgeStatus::Open,
            )
            .await
            .expect("cycle fixture should insert");
        let cycle_error = store
            .list_thread_spawn_edge_details(root_thread_id, None)
            .await
            .expect_err("cycle should be rejected");
        assert!(cycle_error.to_string().contains("cycle"));

        let depth_root = thread_id(/*suffix*/ 70);
        let mut parent = depth_root;
        for index in 0..=MAX_EDGE_DEPTH {
            let child = thread_id(71 + u128::from(index));
            store
                .upsert_thread_spawn_edge(parent, child, ThreadSpawnEdgeStatus::Open)
                .await
                .expect("depth fixture edge should insert");
            parent = child;
        }
        let depth_error = store
            .list_thread_spawn_edge_details(depth_root, None)
            .await
            .expect_err("depth overflow should be rejected");
        assert!(depth_error.to_string().contains("depth limit"));
    }
}
