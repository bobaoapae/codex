// FORK: tests for the `page-scripts.ts` port.
use super::*;
use pretty_assertions::assert_eq;
use serde_json::json;

/// A tricky parameter: quotes, backslashes, newlines, a closing script tag and
/// a placeholder-looking token. Its JSON form must land in the script verbatim.
const NASTY: &str = "say \"hi\"\\n\nline2\t</script>\u{2028}@@TEXT@@ 'single' `tick` ${x}";

fn all_scripts() -> Vec<(&'static str, String)> {
    vec![
        ("wait_ready", wait_ready(5000)),
        ("composer_state", composer_state()),
        ("set_composer_text", set_composer_text(NASTY)),
        ("attachment_tiles", attachment_tiles()),
        ("dismiss_upload_dialog", dismiss_upload_dialog()),
        ("clear_composer", clear_composer()),
        ("click_send", click_send(12_000)),
        ("click_stop", click_stop()),
        (
            "api_call",
            api_call(
                "/backend-api/conversation/abc",
                "PATCH",
                Some(&json!({"title": NASTY})),
            ),
        ),
        (
            "api_call_with_headers",
            api_call_with_headers(
                "/backend-api/aip/connectors",
                "POST",
                None,
                &serde_json::Map::from_iter([("X-Test".to_string(), json!(NASTY))]),
            ),
        ),
        ("stage_download", stage_download(NASTY)),
        (
            "read_download_chunk",
            read_download_chunk(NASTY, 4, 8, true),
        ),
        ("dom_turns", dom_turns(12)),
        ("menu_discover", menu_discover()),
        ("menu_select", menu_select(MenuKind::Level, NASTY)),
    ]
}

#[test]
fn every_script_is_a_function_expression() {
    for (name, src) in all_scripts() {
        assert!(
            src.starts_with("() =>"),
            "{name} must be a `() =>` function expression, got: {}",
            &src[..src.len().min(40)]
        );
    }
}

#[test]
fn no_script_uses_async_or_await() {
    for (name, src) in all_scripts() {
        // The injected runner returns {} for async functions; promise chains only.
        // `NASTY` never contains these words, so a hit is always the JS body.
        assert!(!src.contains("async "), "{name} contains `async `");
        assert!(!src.contains("await "), "{name} contains `await `");
    }
}

#[test]
fn every_script_resolves_a_json_string() {
    for (name, src) in all_scripts() {
        assert!(
            src.contains("JSON.stringify("),
            "{name} must resolve JSON.stringify(...)"
        );
    }
}

#[test]
fn wait_ready_interpolates_the_timeout() {
    let src = wait_ready(1234);
    assert!(src.contains("1234"));
    assert!(src.contains("Date.now() - t0 > 1234)"));
    assert!(src.contains("}, 250);"), "polling interval must stay 250ms");
}

#[test]
fn string_parameters_are_inserted_as_json_literals() {
    let literal = serde_json::to_string(NASTY).expect("serializes");
    // Sanity on the literal itself: quotes/backslashes/newlines escaped, no raw newline.
    assert!(literal.starts_with('"') && literal.ends_with('"'));
    assert!(!literal.contains('\n'));
    assert!(literal.contains("\\\"hi\\\""));
    assert!(literal.contains("\\\\n"));
    assert!(literal.contains("\\n"));
    assert!(literal.contains("</script>"));

    let src = set_composer_text(NASTY);
    assert!(src.contains(&format!("const TEXT = {literal};")));
    // The raw text must not appear unescaped, and the placeholder-looking token
    // inside the value must not have been expanded or altered.
    assert!(!src.contains("const TEXT = say"));
    assert_eq!(
        src.matches("@@TEXT@@").count(),
        1,
        "only the literal's own copy remains"
    );

    let src = stage_download(NASTY);
    assert!(src.contains(&format!("const FID = {literal};")));

    let src = read_download_chunk(NASTY, 4, 8, true);
    assert!(src.contains(&format!("const b64 = cache[{literal}];")));
    assert!(src.contains("b64.slice(4, 12);"));
    assert!(src.contains("if (true) delete cache["));

    let src = menu_select(MenuKind::Model, NASTY);
    assert!(src.contains(&format!("new RegExp({literal}, 'i')")));
    assert!(src.contains("new RegExp(\"^Modelo|^Model\", 'i')"));
    let src = menu_select(MenuKind::Level, "^Alto$|^High$");
    assert!(src.contains("new RegExp(\"N[íi]vel de racioc[íi]nio|Reasoning\", 'i')"));
}

#[test]
fn api_call_interpolates_path_method_and_body() {
    let body = json!({"title": NASTY, "n": 1, "nested": {"a": [1, 2, null]}});
    let src = api_call("/backend-api/conversation/abc", "PATCH", Some(&body));
    assert!(src.contains("const PATH = \"/backend-api/conversation/abc\";"));
    assert!(src.contains("const METHOD = \"PATCH\";"));
    let body_literal = serde_json::to_string(&body).expect("serializes");
    assert!(src.contains(&format!("const BODY = {body_literal};")));
    assert!(src.contains("headers: { Authorization: 'Bearer ' + tok }"));
    assert!(!src.contains("const EXTRA"));
    assert!(src.contains("window.__cgptmcpTok"));
    assert!(
        src.contains("exp: now + 600000"),
        "token cache must stay 10 minutes"
    );

    let src = api_call("/backend-api/models", "GET", None);
    assert!(src.contains("const BODY = null;"));
}

