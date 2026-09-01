use crate::MAX_THREAD_ARTIFACT_PAYLOAD_BYTES;
use crate::SqliteConfig;
use crate::StateRuntime;
use crate::ThreadArtifactAttachmentOutcome;
use crate::ThreadArtifactReadEncoding;
use crate::runtime::test_support::test_thread_metadata;
use crate::runtime::test_support::unique_temp_dir;
use codex_protocol::ThreadId;
use codex_utils_absolute_path::test_support::PathExt;
use pretty_assertions::assert_eq;
use serde_json::json;
use std::sync::Arc;

async fn runtime_with_threads(count: usize) -> (Arc<StateRuntime>, SqliteConfig, Vec<ThreadId>) {
    let home = unique_temp_dir();
    let sqlite = SqliteConfig::new_for_testing(home.as_path().abs());
    let runtime = StateRuntime::init(sqlite.clone(), "test-provider".to_string())
        .await
        .expect("state runtime");
    let mut thread_ids = Vec::with_capacity(count);
    for _ in 0..count {
        let thread_id = ThreadId::new();
        let metadata = test_thread_metadata(home.as_path(), thread_id, home.clone());
        runtime
            .upsert_thread(&metadata)
            .await
            .expect("thread metadata");
        thread_ids.push(thread_id);
    }
    (runtime, sqlite, thread_ids)
}

#[tokio::test]
async fn attach_is_idempotent_and_rejects_payload_conflicts() {
    let (runtime, _sqlite, thread_ids) = runtime_with_threads(1).await;
    let thread_id = thread_ids[0];
    let payload = json!({"result": "ok", "count": 2});
    let first = runtime
        .attach_thread_artifact(thread_id, "test.result", "run-1", payload.clone())
        .await
        .expect("first attach");
    let ThreadArtifactAttachmentOutcome::Created(created) = first else {
        panic!("first attach should create an artifact");
    };

    let second = runtime
        .attach_thread_artifact(thread_id, "test.result", "run-1", payload)
        .await
        .expect("idempotent attach");
    let ThreadArtifactAttachmentOutcome::Existing(existing) = second else {
        panic!("second attach should return the existing artifact");
    };
    assert_eq!(existing, created);
    assert!(
        runtime
            .attach_thread_artifact(
                thread_id,
                "test.result",
                "run-1",
                json!({"result": "changed"}),
            )
            .await
            .is_err()
    );

    assert_eq!(
        runtime.get_thread_artifact(&created.id).await.unwrap(),
        Some(created)
    );
}

#[tokio::test]
async fn list_uses_selection_bound_keyset_pagination() {
    let (runtime, _sqlite, thread_ids) = runtime_with_threads(2).await;
    let first_thread = thread_ids[0];
    let second_thread = thread_ids[1];
    let mut expected_ids = Vec::new();
    for (thread_id, identity_key) in [
        (first_thread, "first"),
        (first_thread, "second"),
        (second_thread, "third"),
    ] {
        let outcome = runtime
            .attach_thread_artifact(
                thread_id,
                "test.result",
                identity_key,
                json!({"identity": identity_key}),
            )
            .await
            .expect("attach artifact");
        let ThreadArtifactAttachmentOutcome::Created(artifact) = outcome else {
            panic!("unique identity should create an artifact");
        };
        expected_ids.push(artifact.id);
    }

    let first_page = runtime
        .list_thread_artifacts(&thread_ids, None, 2)
        .await
        .expect("first artifact page");
    assert_eq!(first_page.artifacts.len(), 2);
    let cursor = first_page.next_cursor.expect("second artifact page");
    let second_page = runtime
        .list_thread_artifacts(&thread_ids, Some(&cursor), 2)
        .await
        .expect("second artifact page");
    assert_eq!(second_page.artifacts.len(), 1);
    assert_eq!(second_page.next_cursor, None);

    let mut listed_ids = first_page
        .artifacts
        .into_iter()
        .chain(second_page.artifacts)
        .map(|artifact| artifact.id)
        .collect::<Vec<_>>();
    let mut sorted_expected = expected_ids;
    listed_ids.sort();
    sorted_expected.sort();
    assert_eq!(listed_ids, sorted_expected);

    let selected_page = runtime
        .list_thread_artifacts(&[first_thread], None, 20)
        .await
        .expect("selected artifact page");
    assert_eq!(selected_page.artifacts.len(), 2);
    assert!(
        selected_page
            .artifacts
            .iter()
            .all(|artifact| artifact.thread_id == first_thread)
    );
    assert!(
        runtime
            .list_thread_artifacts(&[second_thread], Some(&cursor), 2)
            .await
            .is_err()
    );
    assert!(
        runtime
            .list_thread_artifacts(&thread_ids, Some("not-a-cursor"), 2)
            .await
            .is_err()
    );
}

