use pretty_assertions::assert_eq;
use serde_json::json;
use ts_rs::TS;

use super::ExtensionItem;
use super::image_generation::ImageGenerationItem;
use super::receipt::MAX_TAGS;
use super::receipt::ReceiptAttachedItem;
use super::receipt::ReceiptReference;
use super::receipt::ReceiptStatus;
use super::receipt::is_forbidden_metadata_key;
use super::sleep::SleepItem;
use super::web_search::WebSearchAction;
use super::web_search::WebSearchItem;
use std::collections::BTreeMap;

fn completed_image_generation_item() -> ExtensionItem {
    ExtensionItem::ImageGeneration(ImageGenerationItem {
        id: "image-1".to_string(),
        status: "completed".to_string(),
        revised_prompt: Some("A blue square".to_string()),
        result: "cG5n".to_string(),
        transparent_background: None,
        failure: None,
        saved_path: None,
        imagegen_request_id: None,
    })
}

#[test]
fn image_generation_item_preserves_stable_wire_shape() {
    let item = completed_image_generation_item();
    let value = serde_json::to_value(&item).expect("serialize extension item");

    assert_eq!(
        value,
        json!({
            "kind": "image_gen.generation",
            "id": "image-1",
            "status": "completed",
            "revisedPrompt": "A blue square",
            "result": "cG5n",
            "transparentBackground": null,
            "failure": null,
        })
    );
    assert_eq!(
        serde_json::from_value::<ExtensionItem>(value).expect("deserialize extension item"),
        item
    );
    assert_eq!(
        serde_json::from_value::<ExtensionItem>(json!({
            "kind": "image_gen.generation",
            "id": "image-1",
            "status": "completed",
            "revisedPrompt": "A blue square",
            "result": "cG5n",
        }))
        .expect("deserialize legacy image-generation item without transparency metadata"),
        item
    );
}

#[test]
fn image_generation_item_preserves_authoritative_transparency() {
    let ExtensionItem::ImageGeneration(mut image) = completed_image_generation_item() else {
        panic!("expected image-generation item");
    };
    image.transparent_background = Some(true);
    let item = ExtensionItem::ImageGeneration(image);
    let value = serde_json::to_value(&item).expect("serialize extension item");

    assert_eq!(
        value,
        json!({
            "kind": "image_gen.generation",
            "id": "image-1",
            "status": "completed",
            "revisedPrompt": "A blue square",
            "result": "cG5n",
            "transparentBackground": true,
            "failure": null,
        })
    );
    assert_eq!(
        serde_json::from_value::<ExtensionItem>(value).expect("deserialize extension item"),
        item
    );
}

#[test]
fn image_generation_transparency_is_optional_in_typescript() {
    assert!(
        ImageGenerationItem::inline().contains("transparentBackground?: boolean"),
        "image-generation transparency must remain optional for existing TypeScript clients"
    );
}

#[test]
fn image_generation_request_id_stays_internal() {
    let ExtensionItem::ImageGeneration(mut image) = completed_image_generation_item() else {
        panic!("expected image-generation item");
    };
    image.imagegen_request_id = Some("req-imagegen-123".to_string());
    let item = ExtensionItem::ImageGeneration(image);
    let value = serde_json::to_value(&item).expect("serialize extension item");

    assert!(value.get("imagegenRequestId").is_none());
    let ExtensionItem::ImageGeneration(round_tripped) =
        serde_json::from_value::<ExtensionItem>(value).expect("deserialize extension item")
    else {
        panic!("expected image-generation item");
    };
    assert_eq!(round_tripped.imagegen_request_id, None);
    assert!(!ImageGenerationItem::inline().contains("imagegenRequestId"));
}

#[test]
fn web_search_item_preserves_stable_wire_shape() {
    let item = ExtensionItem::WebSearch(WebSearchItem {
        id: "search-1".to_string(),
        query: "docs".to_string(),
        action: Some(WebSearchAction::Search {
            query: Some("docs".to_string()),
            queries: None,
        }),
        results: None,
    });
    let value = serde_json::to_value(&item).expect("serialize extension item");

    assert_eq!(
        value,
        json!({
            "kind": "web.search",
            "id": "search-1",
            "query": "docs",
            "action": {
                "type": "search",
                "query": "docs",
                "queries": null,
            },
            "results": null,
        })
    );
    assert_eq!(
        serde_json::from_value::<ExtensionItem>(value).expect("deserialize extension item"),
        item
    );
    assert_eq!(
        serde_json::from_value::<ExtensionItem>(json!({
            "kind": "web.search",
            "id": "search-1",
            "query": "docs",
            "action": {
                "type": "search",
                "query": "docs",
                "queries": null,
            },
        }))
        .expect("deserialize legacy extension item without results"),
        item
    );
}

#[test]
fn sleep_item_preserves_stable_wire_shape() {
    let item = ExtensionItem::Sleep(SleepItem {
        id: "sleep-1".to_string(),
        duration_ms: 1_000,
    });
    let value = serde_json::to_value(&item).expect("serialize extension item");

    assert_eq!(
        value,
        json!({
            "kind": "clock.sleep",
            "id": "sleep-1",
            "durationMs": 1_000,
        })
    );
    assert_eq!(
        serde_json::from_value::<ExtensionItem>(value).expect("deserialize extension item"),
        item
    );
}

