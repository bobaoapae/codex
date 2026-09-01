use super::*;

use codex_protocol::error::CodexErrorDetails;

#[test]
fn stale_generation_errors_are_explicitly_classified() {
    let error = fleet_error(
        "resume",
        CodexErrorDetails::InvalidRequest("stale fleet generation".to_string()).into(),
    );
    assert_eq!(error.code, crate::error_code::INVALID_REQUEST_ERROR_CODE);
    assert!(error.message.starts_with("staleGeneration:"));
    assert_eq!(
        error.data,
        Some(serde_json::json!({
            "kind": "staleGeneration",
            "operation": "resume",
            "retry": false,
        }))
    );
}

#[test]
fn root_thread_ids_are_parsed_without_accepting_arbitrary_strings() {
    let root = ThreadId::from_u128(17);
    assert_eq!(parse_root_thread_id(&root.to_string()), Ok(root));
    assert!(parse_root_thread_id("not-a-thread-id").is_err());
}
