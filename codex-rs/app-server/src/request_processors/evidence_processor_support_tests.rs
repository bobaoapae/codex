use codex_app_server_protocol::EvidenceStatus;
use codex_protocol::ThreadId;
use codex_state::WorkflowReceipt;
use pretty_assertions::assert_eq;
use serde_json::Value;
use serde_json::json;
use std::collections::BTreeMap;

use super::api_evidence;
use super::api_evidence_with_redaction;
use super::receipt_item;

#[test]
fn api_evidence_redacts_nested_payload_keys() {
    let receipt = WorkflowReceipt {
        receipt_id: "receipt-1".to_string(),
        run_id: None,
        thread_id: Some("thread-1".to_string()),
        turn_id: Some("turn-1".to_string()),
        job_id: None,
        plan_snapshot_id: None,
        schema_version: 7,
        kind: "physical.smoke".to_string(),
        subject: "smoke".to_string(),
        status: "pass".to_string(),
        source: "test-hook".to_string(),
        provenance: Some(json!({
            "handlerId": "runner",
            "nested": {"path": "C:/secret", "safe": true},
        })),
        tags: Vec::new(),
        payload: Some(json!({
            "testName": "smoke",
            "nested": {
                "stdout": "secret",
                "safe": "kept",
            },
            "items": [{"output": "secret", "count": 1}],
        })),
        references: Vec::new(),
        created_at_ms: 1_000,
    };

    let evidence = api_evidence(receipt);
    assert_eq!(
        evidence.metadata,
        Some(json!({
            "testName": "smoke",
            "nested": {"safe": "kept"},
            "items": [{"count": 1}],
        }))
    );
    assert_eq!(
        evidence.provenance,
        Some(json!({
            "handlerId": "runner",
            "nested": {"safe": true},
        }))
    );
}

#[test]
fn export_redaction_reports_removed_field_count() {
    let receipt = WorkflowReceipt {
        receipt_id: "receipt-redacted".to_string(),
        run_id: None,
        thread_id: Some("thread-1".to_string()),
        turn_id: None,
        job_id: None,
        plan_snapshot_id: None,
        schema_version: 1,
        kind: "test".to_string(),
        subject: "redaction".to_string(),
        status: "pass".to_string(),
        source: "test".to_string(),
        provenance: Some(json!({"env": "hidden", "safe": true})),
        tags: Vec::new(),
        payload: Some(json!({"command": "hidden", "nested": {"output": "hidden"}})),
        references: Vec::new(),
        created_at_ms: 1_000,
    };
    let (_evidence, redacted_count) = api_evidence_with_redaction(receipt);
    assert_eq!(redacted_count, 3);
}

#[test]
fn receipt_item_preserves_unknown_kind_and_schema_version() {
    let receipt = receipt_item(
        "receipt-unknown".to_string(),
        4_000,
        "future.vendor.result".to_string(),
        "future result".to_string(),
        EvidenceStatus::Informational,
        ThreadId::from_u128(1),
        None,
        None,
        None,
        1_000,
        "trusted-hook".to_string(),
        None,
        Default::default(),
        Vec::new(),
        Some(json!({"futureField": "preserved"})),
    )
    .expect("valid generic receipt");
    assert_eq!(receipt.schema_version, 4_000);
    assert_eq!(receipt.kind, "future.vendor.result");
    assert_eq!(
        receipt.thread_id.as_deref(),
        Some("00000000-0000-0000-0000-000000000001")
    );
}

#[test]
fn receipt_item_rejects_raw_metadata_before_append() {
    let error = receipt_item(
        "receipt-raw".to_string(),
        1,
        "test".to_string(),
        "raw result".to_string(),
        EvidenceStatus::Fail,
        ThreadId::from_u128(2),
        None,
        None,
        None,
        1_000,
        "trusted-hook".to_string(),
        None,
        Default::default(),
        Vec::new(),
        Some(json!({"nested": {"stdout": "secret"}})),
    )
    .expect_err("raw metadata must be rejected");
    assert!(error.message.contains("forbidden"));
}

#[test]
fn receipt_item_rejects_all_reserved_raw_metadata_names() {
    for key in [
        "stdout",
        "toolOutput",
        "raw",
        "command",
        "env",
        "ciphertext",
    ] {
        let mut metadata = serde_json::Map::new();
        metadata.insert(key.to_string(), Value::String("secret".to_string()));
        let result = receipt_item(
            format!("receipt-{key}"),
            1,
            "test".to_string(),
            "raw result".to_string(),
            EvidenceStatus::Fail,
            ThreadId::from_u128(3),
            None,
            None,
            None,
            1_000,
            "trusted-hook".to_string(),
            None,
            BTreeMap::new(),
            Vec::new(),
            Some(Value::Object(metadata)),
        );
        assert!(result.is_err(), "reserved key {key} should be rejected");
    }
}
