// FORK: tests for the `api.ts` port — `normalize` over real-shaped captures of
// `/backend-api/conversation/<id>`, the fingerprint, and `ChatGptApi` against a
// canned `PageEval`.
use super::*;
use pretty_assertions::assert_eq;
use serde_json::json;
use std::collections::VecDeque;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;

fn fixture(json: &str) -> RawConversation {
    serde_json::from_str(json).expect("fixture deserializes")
}

fn conv_in_progress() -> RawConversation {
    fixture(include_str!("../fixtures/conv_in_progress.json"))
}
fn conv_finished() -> RawConversation {
    fixture(include_str!("../fixtures/conv_finished.json"))
}
fn conv_thoughts() -> RawConversation {
    fixture(include_str!("../fixtures/conv_thoughts.json"))
}
fn conv_image_assets() -> RawConversation {
    fixture(include_str!("../fixtures/conv_image_assets.json"))
}
fn conv_api_tool() -> RawConversation {
    fixture(include_str!("../fixtures/conv_api_tool.json"))
}
fn conv_stopped_old_in_progress() -> RawConversation {
    fixture(include_str!(
        "../fixtures/conv_stopped_old_in_progress.json"
    ))
}

fn roles(conv: &Conversation) -> Vec<&str> {
    conv.turns.iter().map(|t| t.role.as_str()).collect()
}

// ---------------------------------------------------------------------------
// normalize
// ---------------------------------------------------------------------------

#[test]
fn in_progress_reply_is_generating_and_keeps_turn_order() {
    let conv = normalize(&conv_in_progress());
    assert_eq!(conv.id, "11111111-aaaa-4bbb-8ccc-000000000001");
    assert_eq!(conv.title, "Streaming reply");
    assert_eq!(conv.model.as_deref(), Some("gpt-5-2-thinking"));
    assert_eq!(
        conv.current_node.as_deref(),
        Some("11111111-0000-4000-8000-000000000004")
    );
    assert_eq!(
        roles(&conv),
        vec!["user", "assistant-thoughts", "assistant"]
    );
    assert!(conv.any_in_progress);
    assert!(conv.is_generating);
    assert_eq!(conv.async_status, Some(1));

    let user = &conv.turns[0];
    assert_eq!(user.text, "Explique o que é um mutex.");
    assert_eq!(user.message_id, "11111111-0000-4000-8000-000000000002");
    assert_eq!(user.id, user.message_id);

    let reply = &conv.turns[2];
    assert_eq!(reply.status, "in_progress");
    assert_eq!(reply.end_turn, None);
    assert_eq!(reply.recipient.as_deref(), Some("all"));
    assert_eq!(reply.model_slug.as_deref(), Some("gpt-5-2-thinking"));
    assert_eq!(
        reply.parent_id.as_deref(),
        Some("11111111-0000-4000-8000-000000000003")
    );
    assert_eq!(reply.create_time, Some(1756230010.75));
    assert!(reply.text.starts_with("Um mutex é"));

    assert_eq!(conv.last_user_turn_index(), Some(0));
    assert_eq!(conv.reply_turns().len(), 2);
    assert!(conv.api_tool_requests.is_empty());
}

#[test]
fn finished_conversation_is_idle_and_skips_system_and_editable_context() {
    let conv = normalize(&conv_finished());
    // The hidden system message and the `user_editable_context` (custom
    // instructions, role=user) never become turns.
    assert_eq!(roles(&conv), vec!["user", "assistant", "user", "assistant"]);
    assert!(!conv.any_in_progress);
    assert!(!conv.is_generating);
    assert_eq!(conv.async_status, None);
    assert!(!conv.is_archived);
    assert_eq!(conv.update_time, Some(1756240120.5));

    assert_eq!(conv.turns[0].text, "Olá!");
    assert_eq!(conv.turns[1].text, "Olá! Como posso ajudar?");
    assert_eq!(conv.turns[1].end_turn, Some(true));
    assert_eq!(conv.turns[1].status, "finished_successfully");
    assert_eq!(conv.turns[3].end_turn, Some(true));

    // Uploaded files arrive on the user turn via `metadata.attachments`.
    assert_eq!(
        conv.turns[2].assets,
        vec![Asset {
            file_id: "file-Z9Y8X7W6V5U4T3S2R1Q0".to_string(),
            kind: AssetKind::File,
            name: Some("notas.txt".to_string()),
            width: None,
            height: None,
            size_bytes: Some(2048),
        }]
    );
    assert_eq!(conv.last_user_turn_index(), Some(2));
    assert_eq!(conv.reply_turns().len(), 1);
}

