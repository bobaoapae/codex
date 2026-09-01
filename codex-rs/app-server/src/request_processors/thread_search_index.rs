//! App-server integration for the rebuildable workflow search projection.
//!
//! The workflow database owns the FTS5 generation and its live overlay. This
//! module keeps API policy (filters, lineage, hydration and cursors) outside
//! the storage crates. Rollouts remain the source of truth.

#[path = "thread_search_index/filters.rs"]
mod filters;
#[path = "thread_search_index/hydration.rs"]
mod hydration;
#[path = "thread_search_index/query.rs"]
mod query;

use codex_app_server_protocol::IndexState;
use codex_state::SearchFilter;
use codex_state::SearchRequest;
use codex_state::WorkflowStore;
use codex_thread_store::StoredThreadSearchResult;
use codex_thread_store::ThreadStore;

pub(crate) use filters::ThreadFilterOptions;
pub(crate) use filters::classify_threads;
pub(crate) use filters::thread_matches_filters;
pub(crate) use hydration::relation_ids_for_query;
pub(crate) use hydration::relation_ids_for_root;
pub(crate) use query::ThreadSearchQuery;
pub(crate) use query::decode_fallback_cursor;
pub(crate) use query::encode_fallback_cursor;

const SEARCH_PAGE_LIMIT: u32 = 200;
const MAX_INDEX_SCAN_DOCUMENTS: usize = 10_000;

/// State exposed alongside list/search responses.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct IndexStatus {
    pub(crate) state: IndexState,
    pub(crate) partial: bool,
}

/// Distinguishes a malformed/stale client cursor from an unavailable index.
#[derive(Debug)]
pub(crate) enum ThreadIndexError {
    InvalidCursor(String),
    Backend(anyhow::Error),
}

impl std::fmt::Display for ThreadIndexError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidCursor(message) => formatter.write_str(message),
            Self::Backend(error) => write!(formatter, "{error}"),
        }
    }
}

impl std::error::Error for ThreadIndexError {}

/// Return the best available state of the workflow FTS projection.
pub(crate) async fn index_status(workflow: Option<&WorkflowStore>) -> IndexStatus {
    let Some(workflow) = workflow else {
        return unavailable_status();
    };
    let fts_available = workflow.fts5_available().await.unwrap_or(false);
    if !fts_available {
        return unavailable_status();
    }

    let active_generation = workflow.active_search_generation().await.ok().flatten();
    let state = match active_generation {
        Some(_) => IndexState::Ready,
        // The public WorkflowStore API intentionally does not expose the
        // mutable generation rows. FTS exists but no published pointer means
        // the projection is still being built (or needs recovery), so callers
        // must use the bounded fallback and report a partial result.
        None => IndexState::Building,
    };
    IndexStatus {
        partial: !matches!(state, IndexState::Ready),
        state,
    }
}

fn unavailable_status() -> IndexStatus {
    IndexStatus {
        state: IndexState::Unavailable,
        partial: true,
    }
}

