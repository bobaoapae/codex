use super::CompactedHistoryMetadata;
use super::tests::make_session_and_context;
use crate::context::ApprovedPlanRef;
use crate::context::CarriedPlan;
use crate::context::ContextualUserFragment;
use crate::context::PlanLoaded;
use codex_history::CompactedItem;
use codex_history::InitialHistory;
use codex_history::ResponseItemEnvelope;
use codex_history::ResumedHistory;
use codex_history::RolloutItem;
use codex_protocol::models::ContentItem;
use codex_protocol::models::ResponseItem;
use codex_protocol::plan_tool::PlanItemArg;
use codex_protocol::plan_tool::StepStatus;
use codex_protocol::plan_tool::UpdatePlanArgs;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::ThreadRolledBackEvent;
use codex_protocol::protocol::TurnCompleteEvent;
use codex_protocol::protocol::TurnStartedEvent;
use codex_protocol::protocol::UserMessageEvent;
use pretty_assertions::assert_eq;
use serde_json::json;
use std::path::PathBuf;
use std::sync::Arc;

fn user_message(message: &str) -> RolloutItem {
    RolloutItem::EventMsg(EventMsg::UserMessage(UserMessageEvent {
        client_id: None,
        message: message.to_string(),
        images: None,
        local_images: Vec::new(),
        text_elements: Vec::new(),
        ..Default::default()
    }))
}

fn turn_started(turn_id: &str) -> RolloutItem {
    RolloutItem::EventMsg(EventMsg::TurnStarted(TurnStartedEvent {
        turn_id: turn_id.to_string(),
        trace_id: None,
        started_at: None,
        model_context_window: None,
        collaboration_mode_kind: Default::default(),
    }))
}

fn turn_completed(turn_id: &str) -> RolloutItem {
    RolloutItem::EventMsg(EventMsg::TurnComplete(TurnCompleteEvent {
        turn_id: turn_id.to_string(),
        last_agent_message: None,
        error: None,
        started_at: None,
        completed_at: None,
        duration_ms: None,
        time_to_first_token_ms: None,
    }))
}

fn plan_update(step: &str) -> RolloutItem {
    RolloutItem::EventMsg(EventMsg::PlanUpdate(UpdatePlanArgs {
        explanation: None,
        plan: vec![PlanItemArg {
            step: step.to_string(),
            status: StepStatus::InProgress,
        }],
    }))
}

fn response_plan_item(plan: PlanLoaded) -> ResponseItemEnvelope {
    ResponseItemEnvelope::new(ContextualUserFragment::into(plan))
}

#[tokio::test]
async fn approved_plan_injection_is_ordered_and_compaction_keeps_one_fragment() {
    let (session, _turn_context) = make_session_and_context().await;
    session
        .inject_approved_plan("plan-1".to_string(), 3, "approved body".to_string())
        .await
        .expect("approved plan should be injected");

    let (window_number, window_ids) = session.advance_auto_compact_window().await;
    session
        .replace_compacted_history(
            vec![ResponseItemEnvelope::new(ResponseItem::Message {
                id: None,
                role: "user".to_string(),
                content: vec![ContentItem::InputText {
                    text: "compacted user".to_string(),
                }],
                phase: None,
                internal_chat_message_metadata_passthrough: None,
            })],
            None,
            None,
            CompactedHistoryMetadata {
                message: "summary".to_string(),
                window_number,
                window_ids,
                compaction_response_id: None,
                compaction_model_hash: None,
            },
        )
        .await;

    let history = session.clone_history().await;
    let plan_items = history
        .annotated_items()
        .iter()
        .filter(|item| PlanLoaded::is_response_item(&item.item))
        .count();
    assert_eq!(plan_items, 1);
    assert_eq!(
        session.approved_plan_ref(),
        Some(ApprovedPlanRef::new("plan-1", 3))
    );
}