#[test]
fn unknown_extension_kind_is_rejected() {
    let value = json!({
        "kind": "image_gen.unknown",
        "id": "image-1",
    });

    assert!(serde_json::from_value::<ExtensionItem>(value).is_err());
}

#[test]
fn malformed_known_extension_payload_is_rejected() {
    let value = json!({
        "kind": "image_gen.generation",
        "id": "image-1",
        "status": "completed",
    });

    assert!(serde_json::from_value::<ExtensionItem>(value).is_err());
}

fn receipt_item() -> ReceiptAttachedItem {
    let mut item = ReceiptAttachedItem::new(
        "receipt-1",
        1,
        "physical.smoke",
        "Android cold start",
        ReceiptStatus::Pass,
        "2026-08-31T12:00:00Z",
        "tester",
    )
    .expect("valid receipt");
    item.thread_id = Some("thread-1".to_string());
    item.turn_id = Some("turn-1".to_string());
    item.job_id = Some("job-1".to_string());
    item.plan_snapshot_id = Some("plan-1".to_string());
    item.updated_at = Some("2026-08-31T12:00:01Z".to_string());
    item.finished_at = Some("2026-08-31T12:00:02Z".to_string());
    item.provenance = Some(json!({"runner": "device-lab", "future": {"version": 2}}));
    item.tags = BTreeMap::from([
        ("device".to_string(), "pixel-8".to_string()),
        ("owner".to_string(), "qa".to_string()),
    ]);
    item.refs = vec![ReceiptReference {
        kind: "artifact".to_string(),
        id: "artifact-1".to_string(),
    }];
    item.metadata = Some(json!({"futureField": {"value": 7}}));
    item.validate().expect("receipt remains valid");
    item
}

#[test]
fn receipt_attached_preserves_generic_fields_and_unknown_values() {
    let item = ExtensionItem::ReceiptAttached(receipt_item());
    assert_eq!(item.id(), "receipt-1");
    let value = serde_json::to_value(&item).expect("serialize receipt item");
    assert_eq!(value["kind"], "receipt.attached");
    assert_eq!(value["receiptKind"], "physical.smoke");
    assert_eq!(value["schemaVersion"], 1);
    assert_eq!(value["metadata"]["futureField"]["value"], 7);
    assert_eq!(
        serde_json::from_value::<ExtensionItem>(value).expect("deserialize receipt item"),
        item
    );
}

#[test]
fn receipt_attached_accepts_unknown_schema_version_and_kind() {
    let mut value = serde_json::to_value(receipt_item()).expect("serialize receipt");
    value
        .as_object_mut()
        .expect("receipt object")
        .remove("receiptKind");
    value["kind"] = json!("future.provider.result");
    value["schemaVersion"] = json!(u64::MAX);
    let decoded = serde_json::from_value::<ReceiptAttachedItem>(value).expect("future receipt");
    assert_eq!(decoded.schema_version, u64::MAX);
    assert_eq!(decoded.kind, "future.provider.result");
}

#[test]
fn receipt_attached_rejects_unbounded_or_raw_metadata() {
    let mut too_many_tags = receipt_item();
    too_many_tags.tags = (0..=MAX_TAGS)
        .map(|index| (format!("tag-{index}"), "value".to_string()))
        .collect();
    assert!(too_many_tags.validate().is_err());

    let mut raw = receipt_item();
    raw.metadata = Some(json!({"stdout": "must not be copied"}));
    assert!(raw.validate().is_err());

    let mut oversized = receipt_item();
    oversized.metadata = Some(json!({"future": "x".repeat(65 * 1024)}));
    assert!(oversized.validate().is_err());
}

#[test]
fn forbidden_metadata_key_helper_covers_canonical_union() {
    let forbidden = [
        "stdout",
        "stderr",
        "aggregatedOutput",
        "arguments",
        "args",
        "ciphertext",
        "encryptedContent",
        "payload",
        "raw",
        "rawPayload",
        "toolInput",
        "toolOutput",
        "toolResponse",
        "path",
        "paths",
        "cwd",
        "workdir",
        "command",
        "argv",
        "env",
        "environment",
        "output",
        "rawOutput",
    ];
    for key in forbidden {
        assert!(
            is_forbidden_metadata_key(key),
            "key should be forbidden: {key}"
        );
        let mut receipt = receipt_item();
        receipt.metadata = Some(json!({key: "not persisted"}));
        assert!(receipt.validate().is_err(), "key should be rejected: {key}");
    }

    for key in ["futureResult", "rawData", "pathology", "environmental"] {
        assert!(
            !is_forbidden_metadata_key(key),
            "key should remain safe: {key}"
        );
        let mut receipt = receipt_item();
        receipt.metadata = Some(json!({key: true}));
        receipt
            .validate()
            .expect("unknown metadata key should be safe");
    }

    for key in ["AGGREGATED_OUTPUT", "tool-output", "work_dir", "ENV"] {
        assert!(
            is_forbidden_metadata_key(key),
            "separator normalization: {key}"
        );
    }
}
