use super::*;

use codex_core::ownership::OwnershipError;

#[test]
fn ownership_errors_are_structured_and_non_retryable() {
    let error = ownership_error("grant", OwnershipError::ReadOnlyRole);
    assert_eq!(error.code, crate::error_code::INVALID_REQUEST_ERROR_CODE);
    assert_eq!(
        error.data,
        Some(serde_json::json!({
            "kind": "readOnlyRole",
            "operation": "grant",
            "retry": false,
        }))
    );
}

#[test]
fn empty_list_filters_are_rejected() {
    assert!(validate_optional_filter(Some(""), "path").is_err());
    assert!(validate_optional_filter(Some("owner"), "ownerThreadId").is_ok());
}
