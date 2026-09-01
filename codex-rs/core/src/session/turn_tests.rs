use super::*;
use codex_extension_api::ExtensionData;
use codex_extension_api::TurnItemContributor;
use codex_history::RolloutItem;
use codex_protocol::ResponseItemId;
use codex_protocol::error::HISTORY_RECOVERY_REQUIRED_ERROR_MESSAGE;
use codex_protocol::items::AgentMessageContent;
use codex_protocol::protocol::CodexErrorInfo;
use codex_protocol::protocol::ErrorEvent;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::TurnCompleteEvent;
use pretty_assertions::assert_eq;
use std::sync::Arc;
use tracing_subscriber::prelude::*;

#[test]
fn history_recovery_marker_is_recovered_from_terminal_turn_event() {
    let marker = RolloutItem::EventMsg(EventMsg::TurnComplete(TurnCompleteEvent {
        turn_id: "turn-1".to_string(),
        last_agent_message: None,
        error: Some(ErrorEvent {
            message: HISTORY_RECOVERY_REQUIRED_ERROR_MESSAGE.to_string(),
            codex_error_info: Some(CodexErrorInfo::Other),
            misalignment: None,
        }),
        started_at: None,
        completed_at: None,
        duration_ms: None,
        time_to_first_token_ms: None,
    }));
    let unrelated = RolloutItem::EventMsg(EventMsg::TurnComplete(TurnCompleteEvent {
        turn_id: "turn-2".to_string(),
        last_agent_message: Some("done".to_string()),
        error: None,
        started_at: None,
        completed_at: None,
        duration_ms: None,
        time_to_first_token_ms: None,
    }));

    assert_eq!(
        history_recovery_reason_from_rollout(&[unrelated, marker]),
        Some(codex_protocol::error::HistoryRecoveryReason::UndecryptableEncryptedFunctionOutput)
    );
}

#[test]
fn history_recovery_marker_ignores_unrelated_terminal_errors() {
    let unrelated = RolloutItem::EventMsg(EventMsg::TurnComplete(TurnCompleteEvent {
        turn_id: "turn-1".to_string(),
        last_agent_message: None,
        error: Some(ErrorEvent {
            message: "stream disconnected before completion: network reset".to_string(),
            codex_error_info: Some(CodexErrorInfo::Other),
            misalignment: None,
        }),
        started_at: None,
        completed_at: None,
        duration_ms: None,
        time_to_first_token_ms: None,
    }));

    assert_eq!(history_recovery_reason_from_rollout(&[unrelated]), None);
}

struct RewriteAgentMessageContributor;

impl TurnItemContributor for RewriteAgentMessageContributor {
    fn contribute<'a>(
        &'a self,
        _thread_store: &'a ExtensionData,
        _turn_store: &'a ExtensionData,
        item: &'a mut TurnItem,
    ) -> codex_extension_api::ExtensionFuture<'a, Result<(), String>> {
        Box::pin(async move {
            if let TurnItem::AgentMessage(agent_message) = item {
                agent_message.content = vec![AgentMessageContent::Text {
                    text: "plan contributed assistant text".to_string(),
                }];
            }
            Ok(())
        })
    }
}

fn assistant_output_text(text: &str) -> ResponseItem {
    ResponseItem::Message {
        id: Some(ResponseItemId::with_suffix("msg", "1")),
        role: "assistant".to_string(),
        content: vec![ContentItem::OutputText {
            text: text.to_string(),
        }],
        phase: None,
        internal_chat_message_metadata_passthrough: None,
    }
}

#[test]
fn post_sampling_token_estimate_is_disabled_by_always_on_sinks() {
    let feedback = codex_feedback::CodexFeedback::new();
    let subscriber = tracing_subscriber::registry()
        .with(feedback.logger_layer())
        .with(tracing_subscriber::fmt::layer().with_filter(codex_state::log_db::default_filter()));

    tracing::subscriber::with_default(subscriber, || {
        tracing::callsite::rebuild_interest_cache();
        assert!(!tracing::event_enabled!(
            target: POST_SAMPLING_TOKEN_ESTIMATE_TARGET,
            tracing::Level::TRACE,
            turn_id,
            estimated_token_count,
            message
        ));
    });
}

