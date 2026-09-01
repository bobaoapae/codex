use super::*;
use crate::SqliteConfig;
use crate::runtime::test_support::unique_temp_dir;
use codex_utils_absolute_path::test_support::PathExt;
use pretty_assertions::assert_eq;

fn sqlite_config() -> SqliteConfig {
    let home = unique_temp_dir();
    SqliteConfig::new_for_testing(home.as_path().abs())
}

fn create(fork_id: &str) -> WorkflowForkMetricsCreate {
    WorkflowForkMetricsCreate {
        fork_id: fork_id.to_string(),
        spawn_call_id: "call-1".to_string(),
        parent_thread_id: "parent-1".to_string(),
        fork_turns: WorkflowForkTurns::LastNTurns(2),
        spawn_requested_at_ms: 100,
        projected_fork_bytes: 80,
        projected_fork_tokens: 20,
        context_entries: vec![WorkflowForkContextEntry {
            fork_id: fork_id.to_string(),
            sequence: 0,
            origin: WorkflowForkContextOrigin::InheritedHistory,
            byte_count: 80,
            token_count: 20,
        }],
    }
}

#[tokio::test]
async fn fork_metrics_round_trip_timestamps_cache_counts_and_context_origin() {
    let store = WorkflowStore::open(&sqlite_config())
        .await
        .expect("open workflow store");
    store
        .create_fork_metrics(&create("fork-1"))
        .await
        .expect("create fork metrics");
    assert!(
        store
            .mark_fork_child_created("fork-1", "child-1", 110)
            .await
            .expect("child created")
    );
    assert!(
        store
            .mark_fork_first_event("fork-1", 120)
            .await
            .expect("first event")
    );
    assert!(
        store
            .mark_fork_first_new_response("fork-1", 130)
            .await
            .expect("first response")
    );
    assert!(
        store
            .add_fork_provider_usage("fork-1", 100, 40, 10, 140)
            .await
            .expect("provider usage")
    );
    assert!(
        store
            .append_fork_context_entry(&WorkflowForkContextEntry {
                fork_id: "fork-1".to_string(),
                sequence: 1,
                origin: WorkflowForkContextOrigin::NewOutput,
                byte_count: 16,
                token_count: 4,
            })
            .await
            .expect("new output origin")
    );
    assert!(
        store
            .mark_fork_completed("fork-1", 150)
            .await
            .expect("completion")
    );
    let metrics = store
        .get_fork_metrics("fork-1")
        .await
        .expect("read metrics")
        .expect("metrics exist");
    assert_eq!(metrics.child_thread_id.as_deref(), Some("child-1"));
    assert_eq!(metrics.spawn_requested_at_ms, 100);
    assert_eq!(metrics.child_created_at_ms, Some(110));
    assert_eq!(metrics.first_event_at_ms, Some(120));
    assert_eq!(metrics.first_new_response_at_ms, Some(130));
    assert_eq!(metrics.completed_at_ms, Some(150));
    assert_eq!(metrics.provider_input_tokens, Some(100));
    assert_eq!(metrics.provider_cached_input_tokens, Some(40));
    assert_eq!(metrics.provider_uncached_input_tokens, Some(50));
    assert_eq!(metrics.provider_cache_write_input_tokens, Some(10));
    assert_eq!(
        store
            .list_fork_context("fork-1")
            .await
            .expect("read context"),
        vec![
            WorkflowForkContextEntry {
                fork_id: "fork-1".to_string(),
                sequence: 0,
                origin: WorkflowForkContextOrigin::InheritedHistory,
                byte_count: 80,
                token_count: 20,
            },
            WorkflowForkContextEntry {
                fork_id: "fork-1".to_string(),
                sequence: 1,
                origin: WorkflowForkContextOrigin::NewOutput,
                byte_count: 16,
                token_count: 4,
            },
        ]
    );
    store.close().await;
}

#[tokio::test]
async fn fork_warning_claim_is_idempotent_and_does_not_compact() {
    let store = WorkflowStore::open(&sqlite_config())
        .await
        .expect("open workflow store");
    store
        .create_fork_metrics(&create("fork-2"))
        .await
        .expect("create fork metrics");
    assert!(
        store
            .claim_fork_compaction_warning("fork-2", 95, 100, 200)
            .await
            .expect("claim warning")
    );
    assert!(
        !store
            .claim_fork_compaction_warning("fork-2", 96, 100, 201)
            .await
            .expect("duplicate warning claim")
    );
    let metrics = store
        .get_fork_metrics("fork-2")
        .await
        .expect("read metrics")
        .expect("metrics exist");
    assert!(metrics.warning_emitted);
    assert_eq!(metrics.warning_projected_tokens, Some(95));
    assert_eq!(metrics.warning_limit_tokens, Some(100));
    assert_eq!(metrics.completed_at_ms, None);
    store.close().await;
}

#[tokio::test]
async fn fork_projection_is_bounded_and_reopenable() {
    let sqlite = sqlite_config();
    let store = WorkflowStore::open(&sqlite)
        .await
        .expect("open workflow store");
    let mut request = create("fork-3");
    request.context_entries = (0..MAX_FORK_CONTEXT_ENTRIES)
        .map(|sequence| WorkflowForkContextEntry {
            fork_id: "fork-3".to_string(),
            sequence: i64::try_from(sequence).expect("sequence fits"),
            origin: WorkflowForkContextOrigin::InheritedHistory,
            byte_count: 1,
            token_count: 1,
        })
        .collect();
    store
        .create_fork_metrics(&request)
        .await
        .expect("create bounded projection");
    store.close().await;
    let reopened = WorkflowStore::open(&sqlite)
        .await
        .expect("reopen workflow store");
    assert_eq!(
        reopened
            .list_fork_context("fork-3")
            .await
            .expect("read bounded context")
            .len(),
        MAX_FORK_CONTEXT_ENTRIES
    );
    reopened.close().await;
}