#[test]
fn thoughts_before_text_become_reasoning_then_text() {
    let conv = normalize(&conv_thoughts());
    // The `reasoning_recap` message (no parts, no text) is dropped.
    assert_eq!(
        roles(&conv),
        vec!["user", "assistant-thoughts", "assistant"]
    );

    let thoughts = &conv.turns[1];
    assert_eq!(thoughts.content_type, "thoughts");
    assert_eq!(thoughts.end_turn, None);
    assert_eq!(thoughts.message_id, "33333333-0000-4000-8000-000000000002");
    // `[t.summary, t.content].filter(Boolean).join("\n")` joined by "\n\n":
    // an empty `content` is filtered out, not rendered as a blank line.
    assert_eq!(
        thoughts.text,
        "Listando os primos\n\nContando\n2, 3, 5, 7, 11, 13, 17, 19, 23, 29 — são dez."
    );
    assert_eq!(
        thoughts.thoughts,
        vec![
            Thought {
                summary: Some("Listando os primos".to_string()),
                content: Some(String::new()),
            },
            Thought {
                summary: Some("Contando".to_string()),
                content: Some("2, 3, 5, 7, 11, 13, 17, 19, 23, 29 — são dez.".to_string()),
            },
        ]
    );

    let text = &conv.turns[2];
    assert_eq!(text.role, "assistant");
    assert_eq!(text.text, "Existem **10** números primos abaixo de 30.");
    assert_eq!(text.end_turn, Some(true));
    assert!(text.thoughts.is_empty());
    assert!(!conv.any_in_progress);
}

#[test]
fn thoughts_are_dropped_when_not_requested() {
    let conv = normalize_with(
        &conv_thoughts(),
        NormalizeOptions {
            include_thoughts: false,
        },
    );
    assert_eq!(roles(&conv), vec!["user", "assistant"]);
}

#[test]
fn image_assets_come_from_pointers_and_attachments() {
    let conv = normalize(&conv_image_assets());
    // The assistant message addressed to the image tool (recipient != all, no
    // assets) is dropped; the tool message survives because it carries an asset.
    assert_eq!(roles(&conv), vec!["user", "tool", "assistant"]);
    assert!(!conv.any_in_progress);
    assert_eq!(conv.async_status, Some(0));

    let user = &conv.turns[0];
    assert_eq!(user.content_type, "multimodal_text");
    assert_eq!(user.text, "Faça uma versão em aquarela desta foto.");
    // Pointer part first (`file-service://` stripped), then the attachment copy.
    assert_eq!(
        user.assets,
        vec![
            Asset {
                file_id: "file-Up1oAdEd1MaGe0000000001".to_string(),
                kind: AssetKind::Image,
                name: None,
                width: Some(640),
                height: Some(480),
                size_bytes: Some(48213),
            },
            Asset {
                file_id: "file-Up1oAdEd1MaGe0000000001".to_string(),
                kind: AssetKind::Image,
                name: Some("foto.png".to_string()),
                width: None,
                height: None,
                size_bytes: Some(48213),
            },
        ]
    );

    let tool = &conv.turns[1];
    assert_eq!(tool.role, "tool");
    assert_eq!(tool.text, "");
    assert_eq!(tool.end_turn, None);
    assert_eq!(
        tool.assets,
        vec![Asset {
            file_id: "file_00000000gen0000000000000000000001".to_string(),
            kind: AssetKind::Image,
            name: None,
            width: Some(1024),
            height: Some(1024),
            size_bytes: Some(1532988),
        }]
    );

    assert_eq!(conv.turns[2].text, "Aqui está a versão em aquarela.");
    assert_eq!(conv.turns[2].end_turn, Some(true));
}

