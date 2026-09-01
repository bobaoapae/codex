use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use codex_app_server_protocol::SortDirection;
use codex_app_server_protocol::TerminalOutcome;
use codex_app_server_protocol::ThreadClass;
use codex_app_server_protocol::ThreadSearchSortKey;
use codex_app_server_protocol::ThreadSourceKind;
use codex_state::SearchCursor;
use codex_state::SearchDocument;
use codex_state::SearchFilter;
use serde::Deserialize;
use serde::Serialize;
use std::path::PathBuf;

use super::ThreadIndexError;

const MAX_CURSOR_BYTES: usize = 65_536;

/// Normalized API filters shared by indexed and fallback searches.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct ThreadSearchQuery {
    pub(crate) search_term: String,
    pub(crate) sort_key: ThreadSearchSortKey,
    pub(crate) sort_direction: SortDirection,
    pub(crate) model_providers: Option<Vec<String>>,
    pub(crate) cwd_filters: Option<Vec<PathBuf>>,
    pub(crate) project_id: Option<Option<String>>,
    pub(crate) root_thread_id: Option<String>,
    pub(crate) ancestor_thread_id: Option<String>,
    pub(crate) source_kinds: Option<Vec<ThreadSourceKind>>,
    pub(crate) archived: bool,
    pub(crate) thread_classes: Option<Vec<ThreadClass>>,
    pub(crate) terminal_outcomes: Option<Vec<TerminalOutcome>>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct IndexCursor {
    query: ThreadSearchQuery,
    workflow_cursor: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct FallbackCursor {
    query: ThreadSearchQuery,
    store_cursor: String,
}

pub(crate) fn decode_cursor(
    encoded: Option<&str>,
    query: &ThreadSearchQuery,
) -> Result<(Option<String>, bool), ThreadIndexError> {
    let Some(encoded) = encoded else {
        return Ok((None, false));
    };
    if encoded.len() > MAX_CURSOR_BYTES {
        return Err(invalid_cursor());
    }
    let bytes = URL_SAFE_NO_PAD
        .decode(encoded)
        .map_err(|_| invalid_cursor())?;
    let cursor = serde_json::from_slice::<IndexCursor>(&bytes).map_err(|_| invalid_cursor())?;
    if cursor.query != *query {
        return Err(ThreadIndexError::InvalidCursor(
            "stale or incompatible thread/search cursor".to_string(),
        ));
    }
    if cursor.workflow_cursor.is_empty() {
        return Err(invalid_cursor());
    }
    Ok((Some(cursor.workflow_cursor), true))
}

pub(crate) fn encode_cursor(workflow_cursor: String, query: &ThreadSearchQuery) -> String {
    let cursor = IndexCursor {
        query: query.clone(),
        workflow_cursor,
    };
    serde_json::to_vec(&cursor)
        .map(|bytes| URL_SAFE_NO_PAD.encode(bytes))
        .unwrap_or_default()
}

pub(crate) fn decode_fallback_cursor(
    encoded: Option<&str>,
    query: &ThreadSearchQuery,
) -> Result<Option<String>, ThreadIndexError> {
    let Some(encoded) = encoded else {
        return Ok(None);
    };
    if encoded.len() > MAX_CURSOR_BYTES {
        return Err(invalid_cursor());
    }
    let bytes = URL_SAFE_NO_PAD
        .decode(encoded)
        .map_err(|_| invalid_cursor())?;
    let cursor = serde_json::from_slice::<FallbackCursor>(&bytes).map_err(|_| invalid_cursor())?;
    if cursor.query != *query || cursor.store_cursor.is_empty() {
        return Err(ThreadIndexError::InvalidCursor(
            "stale or incompatible thread/search cursor".to_string(),
        ));
    }
    Ok(Some(cursor.store_cursor))
}

pub(crate) fn encode_fallback_cursor(store_cursor: String, query: &ThreadSearchQuery) -> String {
    let cursor = FallbackCursor {
        query: query.clone(),
        store_cursor,
    };
    serde_json::to_vec(&cursor)
        .map(|bytes| URL_SAFE_NO_PAD.encode(bytes))
        .unwrap_or_default()
}

pub(crate) fn state_cursor_for_document(
    document: &SearchDocument,
    generation_id: Option<i64>,
    live_epoch: i64,
    query: &str,
    filter: &SearchFilter,
) -> String {
    SearchCursor {
        generation_id,
        live_epoch,
        query: query.to_string(),
        filter: filter.clone(),
        rank: document.rank.unwrap_or(f64::MAX),
        is_live: document.is_live,
        document_id: document.document_id,
    }
    .encode()
    .unwrap_or_default()
}

pub(crate) fn search_error(error: anyhow::Error, cursor_was_supplied: bool) -> ThreadIndexError {
    let message = error.to_string();
    if cursor_was_supplied
        && (message.contains("cursor")
            || message.contains("generation")
            || message.contains("epoch"))
    {
        ThreadIndexError::InvalidCursor("stale or incompatible thread/search cursor".to_string())
    } else {
        ThreadIndexError::Backend(error)
    }
}

fn invalid_cursor() -> ThreadIndexError {
    ThreadIndexError::InvalidCursor("invalid thread/search cursor".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn query() -> ThreadSearchQuery {
        ThreadSearchQuery {
            search_term: "literal".to_string(),
            sort_key: ThreadSearchSortKey::CreatedAt,
            sort_direction: SortDirection::Desc,
            model_providers: None,
            cwd_filters: None,
            project_id: None,
            root_thread_id: None,
            ancestor_thread_id: None,
            source_kinds: None,
            archived: false,
            thread_classes: None,
            terminal_outcomes: None,
        }
    }

    #[test]
    fn cursor_binding_rejects_changed_filters() {
        let archive_query = query();
        let encoded = encode_cursor("raw".to_string(), &archive_query);
        let mut changed = archive_query;
        changed.search_term = "other".to_string();
        assert!(matches!(
            decode_cursor(Some(encoded.as_str()), &changed),
            Err(ThreadIndexError::InvalidCursor(_))
        ));

        let query = query();
        let encoded = encode_cursor("raw".to_string(), &query);
        let mut changed = query;
        changed.archived = true;
        assert!(matches!(
            decode_cursor(Some(encoded.as_str()), &changed),
            Err(ThreadIndexError::InvalidCursor(_))
        ));
    }
}