/// Search the active generation and live overlay, then hydrate each unique
/// thread through the normal ThreadStore path.
pub(crate) async fn search_index(
    workflow: &WorkflowStore,
    thread_store: &dyn ThreadStore,
    query: ThreadSearchQuery,
    page_size: usize,
    cursor: Option<String>,
) -> Result<IndexedSearchPage, ThreadIndexError> {
    let relation_ids = hydration::relation_ids_for_query(thread_store, &query)
        .await
        .map_err(ThreadIndexError::Backend)?;
    let state_filter = SearchFilter {
        // Archive state is mutable metadata. The FTS generation snapshot must
        // not filter on it; hydration applies the current state below.
        archived: None,
        include_live: true,
        ..SearchFilter::default()
    };
    let (mut raw_cursor, cursor_was_supplied) = query::decode_cursor(cursor.as_deref(), &query)?;
    let mut selected = Vec::with_capacity(page_size);
    let mut seen_threads = std::collections::HashSet::new();
    let mut scanned_documents = 0usize;
    let mut partial = false;

    loop {
        let request = SearchRequest::new(
            query.search_term.clone(),
            state_filter.clone(),
            raw_cursor.clone(),
            SEARCH_PAGE_LIMIT,
        )
        .map_err(ThreadIndexError::Backend)?;
        let page = workflow
            .search_page(&request)
            .await
            .map_err(|error| query::search_error(error, cursor_was_supplied))?;
        if page.documents.is_empty() {
            break;
        }

        let document_count = page.documents.len();
        let page_next_cursor = page.next_cursor.clone();
        let unique_ids = page
            .documents
            .iter()
            .filter_map(|document| {
                (!seen_threads.contains(&document.thread_id)).then_some(document.thread_id.clone())
            })
            .collect::<Vec<_>>();
        let hydrated =
            hydration::hydrate_threads(thread_store, unique_ids, /*include_archived*/ true).await;
        let hydrated_threads = hydrated.values().cloned().collect::<Vec<_>>();
        let classifications = classify_threads(Some(workflow), &hydrated_threads).await;
        let mut stop_after = None;
        let mut stop_after_index = None;
        let mut last_consumed_document = None;

        for (index, document) in page.documents.into_iter().enumerate() {
            scanned_documents = scanned_documents.saturating_add(1);
            last_consumed_document = Some(document.clone());
            if scanned_documents >= MAX_INDEX_SCAN_DOCUMENTS {
                partial = true;
            }
            if scanned_documents > MAX_INDEX_SCAN_DOCUMENTS {
                break;
            }
            if !seen_threads.insert(document.thread_id.clone()) {
                continue;
            }
            let Some(thread) = hydrated.get(&document.thread_id) else {
                continue;
            };
            let Some(classification) = classifications.get(&document.thread_id) else {
                continue;
            };
            if !filters::thread_matches_filters(
                thread,
                classification,
                filters::ThreadFilterOptions {
                    model_providers: query.model_providers.as_deref(),
                    cwd_filters: query.cwd_filters.as_deref(),
                    archived: Some(query.archived),
                    project_id: query.project_id.as_ref(),
                    root_thread_id: query.root_thread_id.as_deref(),
                    source_kinds: query.source_kinds.as_deref(),
                    thread_classes: query.thread_classes.as_deref(),
                    terminal_outcomes: query.terminal_outcomes.as_deref(),
                    relation_ids: relation_ids.as_ref(),
                },
            ) {
                continue;
            }
            selected.push(StoredThreadSearchResult {
                thread: thread.clone(),
                snippet: document.snippet.clone().unwrap_or_default(),
            });
            if selected.len() >= page_size {
                stop_after = Some(document);
                stop_after_index = Some(index);
                break;
            }
        }

        if scanned_documents >= MAX_INDEX_SCAN_DOCUMENTS {
            let next_cursor = last_consumed_document.map(|document| {
                query::encode_cursor(
                    query::state_cursor_for_document(
                        &document,
                        page.generation_id,
                        page.live_epoch,
                        query.search_term.as_str(),
                        &state_filter,
                    ),
                    &query,
                )
            });
            return Ok(IndexedSearchPage {
                items: selected,
                next_cursor,
                partial: true,
            });
        }
        if selected.len() >= page_size {
            let has_more = stop_after_index.is_some_and(|index| index + 1 < document_count)
                || page_next_cursor.is_some();
            let next_cursor = if has_more {
                stop_after.map(|document| {
                    query::encode_cursor(
                        query::state_cursor_for_document(
                            &document,
                            page.generation_id,
                            page.live_epoch,
                            query.search_term.as_str(),
                            &state_filter,
                        ),
                        &query,
                    )
                })
            } else {
                None
            };
            return Ok(IndexedSearchPage {
                items: selected,
                next_cursor,
                partial,
            });
        }
        let Some(next_cursor) = page_next_cursor else {
            break;
        };
        raw_cursor = Some(next_cursor);
    }

    Ok(IndexedSearchPage {
        items: selected,
        next_cursor: None,
        partial,
    })
}

#[derive(Debug)]
pub(crate) struct IndexedSearchPage {
    pub(crate) items: Vec<StoredThreadSearchResult>,
    pub(crate) next_cursor: Option<String>,
    pub(crate) partial: bool,
}