#[test]
fn api_tool_requests_pair_results_by_parent_id() {
    let conv = normalize(&conv_api_tool());
    // Connector traffic is not user-facing: only the user turn remains.
    assert_eq!(roles(&conv), vec!["user"]);
    assert!(!conv.any_in_progress);
    assert_eq!(
        conv.api_tool_requests,
        vec![
            ApiToolRequest {
                message_id: "55555555-0000-4000-8000-000000000002".to_string(),
                request_id: Some("wfr_01a009".to_string()),
                recipient: "api_tool.call_tool".to_string(),
                has_result: true,
            },
            ApiToolRequest {
                message_id: "55555555-0000-4000-8000-000000000004".to_string(),
                request_id: Some("wfr_01a010".to_string()),
                recipient: "api_tool.call_tool".to_string(),
                has_result: false,
            },
        ]
    );
    assert!(!conv.api_tool_requests.iter().all(|r| r.has_result));
}

/// FORK (C5, verified live): a custom-connector result never shows up as a
/// `tool` node in the mapping, so a request that the chain has moved past
/// counts as answered; only the newest request with nothing after it is
/// still pending.
#[test]
fn api_tool_request_followed_by_a_later_message_counts_as_answered() {
    let mut raw = conv_api_tool();
    // Re-point the existing result at a different parent: request 1 has no
    // matching `tool` node any more, but request 2 comes after it.
    let result = raw
        .mapping
        .get_mut("55555555-0000-4000-8000-000000000003")
        .and_then(|n| n.message.as_mut())
        .expect("result node");
    result.metadata.parent_id = Some("55555555-0000-4000-8000-000000000001".to_string());
    let conv = normalize(&raw);
    assert_eq!(conv.api_tool_requests.len(), 2);
    assert!(conv.api_tool_requests[0].has_result);
    assert!(!conv.api_tool_requests[1].has_result);
}

#[test]
fn old_in_progress_before_last_user_turn_does_not_count_as_generating() {
    let conv = normalize(&conv_stopped_old_in_progress());
    assert_eq!(roles(&conv), vec!["user", "assistant", "user", "assistant"]);
    // The stuck node is still visible as a turn...
    assert_eq!(conv.turns[1].status, "in_progress");
    assert_eq!(conv.turns[1].end_turn, None);
    // ...but only nodes at/after the LAST user message decide `any_in_progress`.
    assert!(!conv.any_in_progress);
    assert!(!conv.is_generating);
    assert_eq!(conv.turns[3].end_turn, Some(true));
    assert_eq!(conv.reply_turns().len(), 1);
}

#[test]
fn walk_follows_parents_from_current_node_and_ignores_other_branches() {
    let mut raw = conv_finished();
    // A regenerate leaves a sibling branch; only the current one is linear.
    raw.mapping.insert(
        "sibling".to_string(),
        RawNode {
            id: "sibling".to_string(),
            parent: Some("22222222-0000-4000-8000-000000000005".to_string()),
            children: vec![],
            message: Some(RawMessage {
                id: "sibling".to_string(),
                author: RawAuthor {
                    role: "assistant".to_string(),
                    name: None,
                },
                content: RawContent {
                    content_type: "text".to_string(),
                    parts: Some(vec![json!("Outra tentativa")]),
                    ..RawContent::default()
                },
                status: "in_progress".to_string(),
                ..RawMessage::default()
            }),
        },
    );
    let conv = normalize(&raw);
    assert!(conv.turns.iter().all(|t| t.message_id != "sibling"));
    assert!(!conv.any_in_progress);

    // Pointing current_node at the sibling switches branches.
    raw.current_node = Some("sibling".to_string());
    let conv = normalize(&raw);
    assert_eq!(
        conv.turns.last().map(|t| t.text.as_str()),
        Some("Outra tentativa")
    );
    assert!(conv.any_in_progress);
}

#[test]
fn walk_stops_on_cycles_and_missing_nodes() {
    let mut raw = conv_thoughts();
    // Cycle: root points back at the leaf.
    raw.mapping
        .get_mut("client-created-root")
        .expect("root")
        .parent = Some("33333333-0000-4000-8000-000000000004".to_string());
    let conv = normalize(&raw);
    assert_eq!(
        roles(&conv),
        vec!["user", "assistant-thoughts", "assistant"]
    );

    // Missing current node: nothing to walk.
    raw.current_node = Some("nope".to_string());
    assert!(normalize(&raw).turns.is_empty());
    raw.current_node = None;
    let conv = normalize(&raw);
    assert!(conv.turns.is_empty());
    assert!(!conv.any_in_progress);
}