#[tokio::test]
async fn approved_plan_injection_rejects_oversized_body_without_truncation() {
    let (session, _turn_context) = make_session_and_context().await;
    let body = "approved ".repeat(80_000);
    let error = session
        .inject_approved_plan("large".to_string(), 1, body)
        .await
        .expect_err("oversized approved plans must fail closed");

    assert!(error.to_string().contains("maximum is 10000"));
    assert_eq!(session.approved_plan_ref(), None);
}

#[tokio::test]
async fn reconstruction_uses_latest_surviving_plan_update_after_rollback() {
    let (session, turn_context) = make_session_and_context().await;
    let rollout_items = vec![
        turn_started("first"),
        user_message("first user"),
        plan_update("first plan"),
        turn_completed("first"),
        turn_started("second"),
        user_message("second user"),
        plan_update("second plan"),
        turn_completed("second"),
        RolloutItem::EventMsg(EventMsg::ThreadRolledBack(ThreadRolledBackEvent {
            num_turns: 1,
        })),
    ];

    let reconstruction = session
        .reconstruct_history_from_rollout(&turn_context, &rollout_items)
        .await;
    assert_eq!(
        serde_json::to_value(reconstruction.last_plan).expect("serialize reconstructed plan"),
        json!({
            "explanation": null,
            "plan": [{"step": "first plan", "status": "in_progress"}]
        })
    );
}

#[tokio::test]
async fn reconstruction_restores_plan_reference_and_checklist_after_compaction() {
    let (session, turn_context) = make_session_and_context().await;
    let checklist = UpdatePlanArgs {
        explanation: Some("keep going".to_string()),
        plan: vec![PlanItemArg {
            step: "verify".to_string(),
            status: StepStatus::Pending,
        }],
    };
    let loaded = PlanLoaded::new(ApprovedPlanRef::new("approved-7", 9), "body")
        .expect("approved plan should fit");
    let carried = CarriedPlan::new(&checklist).expect("checklist should fit");
    let rollout_items = vec![RolloutItem::Compacted(CompactedItem {
        message: "summary".to_string(),
        replacement_history: Some(vec![
            response_plan_item(loaded),
            ResponseItemEnvelope::new(ContextualUserFragment::into(carried)),
        ]),
        guardian_history: None,
        retained_context: None,
        mcp_resource_origins: None,
        window_number: Some(1),
        first_window_id: None,
        previous_window_id: None,
        window_id: None,
        compaction_response_id: None,
        latest_token_usage_record: None,
    })];

    let reconstruction = session
        .reconstruct_history_from_rollout(&turn_context, &rollout_items)
        .await;
    assert_eq!(
        reconstruction
            .approved_plan
            .as_ref()
            .map(|plan| plan.approved_plan().clone()),
        Some(ApprovedPlanRef::new("approved-7", 9))
    );
    assert_eq!(
        serde_json::to_value(reconstruction.last_plan).expect("serialize reconstructed checklist"),
        json!({
            "explanation": "keep going",
            "plan": [{"step": "verify", "status": "pending"}]
        })
    );
}

#[tokio::test]
async fn reconstruction_clears_stale_plan_state_when_history_has_no_plan() {
    let (session, _turn_context) = make_session_and_context().await;
    let plan = PlanLoaded::new(ApprovedPlanRef::new("stale", 1), "body").expect("plan should fit");
    session.set_approved_plan(Some(plan));
    session.set_last_plan(Some(UpdatePlanArgs {
        explanation: None,
        plan: vec![PlanItemArg {
            step: "stale".to_string(),
            status: StepStatus::Completed,
        }],
    }));

    session
        .record_initial_history(InitialHistory::Resumed(ResumedHistory {
            conversation_id: session.thread_id(),
            history: Arc::new(Vec::new()),
            rollout_path: Some(PathBuf::from("/tmp/empty-plan-history.jsonl")),
        }))
        .await;

    assert_eq!(session.approved_plan_ref(), None);
    assert!(session.last_plan().is_none());
}
