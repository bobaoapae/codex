//! Request and presentation helpers for the resume picker's remote search.
//!
//! The picker deliberately keeps the list and search request shapes next to
//! each other.  This makes it harder for a new filter to be applied to one
//! path but accidentally omitted from the other path.

use std::collections::HashMap;
use std::path::Path;
use std::path::PathBuf;

use crate::app_server_session::AppServerSession;
use codex_app_server_protocol::ClientRequest;
use codex_app_server_protocol::IndexState;
use codex_app_server_protocol::RequestId;
use codex_app_server_protocol::SortDirection;
use codex_app_server_protocol::ThreadClass;
use codex_app_server_protocol::ThreadListCwdFilter;
use codex_app_server_protocol::ThreadListParams;
use codex_app_server_protocol::ThreadSearchParams;
use codex_app_server_protocol::ThreadSearchResponse;
use codex_app_server_protocol::ThreadSearchSortKey;
use codex_app_server_protocol::ThreadSortKey;
use codex_app_server_protocol::ThreadSourceKind;
use uuid::Uuid;

/// Filters shared by `thread/list` and `thread/search` in the resume picker.
///
/// Keeping the filters as one value is important: the list is used before a
/// query is entered, while search is used as soon as a query is non-empty.
/// Both views must describe the same set of resumable threads.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct PickerSearchFilters {
    cwd: Option<PathBuf>,
    archived: bool,
    model_providers: Option<Vec<String>>,
    source_kinds: Vec<ThreadSourceKind>,
    thread_classes: Option<Vec<ThreadClass>>,
}

impl PickerSearchFilters {
    pub(super) fn for_picker(
        cwd: Option<&Path>,
        status: super::SessionStatus,
        provider_filter: &super::ProviderFilter,
        include_non_interactive: bool,
    ) -> Self {
        let model_providers = match provider_filter {
            super::ProviderFilter::Any => None,
            super::ProviderFilter::MatchDefault(default_provider) => {
                Some(vec![default_provider.clone()])
            }
        };
        Self::new(
            cwd,
            status == super::SessionStatus::Archived,
            model_providers,
            crate::resume_source_kinds(include_non_interactive),
            picker_thread_classes(include_non_interactive),
        )
    }

    pub(super) fn new(
        cwd: Option<&Path>,
        archived: bool,
        model_providers: Option<Vec<String>>,
        source_kinds: Vec<ThreadSourceKind>,
        thread_classes: Option<Vec<ThreadClass>>,
    ) -> Self {
        Self {
            cwd: cwd.map(Path::to_path_buf),
            archived,
            model_providers,
            source_kinds,
            thread_classes,
        }
    }

    pub(super) fn search_params(
        &self,
        cursor: Option<String>,
        query: &str,
        sort_key: ThreadSortKey,
    ) -> ThreadSearchParams {
        ThreadSearchParams {
            cursor,
            limit: Some(super::PAGE_SIZE as u32),
            sort_key: Some(search_sort_key(sort_key)),
            sort_direction: Some(SortDirection::Desc),
            model_providers: self.model_providers.clone(),
            cwd: self
                .cwd
                .as_deref()
                .map(|cwd| ThreadListCwdFilter::One(cwd.to_string_lossy().into_owned())),
            project_id: None,
            root_thread_id: None,
            ancestor_thread_id: None,
            source_kinds: Some(self.source_kinds.clone()),
            archived: Some(self.archived),
            search_term: query.trim().to_string(),
            thread_classes: self.thread_classes.clone(),
            terminal_outcomes: None,
        }
    }
}

pub(super) fn picker_thread_classes(include_non_interactive: bool) -> Option<Vec<ThreadClass>> {
    include_non_interactive.then_some(vec![
        ThreadClass::Interactive,
        ThreadClass::SubAgent,
        ThreadClass::TransientJob,
        ThreadClass::LegacyExec,
    ])
}

pub(super) async fn load_page(
    app_server: &mut AppServerSession,
    params: ThreadSearchParams,
) -> std::io::Result<ThreadSearchResponse> {
    app_server
        .request_handle()
        .request_typed::<ThreadSearchResponse>(ClientRequest::ThreadSearch {
            request_id: RequestId::String(format!("resume-picker-search-{}", Uuid::new_v4())),
            params,
        })
        .await
        .map_err(std::io::Error::other)
}