#[test]
fn empty_in_progress_placeholder_is_kept_as_a_turn() {
    // `if (!text && assets.length === 0 && m.status !== "in_progress") continue;`
    // — an in_progress message with no text yet is the reply placeholder.
    let mut raw = conv_in_progress();
    let leaf = raw
        .mapping
        .get_mut("11111111-0000-4000-8000-000000000004")
        .and_then(|n| n.message.as_mut())
        .expect("leaf");
    leaf.content.parts = Some(vec![json!("")]);
    let conv = normalize(&raw);
    assert_eq!(conv.turns.last().map(|t| t.text.as_str()), Some(""));
    assert_eq!(
        conv.turns.last().map(|t| t.status.as_str()),
        Some("in_progress")
    );

    leaf_status(&mut raw, "finished_successfully");
    let conv = normalize(&raw);
    assert_eq!(roles(&conv), vec!["user", "assistant-thoughts"]);
}

fn leaf_status(raw: &mut RawConversation, status: &str) {
    let leaf = raw
        .mapping
        .get_mut("11111111-0000-4000-8000-000000000004")
        .and_then(|n| n.message.as_mut())
        .expect("leaf");
    leaf.status = status.to_string();
}

#[test]
fn asset_from_pointer_strips_schemes_and_detects_images() {
    let sediment = json!({
        "content_type": "image_asset_pointer",
        "asset_pointer": "sediment://file_abc",
        "size_bytes": 10, "width": 4, "height": 3
    });
    assert_eq!(
        asset_from_pointer(&sediment),
        Some(Asset {
            file_id: "file_abc".to_string(),
            kind: AssetKind::Image,
            name: None,
            width: Some(4),
            height: Some(3),
            size_bytes: Some(10),
        })
    );
    let file = json!({ "asset_pointer": "file-service://file-xyz", "size_bytes": 99 });
    assert_eq!(
        asset_from_pointer(&file),
        Some(Asset {
            file_id: "file-xyz".to_string(),
            kind: AssetKind::File,
            name: None,
            width: None,
            height: None,
            size_bytes: Some(99),
        })
    );
    // `typeof part.width === "number"` alone makes it an image.
    let by_width = json!({ "asset_pointer": "file-service://file-w", "width": 1 });
    assert_eq!(
        asset_from_pointer(&by_width).map(|a| a.kind),
        Some(AssetKind::Image)
    );
    // No scheme: kept as is. Non-string pointer: not an asset.
    let bare = json!({ "asset_pointer": "file-bare" });
    assert_eq!(
        asset_from_pointer(&bare).map(|a| a.file_id),
        Some("file-bare".to_string())
    );
    assert_eq!(asset_from_pointer(&json!({ "asset_pointer": 5 })), None);
    assert_eq!(asset_from_pointer(&json!({ "text": "x" })), None);
    assert_eq!(strip_pointer_scheme("https://x/y"), "x/y");
    assert_eq!(strip_pointer_scheme("a1://x"), "a1://x");
}

#[test]
fn raw_types_tolerate_nulls_and_unknown_fields() {
    let raw: RawConversation = serde_json::from_value(json!({
        "title": null,
        "mapping": {
            "n1": { "id": "n1", "parent": null, "children": null, "message": {
                "id": "m1", "author": { "role": "user", "name": null },
                "content": { "content_type": "text", "parts": null, "thoughts": null },
                "status": "finished_successfully", "end_turn": null,
                "metadata": null, "recipient": null, "channel": null, "weird": [1, 2]
            }},
            "n2": { "id": "n2", "parent": "n1", "children": [], "message": null }
        },
        "current_node": "n2",
        "async_status": "weird",
        "is_archived": null,
        "unknown_top_level": { "x": 1 }
    }))
    .expect("tolerant");
    assert_eq!(raw.title, "");
    assert_eq!(raw.conversation_id(), None);
    let m = raw.mapping["n1"].message.as_ref().expect("message");
    assert_eq!(m.content.parts, None);
    assert!(m.metadata.attachments.is_empty());
    let conv = normalize(&raw);
    assert_eq!(conv.async_status, None);
    assert!(!conv.is_archived);
    // No parts array and no text → empty text → dropped (finished, no assets).
    assert!(conv.turns.is_empty());
}