#[test]
fn api_call_with_headers_merges_extra_headers_over_the_bearer_token() {
    let extra = serde_json::Map::from_iter([("Authorization".to_string(), json!("Bearer other"))]);
    let src = api_call_with_headers("/x", "GET", None, &extra);
    assert!(src.contains("const EXTRA = {\"Authorization\":\"Bearer other\"};"));
    assert!(src.contains("headers: Object.assign({ Authorization: 'Bearer ' + tok }, EXTRA)"));
    assert!(src.contains("window.__cgptmcpTok"));

    let empty = serde_json::Map::new();
    let src = api_call_with_headers("/x", "GET", None, &empty);
    assert!(src.contains("const EXTRA = {};"));
}

#[test]
fn placeholder_filling_never_rescans_inserted_values() {
    let out = fill(
        "a @@X@@ b @@Y@@ c",
        &[("X", "[@@Y@@]".to_string()), ("Y", "y".to_string())],
    );
    assert_eq!(out, "a [@@Y@@] b y c");
    // Unknown tokens and stray `@@` are left alone.
    assert_eq!(fill("@@NOPE@@ @@ end", &[]), "@@NOPE@@ @@ end");
}

#[test]
fn selectors_and_timings_survive_the_port() {
    let cs = composer_state();
    assert!(cs.contains("#prompt-textarea"));
    assert!(cs.contains("[data-testid=\"send-button\"]"));
    assert!(cs.contains("[data-testid=\"stop-button\"]"));
    assert!(cs.contains("group/file-tile"));
    assert!(cs.contains(":not([data-testid=\"composer-plus-btn\"]):not(#composer-plus-btn)"));

    let send = click_send(12_000);
    assert!(send.contains("/\\/c\\/([0-9a-f-]{20,})/i"));
    assert!(send.contains("Date.now() - t0 > 12000"));
    assert!(send.contains("}, 300);"));

    let stop = click_stop();
    assert!(stop.contains("Date.now() - t0 > 8000"));

    let dialog = dismiss_upload_dialog();
    assert!(dialog.contains(".replace(/\\s+/g, ' ')"));

    let set = set_composer_text("x");
    assert!(set.contains("new ClipboardEvent('paste'"));
    assert!(set.contains("setTimeout(res, 300)"));
    assert!(set.contains("setTimeout(r2, 150)"));
    assert!(set.contains("document.execCommand('insertText', false, TEXT)"));

    let menu = menu_discover();
    assert!(menu.contains("/N[íi]vel de racioc[íi]nio|Reasoning/i"));
    assert!(menu.contains("/^Modelo|^Model/i"));
    assert!(menu.contains("}, 4000)"));
    assert!(menu.contains("[role=\"menuitemradio\"]"));

    let dl = stage_download("file_1");
    assert!(dl.contains("'/backend-api/files/' + encodeURIComponent(FID) + '/download'"));
    assert!(dl.contains("window.__cgptmcpDl"));
}

#[test]
fn composer_state_decodes_the_script_result_shape() {
    let value = json!({
        "hasComposer": true,
        "url": "https://chatgpt.com/c/abc",
        "modelLabel": "Pro",
        "sendVisible": false,
        "sendEnabled": false,
        "generating": true,
        "attachments": 2,
        "text": "draft"
    });
    let state: ComposerState = serde_json::from_value(value).expect("decodes");
    assert_eq!(
        state,
        ComposerState {
            has_composer: true,
            url: "https://chatgpt.com/c/abc".to_string(),
            model_label: Some("Pro".to_string()),
            send_visible: false,
            send_enabled: false,
            generating: true,
            attachments: 2,
            text: Some("draft".to_string()),
        }
    );
    // Missing fields default (tolerant of page drift).
    let state: ComposerState = serde_json::from_value(json!({})).expect("decodes");
    assert_eq!(state, ComposerState::default());
}

/// FORK: the DOM progress reader is synchronous (hidden tabs throttle timers)
/// and never touches the backend.
#[test]
fn dom_progress_is_a_synchronous_function_expression_without_fetch() {
    let script = dom_progress();
    assert!(script.trim_start().starts_with("() =>"));
    assert!(!script.contains("async "));
    assert!(!script.contains("await "));
    assert!(!script.contains("fetch("));
    assert!(!script.contains("setTimeout"));
    for key in [
        "generating",
        "streaming",
        "lastUserText",
        "assistantChars",
        "lastAssistantDone",
    ] {
        assert!(script.contains(key), "missing {key}");
    }
}

/// FORK: the level picker drives the slider variant first and keeps the
/// submenu path as the fallback.
#[test]
fn menu_select_handles_the_slider_picker_and_the_legacy_submenu() {
    let script = menu_select(MenuKind::Level, "^Alto$|^High$");
    assert!(script.contains("aria-keyshortcuts"));
    assert!(script.contains("ArrowLeft"));
    assert!(script.contains("ArrowRight"));
    assert!(script.contains("submenu not found"));
    assert!(!script.contains("async "));
}
