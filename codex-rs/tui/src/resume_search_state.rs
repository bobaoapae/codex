//! Mutable search state and picker hooks for remote resume search.

use std::collections::HashMap;
use std::collections::HashSet;

use codex_app_server_protocol::IndexState;
use codex_protocol::ThreadId;

#[derive(Clone, Copy, Debug, Default)]
pub(super) enum SearchState {
    #[default]
    Idle,
    Active {
        token: usize,
    },
}

impl SearchState {
    fn active_token(&self) -> Option<usize> {
        match self {
            Self::Idle => None,
            Self::Active { token } => Some(*token),
        }
    }

    fn is_active(&self) -> bool {
        self.active_token().is_some()
    }
}

/// State owned by the remote resume search lifecycle.
///
/// The picker may render metadata rows immediately while a search request is
/// in flight. Once the first search page arrives, those provisional rows are
/// replaced by the server's global result set and subsequent pages are
/// deduplicated by thread id.
pub(super) struct SearchSession {
    state: SearchState,
    pub(super) snippets: HashMap<ThreadId, String>,
    pub(super) seen_thread_ids: HashSet<ThreadId>,
    pub(super) results_started: bool,
    pub(super) index_state: IndexState,
    pub(super) partial: bool,
    replace_on_next_page: bool,
}

impl Default for SearchSession {
    fn default() -> Self {
        Self {
            state: SearchState::default(),
            snippets: HashMap::new(),
            seen_thread_ids: HashSet::new(),
            results_started: false,
            index_state: IndexState::Ready,
            partial: false,
            replace_on_next_page: false,
        }
    }
}

impl SearchSession {
    pub(super) fn reset(&mut self, query: &str, token: Option<usize>, preserve_loaded_rows: bool) {
        self.state = match token {
            Some(token) if !query.trim().is_empty() => SearchState::Active { token },
            _ => SearchState::Idle,
        };
        self.snippets.clear();
        self.seen_thread_ids.clear();
        self.results_started = false;
        self.index_state = if token.is_some() {
            IndexState::Building
        } else {
            IndexState::Ready
        };
        self.partial = token.is_some();
        self.replace_on_next_page = preserve_loaded_rows;
    }

    pub(super) fn active_token(&self) -> Option<usize> {
        self.state.active_token()
    }

    pub(super) fn is_active(&self) -> bool {
        self.state.is_active()
    }

    pub(super) fn finish(&mut self) {
        self.state = SearchState::Idle;
    }

    pub(super) fn activate(&mut self, token: usize) {
        self.state = SearchState::Active { token };
    }

    pub(super) fn start_remote_page(&mut self, index_state: IndexState, partial: bool) -> bool {
        self.index_state = index_state;
        self.partial = partial;
        if self.results_started {
            return false;
        }
        self.results_started = true;
        true
    }

    pub(super) fn take_replace_on_next_page(&mut self) -> bool {
        std::mem::take(&mut self.replace_on_next_page)
    }

    pub(super) fn has_replace_on_next_page(&self) -> bool {
        self.replace_on_next_page
    }

    pub(super) fn set_page_metadata(&mut self, index_state: IndexState, partial: bool) {
        self.index_state = index_state;
        self.partial = partial;
    }

    pub(super) fn accept_thread(&mut self, thread_id: ThreadId) -> bool {
        self.seen_thread_ids.insert(thread_id)
    }

    pub(super) fn accept_snippet(&mut self, thread_id: ThreadId, snippet: String) {
        if !snippet.trim().is_empty() {
            self.snippets.insert(thread_id, snippet);
        }
    }

    pub(super) fn should_filter_locally(&self, query: &str) -> bool {
        !query.trim().is_empty() && !self.results_started
    }

    pub(super) fn snippet(&self, thread_id: ThreadId) -> Option<&str> {
        self.snippets
            .get(&thread_id)
            .map(String::as_str)
            .filter(|snippet| !snippet.trim().is_empty())
    }
}

impl super::PickerState {
    pub(super) fn start_initial_load_with(&mut self, preserve_loaded_rows: bool) {
        self.relative_time_reference = Some(chrono::Utc::now());
        self.reset_pagination();
        if !preserve_loaded_rows {
            self.all_rows.clear();
        }
        self.filtered_rows.clear();
        self.thread_history_modes.clear();
        self.seen_rows.clear();
        self.selected = 0;
        self.pending_page_down_target = None;
        self.frozen_footer_percent = None;

        let search_token = (!self.query.trim().is_empty()).then(|| self.allocate_search_token());
        self.search_state
            .reset(&self.query, search_token, preserve_loaded_rows);

        if preserve_loaded_rows {
            // While the remote index is answering, retain only metadata matches
            // from the rows already loaded in this picker. The first remote
            // page replaces these provisional rows with the global result set.
            self.apply_filter();
        }

        let request_token = self.allocate_request_token();
        let mode = self.initial_page_mode;
        self.pagination
            .start_load(request_token, search_token, mode);
        self.request_frame();

        (self.picker_loader)(super::PickerLoadRequest::Page(super::PageLoadRequest {
            cursor: None,
            request_token,
            search_token,
            query: (!self.query.trim().is_empty()).then(|| self.query.trim().to_string()),
            mode,
            cwd_filter: self.active_cwd_filter(),
            status: self.status,
            provider_filter: self.provider_filter.clone(),
            sort_key: self.sort_key,
        }));
    }