// ---------------------------------------------------------------------------
// fingerprint
// ---------------------------------------------------------------------------

#[test]
fn fingerprint_is_stable_for_identical_input_and_changes_when_text_grows() {
    let a = normalize(&conv_in_progress());
    let b = normalize(&conv_in_progress());
    assert_eq!(fingerprint(&a), fingerprint(&b));

    let mut grown = conv_in_progress();
    let leaf = grown
        .mapping
        .get_mut("11111111-0000-4000-8000-000000000004")
        .and_then(|n| n.message.as_mut())
        .expect("leaf");
    leaf.content.parts = Some(vec![json!(
        "Um mutex é um objeto de sincronização que garante que apenas uma thread acesse"
    )]);
    let grown = normalize(&grown);
    assert_ne!(fingerprint(&a), fingerprint(&grown));

    // A status flip alone also moves the fingerprint.
    let mut finished = conv_in_progress();
    leaf_status(&mut finished, "finished_successfully");
    assert_ne!(fingerprint(&a), fingerprint(&normalize(&finished)));

    // Different conversations differ.
    assert_ne!(fingerprint(&a), fingerprint(&normalize(&conv_finished())));
}

#[test]
fn fingerprint_changes_when_assets_or_thoughts_change() {
    let base = normalize(&conv_image_assets());
    let mut without_gen = base.clone();
    without_gen.turns[1].assets.clear();
    assert_ne!(fingerprint(&base), fingerprint(&without_gen));

    let base = normalize(&conv_thoughts());
    let mut more = base.clone();
    more.turns[1].thoughts.push(Thought {
        summary: Some("Mais".to_string()),
        content: None,
    });
    assert_ne!(fingerprint(&base), fingerprint(&more));
}

// ---------------------------------------------------------------------------
// ChatGptApi over a fake PageEval
// ---------------------------------------------------------------------------

/// Returns canned eval results in order and records every expression.
struct FakePageEval {
    responses: Mutex<VecDeque<DriverResult<Value>>>,
    calls: Mutex<Vec<(TabId, String, u64)>>,
    evals: AtomicUsize,
}

impl FakePageEval {
    fn new(responses: Vec<DriverResult<Value>>) -> Self {
        Self {
            responses: Mutex::new(responses.into()),
            calls: Mutex::new(Vec::new()),
            evals: AtomicUsize::new(0),
        }
    }

    fn expressions(&self) -> Vec<String> {
        self.calls
            .lock()
            .expect("lock")
            .iter()
            .map(|(_, expr, _)| expr.clone())
            .collect()
    }
}

impl PageEval for FakePageEval {
    fn eval<'a>(
        &'a self,
        tab_id: TabId,
        expression: String,
        timeout_ms: u64,
    ) -> BoxFuture<'a, DriverResult<Value>> {
        self.evals.fetch_add(1, Ordering::SeqCst);
        self.calls
            .lock()
            .expect("lock")
            .push((tab_id, expression, timeout_ms));
        let next = self
            .responses
            .lock()
            .expect("lock")
            .pop_front()
            .unwrap_or_else(|| Err(DriverError::other("FakePageEval: no more canned responses")));
        Box::pin(async move { next })
    }
}

fn http(status: u16, json: Value) -> DriverResult<Value> {
    Ok(json!({ "status": status, "json": json, "text": null }))
}

fn http_text(status: u16, text: &str) -> DriverResult<Value> {
    Ok(json!({ "status": status, "json": null, "text": text }))
}

fn no_backoff() -> Vec<Duration> {
    vec![Duration::ZERO; RATE_LIMIT_BACKOFF.len()]
}

