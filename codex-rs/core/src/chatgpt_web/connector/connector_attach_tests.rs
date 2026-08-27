//! FORK: tests for the connector browser-attach page scripts.

use super::*;

/// Every script is a non-async function expression that resolves a JSON string.
fn assert_pure_function(script: &str) {
    assert!(
        script.trim_start().starts_with("() =>"),
        "not a function expression: {}",
        &script[..script.len().min(40)]
    );
    assert!(!script.contains("async "), "script must not be async");
    assert!(!script.contains("await "), "script must not use await");
    assert!(
        script.contains("JSON.stringify"),
        "script must resolve JSON"
    );
}

#[test]
fn mention_and_compose_script_is_pure_and_carries_name_text_and_trigger() {
    let script = mention_and_compose_script("Codex \"Native\"", "run the tests\nplease");
    assert_pure_function(&script);
    // The connector name is embedded through serde, so embedded quotes and the
    // newline in the text are escaped rather than breaking the JS string.
    assert!(script.contains(r#"const NAME = "Codex \"Native\"""#));
    assert!(script.contains(r#"const TEXT = "run the tests\nplease""#));
    // The trigger is the first word.
    assert!(script.contains(r#"const TRIGGER = "Codex""#));
    assert!(script.contains(r#"[data-id^="plugin:"][data-keyword]"#));
    assert!(script.contains(".__menu-item[tabindex=\"0\"]"));
}

#[test]
fn approval_script_is_pure_and_matches_pt_and_en_buttons() {
    let script = approval_script("Codex Native", true);
    assert_pure_function(&script);
    assert!(script.contains("sempre permitir|allow always"));
    assert!(script.contains("permitir uma vez|allow once"));
    assert!(script.contains(r#"[data-testid="tool-approval-card"]"#));
    assert!(script.contains("const PREFER_ALWAYS = true"));
}

#[test]
fn an_approval_click_decodes() {
    let result: ApprovalResult =
        serde_json::from_str(r#"{"found": true, "clicked": true, "button": "Sempre permitir"}"#)
            .expect("decode");
    assert!(result.clicked);
    assert_eq!(result.button.as_deref(), Some("Sempre permitir"));
}
