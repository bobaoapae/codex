use super::*;
use pretty_assertions::assert_eq;

#[test]
fn the_secret_segment_is_read_from_both_url_shapes() {
    assert_eq!(secret_segment("/mcp/abc"), Some("abc"));
    assert_eq!(secret_segment("/mcp/abc/healthz"), Some("abc"));
    assert_eq!(
        secret_segment("/.well-known/oauth-protected-resource/mcp/abc"),
        Some("abc")
    );
    assert_eq!(secret_segment("/mcp/"), None);
    assert_eq!(secret_segment("/other"), None);
    assert_eq!(
        secret_segment("/.well-known/oauth-protected-resource"),
        None
    );
}

#[test]
fn secrets_compare_exactly() {
    assert!(secrets_match("abc", "abc"));
    assert!(!secrets_match("abd", "abc"));
    assert!(!secrets_match("ab", "abc"));
    assert!(!secrets_match("", "abc"));
}

#[test]
fn fresh_secrets_are_long_and_unique() {
    let a = new_secret();
    let b = new_secret();
    assert_eq!(a.len(), 43, "32 bytes base64url without padding");
    assert_ne!(a, b);
}

#[test]
fn the_call_budget_is_a_sliding_window() {
    let limiter = RateLimiter::default();
    for _ in 0..CALLS_PER_WINDOW {
        assert!(limiter.admit_call());
    }
    assert!(!limiter.admit_call());
    for _ in 0..FAILED_CLAIMS_PER_WINDOW {
        assert!(!limiter.failed_claims_exhausted());
        limiter.record_failed_claim();
    }
    assert!(limiter.failed_claims_exhausted());
}

#[test]
fn broker_results_become_tool_results_with_images_and_structured_content() {
    let result = to_call_tool_result(BrokerResult {
        content: vec![
            ResultContent::Text {
                text: "hello".into(),
            },
            ResultContent::Image {
                data: "AAAA".into(),
                mime_type: "image/png".into(),
            },
        ],
        is_error: false,
        structured: Some(json!({"exit_code": 0})),
    });
    assert_eq!(result.is_error, Some(false));
    assert_eq!(result.content.len(), 2);
    assert_eq!(
        result.content[0].as_text().map(|text| text.text.as_str()),
        Some("hello")
    );
    assert!(result.content[1].as_image().is_some());
    assert_eq!(result.structured_content, Some(json!({"exit_code": 0})));

    let failed = to_call_tool_result(BrokerResult::error("nope"));
    assert_eq!(failed.is_error, Some(true));
}

#[test]
fn log_hashes_never_contain_the_value() {
    let hash = log_hash("turn_secret_value_0123456789");
    assert_eq!(hash.len(), 12);
    assert!(!hash.contains("secret"));
}