#[tokio::test]
async fn get_conversation_decodes_the_page_response() {
    let raw: Value =
        serde_json::from_str(include_str!("../fixtures/conv_finished.json")).expect("json");
    let eval = FakePageEval::new(vec![http(200, raw)]);
    let api = ChatGptApi::new(&eval, 7, "");
    let conv = api
        .get_conversation("22222222-aaaa-4bbb-8ccc-000000000002")
        .await
        .expect("ok");
    assert_eq!(
        conv.conversation_id(),
        Some("22222222-aaaa-4bbb-8ccc-000000000002")
    );
    assert_eq!(conv.mapping.len(), 7);
    assert_eq!(normalize(&conv).turns.len(), 4);

    let calls = eval.calls.lock().expect("lock");
    assert_eq!(calls.len(), 1);
    let (tab, expr, timeout) = &calls[0];
    assert_eq!(*tab, 7);
    assert_eq!(*timeout, API_EVAL_TIMEOUT_MS);
    assert!(expr.starts_with("() =>"));
    assert!(expr.contains(
        "const PATH = \"/backend-api/conversation/22222222-aaaa-4bbb-8ccc-000000000002\";"
    ));
    assert!(expr.contains("const METHOD = \"GET\";"));
    assert!(expr.contains("const BODY = null;"));
}

#[tokio::test]
async fn read_conversation_normalizes() {
    let raw: Value =
        serde_json::from_str(include_str!("../fixtures/conv_in_progress.json")).expect("json");
    let eval = FakePageEval::new(vec![http(200, raw)]);
    let api = ChatGptApi::new(&eval, 1, "");
    let conv = api.read_conversation("x").await.expect("ok");
    assert!(conv.is_generating);
}

#[tokio::test]
async fn base_url_is_prefixed_to_every_path() {
    let eval = FakePageEval::new(vec![http(200, json!({ "mapping": {} }))]);
    let api = ChatGptApi::new(&eval, 1, "https://chatgpt.com/");
    assert_eq!(api.base_url(), "https://chatgpt.com");
    assert_eq!(api.tab_id(), 1);
    api.get_conversation("abc").await.expect("ok");
    assert!(
        eval.expressions()[0]
            .contains("const PATH = \"https://chatgpt.com/backend-api/conversation/abc\";")
    );
}

#[tokio::test]
async fn a_string_eval_result_is_decoded_as_json() {
    // A PageEval that skipped the second decode still works.
    let eval = FakePageEval::new(vec![Ok(json!(
        "{\"status\":200,\"json\":{\"mapping\":{},\"title\":\"t\"},\"text\":null}"
    ))]);
    let api = ChatGptApi::new(&eval, 1, "");
    let conv = api.get_conversation("abc").await.expect("ok");
    assert_eq!(conv.title, "t");

    let eval = FakePageEval::new(vec![Ok(json!("not json"))]);
    let api = ChatGptApi::new(&eval, 1, "");
    let err = api.get_conversation("abc").await.expect_err("err");
    assert_eq!(err.kind, DriverErrorKind::Other);
    assert!(err.message.contains("non-JSON"));

    let eval = FakePageEval::new(vec![Ok(json!([1, 2]))]);
    let api = ChatGptApi::new(&eval, 1, "");
    let err = api.get_conversation("abc").await.expect_err("err");
    assert_eq!(err.kind, DriverErrorKind::Other);
}

#[tokio::test]
async fn http_404_maps_to_conversation_not_found() {
    let eval = FakePageEval::new(vec![http(
        404,
        json!({ "detail": "Conversation not found" }),
    )]);
    let api = ChatGptApi::new(&eval, 1, "");
    let err = api.get_conversation("missing").await.expect_err("404");
    assert_eq!(err.kind, DriverErrorKind::ConversationNotFound);
    assert_eq!(
        err.message,
        "ChatGPT API GET /backend-api/conversation/missing → HTTP 404: {\"detail\":\"Conversation not found\"}"
    );
    assert_eq!(err.phase, None);
    assert_eq!(err.message_landed, None);
}

