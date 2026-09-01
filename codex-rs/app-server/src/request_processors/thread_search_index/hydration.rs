use codex_protocol::ThreadId;
use codex_thread_store::ListThreadsParams;
use codex_thread_store::ReadThreadParams;
use codex_thread_store::SortDirection as StoreSortDirection;
use codex_thread_store::StoredThread;
use codex_thread_store::ThreadRelationFilter;
use codex_thread_store::ThreadSortKey;
use codex_thread_store::ThreadStore;
use std::collections::HashMap;
use std::collections::HashSet;

use super::query::ThreadSearchQuery;

const SEARCH_PAGE_LIMIT: usize = 200;
const MAX_RELATION_IDS: usize = 100_000;

pub(crate) async fn hydrate_threads(
    thread_store: &dyn ThreadStore,
    thread_ids: Vec<String>,
    include_archived: bool,
) -> HashMap<String, StoredThread> {
    let mut hydrated = HashMap::with_capacity(thread_ids.len());
    for thread_id in thread_ids {
        let Ok(thread_id) = ThreadId::from_string(&thread_id) else {
            continue;
        };
        let Ok(thread) = thread_store
            .read_thread(ReadThreadParams {
                thread_id,
                include_archived,
                include_history: false,
            })
            .await
        else {
            continue;
        };
        hydrated.insert(thread.thread_id.to_string(), thread);
    }
    hydrated
}

pub(crate) async fn relation_ids_for_query(
    thread_store: &dyn ThreadStore,
    query: &ThreadSearchQuery,
) -> anyhow::Result<Option<HashSet<String>>> {
    if query.ancestor_thread_id.is_none() && query.root_thread_id.is_none() {
        return Ok(None);
    }
    let ids = if let Some(ancestor_thread_id) = query.ancestor_thread_id.as_deref() {
        let mut ids =
            relation_ids_for_root_inner(thread_store, ancestor_thread_id, query.archived, false)
                .await?;
        if let Some(root_thread_id) = query.root_thread_id.as_deref() {
            let root_ids =
                relation_ids_for_root_inner(thread_store, root_thread_id, query.archived, true)
                    .await?;
            ids.retain(|thread_id| root_ids.contains(thread_id));
        }
        ids
    } else {
        let Some(root_thread_id) = query.root_thread_id.as_deref() else {
            return Ok(None);
        };
        relation_ids_for_root_inner(thread_store, root_thread_id, query.archived, true).await?
    };
    Ok(Some(ids))
}

pub(crate) async fn relation_ids_for_root(
    thread_store: &dyn ThreadStore,
    relation_id: &str,
    archived: bool,
) -> anyhow::Result<HashSet<String>> {
    relation_ids_for_root_inner(thread_store, relation_id, archived, true).await
}

async fn relation_ids_for_root_inner(
    thread_store: &dyn ThreadStore,
    relation_id: &str,
    archived: bool,
    include_root: bool,
) -> anyhow::Result<HashSet<String>> {
    let relation_thread_id = ThreadId::from_string(relation_id)?;
    let relation = ThreadRelationFilter::DescendantsOf(relation_thread_id);
    let mut cursor = None;
    let mut ids = HashSet::new();
    loop {
        let page = thread_store
            .list_threads(ListThreadsParams {
                page_size: SEARCH_PAGE_LIMIT,
                cursor,
                sort_key: ThreadSortKey::UpdatedAt,
                sort_direction: StoreSortDirection::Desc,
                allowed_sources: Vec::new(),
                model_providers: None,
                cwd_filters: None,
                section: None,
                project_id: None,
                archived,
                search_term: None,
                relation_filter: Some(relation),
                use_state_db_only: true,
            })
            .await
            .map_err(|error| anyhow::anyhow!(error.to_string()))?;
        ids.extend(
            page.items
                .into_iter()
                .map(|thread| thread.thread_id.to_string()),
        );
        if ids.len() > MAX_RELATION_IDS {
            anyhow::bail!("thread relation filter exceeds {MAX_RELATION_IDS} entries");
        }
        let Some(next_cursor) = page.next_cursor else {
            break;
        };
        cursor = Some(next_cursor);
    }
    if include_root {
        ids.insert(relation_thread_id.to_string());
    }
    Ok(ids)
}
