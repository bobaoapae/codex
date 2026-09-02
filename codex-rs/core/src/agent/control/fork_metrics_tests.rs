use super::*;
use codex_protocol::protocol::AgentMessageEvent;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::TokenUsage;
use codex_protocol::protocol::TurnCompleteEvent;
use codex_protocol::protocol::WarningEvent;
use codex_state::SqliteConfig;
use codex_utils_absolute_path::test_support::PathExt;
use pretty_assertions::assert_eq;
use std::sync::Arc;
use std::sync::atomic::AtomicI64;
use std::sync::atomic::Ordering;
use tempfile::tempdir;

#[derive(Debug)]
struct FakeClock(AtomicI64);

impl FakeClock {
    fn new(value: i64) -> Self {
        Self(AtomicI64::new(value))
    }
}

impl ForkMetricsClock for FakeClock {
    fn now_ms(&self) -> i64 {
        self.0.load(Ordering::Relaxed)
    }
}

fn sqlite_config() -> SqliteConfig {
    let home = Box::leak(Box::new(tempdir().expect("workflow home")));
    SqliteConfig::new_for_testing(home.path().abs())
}

#[tokio::test]
async fn first_event_and_response_exclude_inherited_history_and_aggregate_cache_usage() {
    let workflow = WorkflowStore::open(&sqlite_config())
        .await
        .expect("open workflow store");
    let clock = Arc::new(FakeClock::new(100));
    let tracker = ForkMetricsTracker::with_clock(clock.clone());
    let parent = ThreadId::new();
    let child = ThreadId::new();
    let fork_id = tracker
        .spawn_requested(
            Some(workflow.clone()),
            parent,
            "spawn-call",
            WorkflowForkTurns::FullHistory,
        )
        .await;
    tracker
        .update_projection(
            Some(workflow.clone()),
            &fork_id,
            400,
            100,
            vec![WorkflowForkContextEntry {
                fork_id: fork_id.clone(),
                sequence: 0,
                origin: WorkflowForkContextOrigin::InheritedHistory,
                byte_count: 400,
                token_count: 100,
            }],
        )
        .await;
    clock.0.store(110, Ordering::Relaxed);
    tracker
        .child_created(Some(workflow.clone()), &fork_id, child)
        .await;
    clock.0.store(120, Ordering::Relaxed);
    tracker
        .observe_event(
            Some(workflow.clone()),
            child,
            &EventMsg::Warning(WarningEvent {
                message: "inherited item is not replayed as an event".to_string(),
            }),
        )
        .await;
    clock.0.store(130, Ordering::Relaxed);
    tracker
        .observe_event(
            Some(workflow.clone()),
            child,
            &EventMsg::AgentMessage(AgentMessageEvent {
                message: "new response".to_string(),
                phase: None,
                memory_citation: None,
                delivery: None,
                questions: None,
            }),
        )
        .await;
    tracker
        .observe_usage(
            Some(workflow.clone()),
            child,
            &TokenUsage {
                input_tokens: 100,
                cached_input_tokens: 60,
                cache_write_input_tokens: 10,
                ..TokenUsage::default()
            },
        )
        .await;
    clock.0.store(140, Ordering::Relaxed);
    tracker
        .observe_event(
            Some(workflow.clone()),
            child,
            &EventMsg::TurnComplete(TurnCompleteEvent {
                turn_id: "turn-1".to_string(),
                last_agent_message: None,
                error: None,
                started_at: None,
                completed_at: None,
                duration_ms: None,
                time_to_first_token_ms: None,
            }),
        )
        .await;
    let metrics = workflow
        .get_fork_metrics(&fork_id)
        .await
        .expect("read metrics")
        .expect("metrics exist");
    assert_eq!(metrics.child_created_at_ms, Some(110));
    assert_eq!(metrics.first_event_at_ms, Some(120));
    assert_eq!(metrics.first_new_response_at_ms, Some(130));
    assert_eq!(metrics.completed_at_ms, Some(140));
    assert_eq!(metrics.provider_input_tokens, Some(100));
    assert_eq!(metrics.provider_cached_input_tokens, Some(60));
    assert_eq!(metrics.provider_uncached_input_tokens, Some(30));
    assert_eq!(metrics.provider_cache_write_input_tokens, Some(10));
    assert_eq!(
        workflow
            .list_fork_context(&fork_id)
            .await
            .expect("read fork context")
            .iter()
            .map(|entry| entry.origin)
            .collect::<Vec<_>>(),
        vec![
            WorkflowForkContextOrigin::InheritedHistory,
            WorkflowForkContextOrigin::NewOutput,
        ]
    );
    workflow.close().await;
}

#[tokio::test]
async fn full_history_warning_is_thresholded_and_claimed_once() {
    let workflow = WorkflowStore::open(&sqlite_config())
        .await
        .expect("open workflow store");
    let tracker = ForkMetricsTracker::new();
    let fork_id = tracker
        .spawn_requested(
            Some(workflow.clone()),
            ThreadId::new(),
            "spawn-call",
            WorkflowForkTurns::FullHistory,
        )
        .await;
    assert!(
        !tracker
            .claim_compaction_warning(Some(workflow.clone()), &fork_id, 89, 100)
            .await
    );
    assert!(
        tracker
            .claim_compaction_warning(Some(workflow.clone()), &fork_id, 90, 100)
            .await
    );
    assert!(
        !tracker
            .claim_compaction_warning(Some(workflow.clone()), &fork_id, 99, 100)
            .await
    );
    workflow.close().await;
}
