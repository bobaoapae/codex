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