#[tokio::test]
async fn read_returns_utf8_bounded_pages_and_rejects_cross_artifact_cursor() {
    let (runtime, _sqlite, thread_ids) = runtime_with_threads(1).await;
    let payload = json!({
        "message": "olá 🌊 — payload stays JSON",
        "values": [1, 2, 3, 4],
    });
    let outcome = runtime
        .attach_thread_artifact(thread_ids[0], "test.result", "read-1", payload.clone())
        .await
        .expect("attach artifact");
    let ThreadArtifactAttachmentOutcome::Created(artifact) = outcome else {
        panic!("artifact should be created");
    };
    let serialized = serde_json::to_string(&payload).expect("serialize payload");

    let mut cursor = None;
    let mut combined = String::new();
    let mut pages = 0;
    loop {
        let page = runtime
            .read_thread_artifact(&artifact.id, cursor.as_deref(), 7)
            .await
            .expect("read artifact")
            .expect("artifact exists");
        assert_eq!(page.encoding, ThreadArtifactReadEncoding::JsonUtf8);
        assert_eq!(page.total_bytes, serialized.len());
        assert_eq!(page.offset, combined.len());
        assert!(page.chunk.len() <= 7 || page.chunk.chars().count() == 1);
        combined.push_str(&page.chunk);
        pages += 1;
        if page.complete {
            assert_eq!(page.next_cursor, None);
            break;
        }
        cursor = page.next_cursor;
        assert!(cursor.is_some());
        assert!(pages < 100);
    }
    assert!(pages > 1);
    assert_eq!(combined, serialized);

    let first_page = runtime
        .read_thread_artifact(&artifact.id, None, 7)
        .await
        .unwrap()
        .unwrap();
    let first_cursor = first_page.next_cursor.as_deref().expect("next read cursor");
    let other = runtime
        .attach_thread_artifact(
            thread_ids[0],
            "test.result",
            "read-2",
            json!({"other": true}),
        )
        .await
        .unwrap();
    let ThreadArtifactAttachmentOutcome::Created(other) = other else {
        panic!("second artifact should be created");
    };
    assert!(
        runtime
            .read_thread_artifact(&other.id, Some(first_cursor), 7)
            .await
            .is_err()
    );
    assert!(
        runtime
            .read_thread_artifact(&artifact.id, Some("not-a-cursor"), 7)
            .await
            .is_err()
    );
    assert!(
        runtime
            .read_thread_artifact(&artifact.id, Some("éé"), 7)
            .await
            .is_err()
    );
}

#[tokio::test]
async fn artifacts_survive_reopen_and_missing_threads_fail_closed() {
    let (runtime, sqlite, thread_ids) = runtime_with_threads(1).await;
    let thread_id = thread_ids[0];
    let outcome = runtime
        .attach_thread_artifact(
            thread_id,
            "test.result",
            "reopen-1",
            json!({"durable": true}),
        )
        .await
        .unwrap();
    let ThreadArtifactAttachmentOutcome::Created(artifact) = outcome else {
        panic!("artifact should be created");
    };
    runtime.close().await;

    let reopened = StateRuntime::init(sqlite, "test-provider".to_string())
        .await
        .expect("reopen state runtime");
    assert_eq!(
        reopened.get_thread_artifact(&artifact.id).await.unwrap(),
        Some(artifact)
    );
    let missing_thread = ThreadId::new();
    assert!(
        reopened
            .attach_thread_artifact(
                missing_thread,
                "test.result",
                "missing-thread",
                json!({"should": "fail"}),
            )
            .await
            .is_err()
    );
    reopened.close().await;
}

#[tokio::test]
async fn artifact_identity_and_payload_limits_are_enforced() {
    let (runtime, _sqlite, thread_ids) = runtime_with_threads(1).await;
    let oversized = "x".repeat(MAX_THREAD_ARTIFACT_PAYLOAD_BYTES);
    assert!(
        runtime
            .attach_thread_artifact(
                thread_ids[0],
                "test.result",
                "oversized-payload",
                json!(oversized),
            )
            .await
            .is_err()
    );
    assert!(
        runtime
            .attach_thread_artifact(
                thread_ids[0],
                &"t".repeat(crate::MAX_THREAD_ARTIFACT_TYPE_BYTES + 1),
                "long-type",
                json!({}),
            )
            .await
            .is_err()
    );
    assert!(
        runtime
            .attach_thread_artifact(
                thread_ids[0],
                "test.result",
                &"k".repeat(crate::MAX_THREAD_ARTIFACT_IDENTITY_KEY_BYTES + 1),
                json!({}),
            )
            .await
            .is_err()
    );
}
