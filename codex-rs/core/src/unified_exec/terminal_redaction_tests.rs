use super::*;
use pretty_assertions::assert_eq;

#[test]
fn redacts_assignments_bearer_values_and_known_token_prefixes() {
    let input = "token=top-secret --token separated-secret Authorization: Bearer bearer-secret sk-abcdefghijklmnopqrstuvwxyz";
    let output = redact_and_truncate(input, 512);
    assert!(!output.contains("top-secret"));
    assert!(!output.contains("separated-secret"));
    assert!(!output.contains("bearer-secret"));
    assert!(!output.contains("sk-abcdefghijklmnopqrstuvwxyz"));
    assert_eq!(output.matches("[REDACTED_SECRET]").count(), 4);
}

#[test]
fn truncation_respects_byte_limit_for_utf8_and_zero() {
    assert_eq!(redact_and_truncate("abcdefgh", 0), "");
    let truncated = redact_and_truncate("ééééé", 5);
    assert!(truncated.len() <= 5);
    assert!(truncated.is_char_boundary(truncated.len()));
}