#[tokio::test]
async fn http_429_retries_with_backoff_then_maps_to_rate_limited() {
    let eval = FakePageEval::new(vec![
        http_text(429, "Too many requests"),
        http_text(429, "Too many requests"),
        http_text(429, "Too many requests"),
        http_text(429, "Too many requests"),
    ]);
    let api = ChatGptApi::new(&eval, 1, "").with_backoff(no_backoff());
    let err = api.get_conversation("abc").await.expect_err("429");
    assert_eq!(err.kind, DriverErrorKind::RateLimited);
    // 1 initial + 3 backoff retries, then give up.
    assert_eq!(eval.evals.load(Ordering::SeqCst), 4);
    assert!(err.message.ends_with("→ HTTP 429: \"Too many requests\""));
}

#[tokio::test]
async fn http_429_then_200_succeeds() {
    let eval = FakePageEval::new(vec![
        http_text(429, "slow down"),
        http(200, json!({ "title": "ok", "mapping": {} })),
    ]);
    let api = ChatGptApi::new(&eval, 1, "").with_backoff(no_backoff());
    let conv = api.get_conversation("abc").await.expect("ok after retry");
    assert_eq!(conv.title, "ok");
    assert_eq!(eval.evals.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn other_http_statuses_map_to_driver_error_kinds() {
    for (status, kind) in [
        (401, DriverErrorKind::LoginRequired),
        (403, DriverErrorKind::LoginRequired),
        (500, DriverErrorKind::Upstream),
        (502, DriverErrorKind::Upstream),
        (503, DriverErrorKind::Upstream),
        (400, DriverErrorKind::Other),
        (418, DriverErrorKind::Other),
    ] {
        let eval = FakePageEval::new(vec![http_text(status, "nope")]);
        let api = ChatGptApi::new(&eval, 1, "").with_backoff(no_backoff());
        let err = api.get_conversation("abc").await.expect_err("error");
        assert_eq!(err.kind, kind, "status {status}");
        assert!(err.message.contains(&format!("HTTP {status}")));
    }
}

#[tokio::test]
async fn page_level_errors_are_reported_and_login_is_detected() {
    let eval = FakePageEval::new(vec![Ok(json!({
        "status": 0,
        "error": "Error: not logged in: /api/auth/session returned no accessToken"
    }))]);
    let api = ChatGptApi::new(&eval, 1, "");
    let err = api.get_conversation("abc").await.expect_err("login");
    assert_eq!(err.kind, DriverErrorKind::LoginRequired);
    assert!(
        err.message
            .starts_with("ChatGPT API /backend-api/conversation/abc failed in page:")
    );

    let eval = FakePageEval::new(vec![Ok(
        json!({ "status": 0, "error": "TypeError: Failed to fetch" }),
    )]);
    let api = ChatGptApi::new(&eval, 1, "");
    let err = api.get_conversation("abc").await.expect_err("fetch");
    assert_eq!(err.kind, DriverErrorKind::Other);

    // Eval failures pass through untouched.
    let eval = FakePageEval::new(vec![Err(DriverError::timeout("eval timed out"))]);
    let api = ChatGptApi::new(&eval, 1, "");
    let err = api.get_conversation("abc").await.expect_err("timeout");
    assert_eq!(err.kind, DriverErrorKind::Timeout);
}

#[tokio::test]
async fn list_conversations_requests_updated_order_and_decodes_items() {
    let eval = FakePageEval::new(vec![http(
        200,
        json!({
            "items": [
                { "id": "c1", "title": "Primeira", "update_time": "2026-08-26T10:00:00.000000+00:00", "is_archived": false },
                { "id": "c2", "title": null, "update_time": "2026-08-25T10:00:00.000000+00:00" }
            ],
            "total": 2, "limit": 20, "offset": 40, "has_missing_conversations": false
        }),
    )]);
    let api = ChatGptApi::new(&eval, 1, "");
    let list = api.list_conversations(40, 20).await.expect("ok");
    assert_eq!(list.total, 2);
    assert_eq!(list.items.len(), 2);
    assert_eq!(list.items[0].id, "c1");
    assert_eq!(list.items[0].is_archived, Some(false));
    assert_eq!(list.items[1].title, "");
    assert!(
        eval.expressions()[0].contains(
            "const PATH = \"/backend-api/conversations?offset=40&limit=20&order=updated\";"
        )
    );
}

#[tokio::test]
async fn patch_conversation_sends_the_body_and_ignores_the_reply() {
    let eval = FakePageEval::new(vec![
        http(200, json!({ "success": true })),
        http(200, json!({ "success": true })),
        http(200, json!({ "success": true })),
    ]);
    let api = ChatGptApi::new(&eval, 1, "");
    api.patch_conversation("abc", json!({ "title": "Novo \"título\"" }))
        .await
        .expect("title");
    api.patch_conversation("abc", json!({ "is_archived": true }))
        .await
        .expect("archive");
    api.patch_conversation("abc", json!({ "is_visible": false }))
        .await
        .expect("hide");
    let exprs = eval.expressions();
    assert!(exprs[0].contains("const METHOD = \"PATCH\";"));
    assert!(exprs[0].contains("const BODY = {\"title\":\"Novo \\\"título\\\"\"};"));
    assert!(exprs[1].contains("const BODY = {\"is_archived\":true};"));
    assert!(exprs[2].contains("const BODY = {\"is_visible\":false};"));
    assert!(
        exprs
            .iter()
            .all(|e| e.contains("const PATH = \"/backend-api/conversation/abc\";"))
    );
}

#[tokio::test]
async fn models_are_cached_per_base_url_for_five_minutes() {
    let base = "https://models-cache-test.invalid";
    clear_models_cache(base);
    let payload = json!({
        "models": [
            { "slug": "gpt-5-2", "title": "GPT-5.2", "max_tokens": 128000 },
            { "slug": "gpt-5-2-thinking", "title": "GPT-5.2 Thinking" },
            { "slug": "gpt-5-2-pro", "title": "GPT-5.2 Pro" }
        ],
        "default_model_slug": "gpt-5-2",
        "categories": []
    });
    let eval = FakePageEval::new(vec![http(200, payload)]);
    let api = ChatGptApi::new(&eval, 1, base);
    let first = api.models().await.expect("first");
    assert_eq!(first.default_slug.as_deref(), Some("gpt-5-2"));
    assert_eq!(
        first.models,
        vec![
            ModelEntry {
                slug: "gpt-5-2".to_string(),
                title: "GPT-5.2".to_string(),
            },
            ModelEntry {
                slug: "gpt-5-2-thinking".to_string(),
                title: "GPT-5.2 Thinking".to_string(),
            },
            ModelEntry {
                slug: "gpt-5-2-pro".to_string(),
                title: "GPT-5.2 Pro".to_string(),
            },
        ]
    );
    assert!(eval.expressions()[0].contains(
        "const PATH = \"https://models-cache-test.invalid/backend-api/models?history_and_training_disabled=false\";"
    ));

    // Cache hit: no second eval, even through a fresh ChatGptApi value.
    let api2 = ChatGptApi::new(&eval, 2, base);
    let second = api2.models().await.expect("cached");
    assert_eq!(second, first);
    assert_eq!(eval.evals.load(Ordering::SeqCst), 1);

    // A different origin is a different cache entry.
    let other = "https://models-cache-test-2.invalid";
    clear_models_cache(other);
    let eval_other = FakePageEval::new(vec![http(200, json!({ "models": [] }))]);
    let api_other = ChatGptApi::new(&eval_other, 1, other);
    let info = api_other.models().await.expect("other origin");
    assert_eq!(info, ModelsInfo::default());
    assert_eq!(eval_other.evals.load(Ordering::SeqCst), 1);

    // Clearing forces a refetch.
    clear_models_cache(base);
    let eval3 = FakePageEval::new(vec![http_text(500, "boom")]);
    let api3 = ChatGptApi::new(&eval3, 1, base);
    let err = api3.models().await.expect_err("refetched and failed");
    assert_eq!(err.kind, DriverErrorKind::Upstream);
    clear_models_cache(base);
    clear_models_cache(other);
}

/// FORK: the process-wide limiter spaces backend calls.
#[tokio::test]
async fn the_backend_limiter_spaces_calls_by_the_minimum_interval() {
    let limiter = BackendLimiter::new(Duration::from_millis(40));
    let started = Instant::now();
    limiter.acquire().await;
    limiter.acquire().await;
    limiter.acquire().await;
    assert!(
        started.elapsed() >= Duration::from_millis(80),
        "three calls must span two intervals, took {:?}",
        started.elapsed()
    );
}
