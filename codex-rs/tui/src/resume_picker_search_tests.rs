use super::*;

use codex_app_server_protocol::IndexState;
use codex_protocol::ThreadId;
use insta::assert_snapshot;
use pretty_assertions::assert_eq;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Mutex;

fn recording_loader() -> (PickerLoader, Arc<Mutex<Vec<PageLoadRequest>>>) {
    let requests = Arc::new(Mutex::new(Vec::new()));
    let request_sink = Arc::clone(&requests);
    let loader = Arc::new(move |request| {
        if let PickerLoadRequest::Page(request) = request {
            request_sink.lock().expect("request sink").push(request);
        }
    });
    (loader, requests)
}

fn row(thread_id: ThreadId, preview: &str) -> Row {
    Row {
        path: Some(PathBuf::from(format!("/tmp/{thread_id}.jsonl"))),
        preview: preview.to_string(),
        thread_id: Some(thread_id),
        thread_name: Some(String::from("Named session")),
        created_at: None,
        updated_at: None,
        cwd: None,
        git_branch: None,
    }
}

fn page(
    rows: Vec<Row>,
    snippets: impl IntoIterator<Item = (ThreadId, String)>,
    next_cursor: Option<&str>,
    index_state: IndexState,
    partial: bool,
    from_search: bool,
) -> PickerPage {
    PickerPage {
        rows,
        history_modes: HashMap::new(),
        snippets: snippets.into_iter().collect(),
        next_cursor: next_cursor.map(|cursor| PageCursor::AppServer(cursor.to_string())),
        num_scanned_files: 1,
        reached_scan_cap: false,
        index_state,
        partial,
        from_search,
    }
}

async fn deliver_page(state: &mut PickerState, request: &PageLoadRequest, page: PickerPage) {
    state
        .handle_background_event(BackgroundEvent::Page {
            request_token: request.request_token,
            search_token: request.search_token,
            page: Ok(page),
        })
        .await
        .expect("page event");
}

fn test_state(loader: PickerLoader) -> PickerState {
    PickerState::new(
        FrameRequester::test_dummy(),
        loader,
        ProviderFilter::Any,
        /*show_all*/ true,
        /*filter_cwd*/ None,
        SessionPickerAction::Resume,
    )
}

#[test]
fn empty_query_lists_and_non_empty_query_searches() {
    let (loader, requests) = recording_loader();
    let mut state = test_state(loader);

    state.start_initial_load();
    assert_eq!(requests.lock().expect("request sink")[0].query, None);

    state.set_query(String::from("needle"));
    assert_eq!(
        requests.lock().expect("request sink")[1].query,
        Some(String::from("needle"))
    );
}

#[tokio::test]
async fn remote_search_replaces_provisional_rows_and_renders_snippet() {
    let (loader, requests) = recording_loader();
    let mut state = test_state(loader);
    let provisional_id = ThreadId::new();
    state.all_rows = vec![row(provisional_id, "local metadata needle")];
    state.apply_filter();

    state.set_query(String::from("needle"));
    assert_eq!(state.filtered_rows.len(), 1);
    let request = requests.lock().expect("request sink")[0].clone();
    let global_id = ThreadId::new();
    deliver_page(
        &mut state,
        &request,
        page(
            vec![row(global_id, "does not contain the query")],
            [(global_id, String::from("needle from the global index"))],
            None,
            IndexState::Ready,
            false,
            true,
        ),
    )
    .await;

    assert_eq!(state.all_rows.len(), 1);
    assert_eq!(state.all_rows[0].thread_id, Some(global_id));
    assert_eq!(
        state.display_preview(&state.all_rows[0]),
        "needle from the global index"
    );
    assert!(!state.search_state.is_active());
    let rendered_title = render_session_lines(
        &state.all_rows[0],
        &state,
        /*is_selected*/ false,
        /*is_expanded*/ false,
        /*is_zebra*/ false,
        /*width*/ 80,
    )
    .into_iter()
    .next()
    .expect("rendered search row title")
    .to_string();
    assert_snapshot!(
        rendered_title.as_str(),
        @r"  needle from the global index"
    );
}

#[tokio::test]
async fn remote_search_deduplicates_by_thread_id_across_pages() {
    let (loader, requests) = recording_loader();
    let mut state = test_state(loader);
    state.set_query(String::from("needle"));
    let first_request = requests.lock().expect("request sink")[0].clone();
    let thread_id = ThreadId::new();
    deliver_page(
        &mut state,
        &first_request,
        page(
            vec![row(thread_id, "first")],
            [(thread_id, String::from("first snippet"))],
            Some("next"),
            IndexState::Ready,
            false,
            true,
        ),
    )
    .await;

    state.load_more_if_needed(LoadTrigger::Scroll);
    let second_request = requests.lock().expect("request sink")[1].clone();
    deliver_page(
        &mut state,
        &second_request,
        page(
            vec![row(thread_id, "duplicate")],
            [(thread_id, String::from("duplicate snippet"))],
            None,
            IndexState::Ready,
            false,
            true,
        ),
    )
    .await;

    assert_eq!(state.all_rows.len(), 1);
    assert_eq!(
        state.search_state.snippet(thread_id),
        Some("duplicate snippet")
    );
}

#[tokio::test]
async fn stale_search_page_does_not_replace_new_query() {
    let (loader, requests) = recording_loader();
    let mut state = test_state(loader);
    state.set_query(String::from("old"));
    let old_request = requests.lock().expect("request sink")[0].clone();
    state.set_query(String::from("new"));
    let new_request = requests.lock().expect("request sink")[1].clone();

    deliver_page(
        &mut state,
        &old_request,
        page(
            vec![row(ThreadId::new(), "stale")],
            Vec::<(ThreadId, String)>::new(),
            None,
            IndexState::Ready,
            false,
            true,
        ),
    )
    .await;
    assert!(state.all_rows.is_empty());
    assert_eq!(state.query, "new");
    let stale_render = render_empty_state_line(&state).to_string();
    assert_snapshot!(
        stale_render.as_str(),
        @r"Searching… · index: building · partial"
    );

    let fresh_id = ThreadId::new();
    deliver_page(
        &mut state,
        &new_request,
        page(
            vec![row(fresh_id, "fresh")],
            [(fresh_id, String::from("fresh snippet"))],
            None,
            IndexState::Ready,
            false,
            true,
        ),
    )
    .await;
    assert_eq!(state.all_rows[0].thread_id, Some(fresh_id));
}

#[test]
fn search_empty_states_expose_index_state_and_partial() {
    let (loader, _) = recording_loader();
    let mut state = test_state(loader);
    state.query = String::from("needle");

    state.search_state.reset("needle", Some(1), false);
    let building_render = render_empty_state_line(&state).to_string();
    assert_snapshot!(
        building_render.as_str(),
        @r"Searching… · index: building · partial"
    );

    state.search_state.reset("needle", None, false);
    let ready_render = render_empty_state_line(&state).to_string();
    assert_snapshot!(
        ready_render.as_str(),
        @r"No results for your search · index: ready"
    );

    state
        .search_state
        .set_page_metadata(IndexState::Unavailable, true);
    let unavailable_render = render_empty_state_line(&state).to_string();
    assert_snapshot!(
        unavailable_render.as_str(),
        @r"No results for your search · index: unavailable · partial"
    );
}