pub(super) async fn load_list_page(
    app_server: &mut AppServerSession,
    params: ThreadListParams,
) -> std::io::Result<super::PickerPage> {
    let response = app_server
        .thread_list(params)
        .await
        .map_err(std::io::Error::other)?;
    let num_scanned_files = response.data.len();
    let (rows, history_modes): (Vec<_>, HashMap<_, _>) = response
        .data
        .into_iter()
        .filter_map(|thread| {
            let history_mode = thread.history_mode;
            let row = super::row_from_app_server_thread(thread)?;
            let thread_id = row.thread_id?;
            Some((row, (thread_id, history_mode)))
        })
        .unzip();

    Ok(super::PickerPage {
        rows,
        history_modes,
        snippets: HashMap::new(),
        next_cursor: response.next_cursor.map(super::PageCursor::AppServer),
        num_scanned_files,
        reached_scan_cap: false,
        index_state: response.index_state,
        partial: response.partial,
        from_search: false,
    })
}

pub(super) async fn load_search_page(
    app_server: &mut AppServerSession,
    params: ThreadSearchParams,
) -> std::io::Result<super::PickerPage> {
    let response = load_page(app_server, params).await?;
    let num_scanned_files = response.data.len();
    let mut snippets = HashMap::with_capacity(response.data.len());
    let mut rows = Vec::with_capacity(response.data.len());
    let mut history_modes = HashMap::with_capacity(response.data.len());
    for result in response.data {
        let thread = result.thread;
        let history_mode = thread.history_mode;
        let Some(row) = super::row_from_app_server_thread(thread) else {
            continue;
        };
        let Some(thread_id) = row.thread_id else {
            continue;
        };
        snippets.insert(thread_id, result.snippet);
        history_modes.insert(thread_id, history_mode);
        rows.push(row);
    }

    Ok(super::PickerPage {
        rows,
        history_modes,
        snippets,
        next_cursor: response.next_cursor.map(super::PageCursor::AppServer),
        num_scanned_files,
        reached_scan_cap: false,
        index_state: response.index_state,
        partial: response.partial,
        from_search: true,
    })
}

fn search_sort_key(sort_key: ThreadSortKey) -> ThreadSearchSortKey {
    match sort_key {
        ThreadSortKey::CreatedAt => ThreadSearchSortKey::CreatedAt,
        ThreadSortKey::UpdatedAt => ThreadSearchSortKey::UpdatedAt,
        ThreadSortKey::RecencyAt | ThreadSortKey::SectionPosition => ThreadSearchSortKey::RecencyAt,
    }
}

/// Human-readable status used by the picker when an index is not immediately
/// usable, or when the server explicitly reports that a page is partial.
pub(super) fn index_status_label(index_state: IndexState, partial: bool) -> String {
    let state = match index_state {
        IndexState::Building => "building",
        IndexState::Ready => "ready",
        IndexState::Unavailable => "unavailable",
        IndexState::Recoverable => "recoverable",
    };
    if partial {
        format!("index: {state} · partial")
    } else {
        format!("index: {state}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use codex_app_server_protocol::ThreadListCwdFilter;

    #[test]
    fn list_and_search_keep_the_same_filters() {
        let filters = PickerSearchFilters::new(
            Some(Path::new("/workspace")),
            true,
            Some(vec![String::from("openai")]),
            vec![ThreadSourceKind::Cli, ThreadSourceKind::Exec],
            Some(vec![ThreadClass::Interactive, ThreadClass::LegacyExec]),
        );
        let search = filters.search_params(None, "needle", ThreadSortKey::UpdatedAt);

        assert_eq!(search.model_providers, Some(vec![String::from("openai")]));
        assert_eq!(
            search.cwd,
            Some(ThreadListCwdFilter::One(String::from("/workspace")))
        );
        assert_eq!(
            search.source_kinds,
            Some(vec![ThreadSourceKind::Cli, ThreadSourceKind::Exec])
        );
        assert_eq!(search.archived, Some(true));
        assert_eq!(
            search.thread_classes,
            Some(vec![ThreadClass::Interactive, ThreadClass::LegacyExec])
        );
    }

    #[test]
    fn index_status_includes_partial_marker() {
        assert_eq!(
            index_status_label(IndexState::Ready, true),
            "index: ready · partial"
        );
        assert_eq!(
            index_status_label(IndexState::Unavailable, false),
            "index: unavailable"
        );
    }
}
