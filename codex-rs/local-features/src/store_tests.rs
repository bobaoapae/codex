use super::*;
use serde::Deserialize;

#[derive(Debug, Deserialize, PartialEq, Eq, Serialize)]
struct TestPlan {
    explanation: String,
    items: Vec<String>,
}

#[tokio::test]
async fn disabled_store_does_not_create_database() {
    let home = tempfile::tempdir().expect("temporary home");
    let store = LocalExtensionsStore::new(home.path(), &LocalExtensionsConfig::default());

    store
        .save_latest_plan(
            "thread",
            &TestPlan {
                explanation: "unused".into(),
                items: Vec::new(),
            },
        )
        .await
        .expect("disabled store is a no-op");

    assert!(!store.path().exists());
}

#[tokio::test]
async fn enabled_store_round_trips_latest_plan() {
    let home = tempfile::tempdir().expect("temporary home");
    let config = LocalExtensionsConfig {
        operations_dock: crate::OperationsDockMode::Auto,
        ..Default::default()
    };
    let store = LocalExtensionsStore::new(home.path(), &config);
    let expected = TestPlan {
        explanation: "keep full args".into(),
        items: vec!["one".into(), "two".into()],
    };

    store
        .save_latest_plan("thread", &expected)
        .await
        .expect("save plan");
    let actual = store
        .load_latest_plan::<TestPlan>("thread")
        .await
        .expect("load plan");

    assert_eq!(actual, Some(expected));
}

#[tokio::test]
async fn runtime_checkpoint_round_trips_and_rejects_changed_boundary() {
    let home = tempfile::tempdir().expect("temporary home");
    let rollout = home.path().join("rollout.jsonl");
    tokio::fs::write(&rollout, b"meta\nturn\n")
        .await
        .expect("write rollout");
    let config = LocalExtensionsConfig {
        resume: crate::ResumeMode::Checkpointed,
        ..Default::default()
    };
    let store = LocalExtensionsStore::new(home.path(), &config);
    let offset = tokio::fs::metadata(&rollout).await.expect("metadata").len();
    let boundary_hash = crate::checkpoints::boundary_hash(&rollout, offset)
        .await
        .expect("boundary hash");
    let checkpoint = crate::checkpoints::RuntimeCheckpoint {
        thread_id: "thread".into(),
        rollout_path: rollout.clone(),
        next_rollout_byte_offset: offset,
        next_rollout_ordinal: 2,
        session_meta_hash: "meta-hash".into(),
        boundary_hash,
        checkpoint: TestPlan {
            explanation: "materialized".into(),
            items: vec!["state".into()],
        },
    };
    store
        .save_runtime_checkpoint(checkpoint)
        .await
        .expect("save checkpoint");

    let loaded = store
        .load_runtime_checkpoint::<TestPlan>("thread", &rollout, "meta-hash")
        .await
        .expect("load checkpoint")
        .expect("checkpoint hit");
    assert_eq!(loaded.checkpoint.explanation, "materialized");

    tokio::fs::write(&rollout, b"changed!!\n")
        .await
        .expect("change rollout prefix");
    let miss = store
        .load_runtime_checkpoint::<TestPlan>("thread", &rollout, "meta-hash")
        .await
        .expect("changed rollout is a cache miss");
    assert!(miss.is_none());
}

#[tokio::test]
async fn runtime_checkpoint_rejects_truncation_and_session_meta_change() {
    let home = tempfile::tempdir().expect("temporary home");
    let rollout = home.path().join("rollout.jsonl");
    tokio::fs::write(&rollout, b"session-meta\nturn\n")
        .await
        .expect("write rollout");
    let config = LocalExtensionsConfig {
        resume: crate::ResumeMode::Checkpointed,
        ..Default::default()
    };
    let store = LocalExtensionsStore::new(home.path(), &config);
    let offset = tokio::fs::metadata(&rollout).await.expect("metadata").len();
    store
        .save_runtime_checkpoint(crate::checkpoints::RuntimeCheckpoint {
            thread_id: "thread".into(),
            rollout_path: rollout.clone(),
            next_rollout_byte_offset: offset,
            next_rollout_ordinal: 2,
            session_meta_hash: "original".into(),
            boundary_hash: crate::checkpoints::boundary_hash(&rollout, offset)
                .await
                .expect("boundary hash"),
            checkpoint: TestPlan {
                explanation: "state".into(),
                items: Vec::new(),
            },
        })
        .await
        .expect("save checkpoint");

    assert!(
        store
            .load_runtime_checkpoint::<TestPlan>("thread", &rollout, "different")
            .await
            .expect("meta mismatch is a miss")
            .is_none()
    );
    tokio::fs::write(&rollout, b"short")
        .await
        .expect("truncate rollout");
    assert!(
        store
            .load_runtime_checkpoint::<TestPlan>("thread", &rollout, "original")
            .await
            .expect("truncation is a miss")
            .is_none()
    );
}