    pub(super) fn ingest_page(&mut self, page: super::PickerPage) {
        let super::PickerPage {
            rows,
            history_modes,
            snippets,
            next_cursor,
            num_scanned_files,
            reached_scan_cap,
            index_state,
            partial,
            from_search,
        } = page;
        self.pagination
            .complete_page(next_cursor, num_scanned_files, reached_scan_cap);
        self.thread_history_modes.extend(history_modes);

        self.search_state.set_page_metadata(index_state, partial);
        let remote_search_started =
            from_search && self.search_state.start_remote_page(index_state, partial);
        let replace_provisional_rows =
            self.search_state.take_replace_on_next_page() || remote_search_started;
        if replace_provisional_rows {
            self.all_rows.clear();
            self.filtered_rows.clear();
            self.seen_rows.clear();
        }

        for row in rows {
            if from_search {
                let Some(thread_id) = row.thread_id else {
                    continue;
                };
                if self.search_state.accept_thread(thread_id) {
                    self.all_rows.push(row);
                }
            } else if let Some(seen_key) = row.seen_key() {
                if self.seen_rows.insert(seen_key) {
                    self.all_rows.push(row);
                }
            } else {
                self.all_rows.push(row);
            }
        }

        for (thread_id, snippet) in snippets {
            self.search_state.accept_snippet(thread_id, snippet);
        }

        self.apply_filter();
    }

    pub(super) fn apply_filter(&mut self) {
        let base_iter = self
            .all_rows
            .iter()
            .filter(|row| self.row_matches_filter(row));
        if self.search_state.should_filter_locally(&self.query) {
            let q = self.query.trim().to_lowercase();
            self.filtered_rows = base_iter
                .filter(|row| row.matches_query(&q))
                .cloned()
                .collect();
        } else {
            self.filtered_rows = base_iter.cloned().collect();
        }
        if self.selected >= self.filtered_rows.len() {
            self.selected = self.filtered_rows.len().saturating_sub(1);
        }
        if self.filtered_rows.is_empty() {
            self.scroll_top = 0;
        }
        self.ensure_selected_visible();
        self.request_frame();
    }

    pub(super) fn row_matches_filter(&self, row: &super::Row) -> bool {
        if self.filter_mode == super::SessionFilterMode::All {
            return true;
        }
        let Some(filter_cwd) = self.local_filter_cwd.as_ref() else {
            return true;
        };
        let Some(row_cwd) = row.cwd.as_ref() else {
            return false;
        };
        super::paths_match(row_cwd, filter_cwd)
    }

    pub(super) fn display_preview<'a>(&'a self, row: &'a super::Row) -> &'a str {
        if self.search_state.results_started
            && !self.query.trim().is_empty()
            && let Some(thread_id) = row.thread_id
            && let Some(snippet) = self.search_state.snippet(thread_id)
        {
            return snippet;
        }
        row.display_preview()
    }

    pub(super) fn set_query(&mut self, new_query: String) {
        if self.query == new_query {
            return;
        }
        self.query = new_query;
        let preserve_loaded_rows = !self.query.trim().is_empty() && !self.all_rows.is_empty();
        self.start_initial_load_with(preserve_loaded_rows);
    }

    pub(super) fn clear_query_preserving_selection(&mut self) {
        let selected_key = self
            .filtered_rows
            .get(self.selected)
            .and_then(super::Row::seen_key);
        self.query.clear();
        self.start_initial_load_with(/*preserve_loaded_rows*/ true);
        if let Some(selected_key) = selected_key
            && let Some(index) = self
                .filtered_rows
                .iter()
                .position(|row| row.seen_key().as_ref() == Some(&selected_key))
        {
            self.selected = index;
            self.ensure_selected_visible();
            self.request_frame();
        }
    }

    pub(super) fn continue_search_if_needed(&mut self) {
        let Some(token) = self.search_state.active_token() else {
            return;
        };
        if !self.filtered_rows.is_empty() {
            self.search_state.finish();
            return;
        }
        if self.pagination.reached_scan_cap || self.pagination.next_cursor.is_none() {
            self.search_state.finish();
            return;
        }
        self.load_more_if_needed(super::LoadTrigger::Search { token });
    }

    pub(super) fn continue_search_if_token_matches(&mut self, completed_token: Option<usize>) {
        let Some(active) = self.search_state.active_token() else {
            return;
        };
        if let Some(token) = completed_token
            && token != active
        {
            return;
        }
        self.continue_search_if_needed();
    }
}