#[tokio::test]
async fn fork_invariant_plan_mode_uses_contributed_turn_item_for_last_agent_message() {
    let (mut session, turn_context) = crate::session::tests::make_session_and_context().await;
    let mut builder = codex_extension_api::ExtensionRegistryBuilder::new();
    builder.turn_item_contributor(Arc::new(RewriteAgentMessageContributor));
    session.services.extensions = Arc::new(builder.build());
    let turn_store = ExtensionData::new(turn_context.sub_id.clone());
    let mut state = PlanModeStreamState::new(&turn_context.sub_id);
    let mut last_agent_message = None;
    let item = assistant_output_text("original assistant text");

    let handled = handle_assistant_item_done_in_plan_mode(
        &session,
        &turn_context,
        &turn_store,
        &item,
        &mut state,
        /*previously_active_item*/ None,
        &mut last_agent_message,
    )
    .await;

    assert!(handled);
    assert_eq!(
        last_agent_message.as_deref(),
        Some("plan contributed assistant text")
    );
}

/// FORK: the Plan-mode reminder is recorded once per turn and only in Plan mode.
#[tokio::test]
async fn fork_invariant_plan_mode_reminder_is_recorded_once_per_turn() {
    use crate::session::plan_reminder::maybe_record_plan_mode_reminder;
    use crate::session::step_context::StepContext;
    use crate::session::tests::update_selected_settings_for_test;
    use codex_protocol::config_types::ModeKind;

    let (session, turn_context) = crate::session::tests::make_session_and_context().await;
    let mut turn_context = turn_context;
    crate::session::tests::update_turn_settings_for_test(&mut turn_context, |settings| {
        update_selected_settings_for_test(settings, |selected| {
            selected.collaboration_mode.mode = ModeKind::Plan;
        });
    });
    let turn_context = Arc::new(turn_context);
    let step_context = StepContext::for_test(Arc::clone(&turn_context));

    let mut recorded = false;
    maybe_record_plan_mode_reminder(&session, &turn_context, &step_context, &mut recorded).await;
    assert!(recorded);
    // A second model step within the same turn must not add another reminder.
    maybe_record_plan_mode_reminder(&session, &turn_context, &step_context, &mut recorded).await;

    assert_eq!(count_plan_mode_reminders(&session).await, 1);
}

#[tokio::test]
async fn fork_invariant_plan_mode_reminder_is_not_recorded_outside_plan_mode() {
    use crate::session::plan_reminder::maybe_record_plan_mode_reminder;
    use crate::session::step_context::StepContext;
    use codex_protocol::config_types::ModeKind;

    let (session, turn_context) = crate::session::tests::make_session_and_context().await;
    let turn_context = Arc::new(turn_context);
    assert_eq!(
        turn_context
            .initial_settings
            .selected()
            .collaboration_mode
            .mode,
        ModeKind::Default
    );
    let step_context = StepContext::for_test(Arc::clone(&turn_context));

    let mut recorded = false;
    maybe_record_plan_mode_reminder(&session, &turn_context, &step_context, &mut recorded).await;

    assert!(!recorded);
    assert_eq!(count_plan_mode_reminders(&session).await, 0);
}

async fn count_plan_mode_reminders(session: &Session) -> usize {
    use crate::context::ContextualUserFragment;
    use crate::context::PlanModeReminder;

    session
        .clone_history()
        .await
        .into_raw_items()
        .iter()
        .filter(|item| match item {
            ResponseItem::Message { content, .. } => content.iter().any(|content_item| {
                matches!(content_item, ContentItem::InputText { text } if PlanModeReminder::matches_text(text))
            }),
            _ => false,
        })
        .count()
}
