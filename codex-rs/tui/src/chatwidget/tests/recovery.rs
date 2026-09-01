//! FORK: `/recover` popup and stale-response coverage.

use super::*;
use crate::app_event::ThreadRecoveryPreviewResult;
use codex_app_server_protocol::ThreadRecoveryCounts;
use codex_app_server_protocol::ThreadRecoveryExcludedItem;
use codex_app_server_protocol::ThreadRecoveryPreviewResponse;
use codex_app_server_protocol::ThreadRecoveryWatermark;

fn preview_response(thread_id: ThreadId, token: Option<&str>) -> ThreadRecoveryPreviewResult {
    ThreadRecoveryPreviewResult {
        token: token.map(str::to_string),
        response: ThreadRecoveryPreviewResponse {
            token: None,
            thread_id: thread_id.to_string(),
            source_rollout_id: "rollout-1".to_string(),
            source_model_provider: Some("chatgpt_web".to_string()),
            watermark: ThreadRecoveryWatermark {
                rollout_id: "rollout-1".to_string(),
                end_ordinal_exclusive: 12,
                end_byte_offset: 4096,
            },
            source_item_count: 12,
            source_serialized_bytes: 4096,
            retained_item_count: 10,
            retained_serialized_bytes: 3000,
            excluded_items: vec![ThreadRecoveryExcludedItem {
                rollout_ordinal: 7,
                item_id: Some("item-7".to_string()),
                turn_id: Some("turn-2".to_string()),
                reason: "provider-local plaintext was not readable as ciphertext".to_string(),
            }],
            counts: ThreadRecoveryCounts {
                total_items: 12,
                retained_items: 10,
                excluded_items: 2,
                failed_turns: 1,
            },
            can_recover: token.is_some(),
            reason: None,
            blocked_reason: None,
        },
    }
}

#[tokio::test]
async fn recover_slash_command_starts_preview_for_idle_root() {
    let (mut chat, mut rx, _op_rx) = make_chatwidget_manual(/*model_override*/ None).await;
    let thread_id =
        ThreadId::from_string("019c2d47-4935-7423-a190-05691f566092").expect("thread id");
    chat.thread_id = Some(thread_id);

    chat.dispatch_command(SlashCommand::Recover);

    assert!(matches!(rx.try_recv(), Ok(AppEvent::OpenRecovery)));
}

#[tokio::test]
async fn recover_slash_command_is_blocked_for_busy_side_and_parent_owned_threads() {
    let (mut chat, mut rx, _op_rx) = make_chatwidget_manual(/*model_override*/ None).await;
    chat.thread_id = Some(ThreadId::new());
    chat.on_task_started();
    chat.dispatch_command(SlashCommand::Recover);
    assert!(!matches!(rx.try_recv(), Ok(AppEvent::OpenRecovery)));

    let (mut chat, mut rx, _op_rx) = make_chatwidget_manual(/*model_override*/ None).await;
    chat.thread_id = Some(ThreadId::new());
    chat.set_side_conversation_active(/*active*/ true);
    chat.dispatch_command(SlashCommand::Recover);
    assert!(!matches!(rx.try_recv(), Ok(AppEvent::OpenRecovery)));

    let (mut chat, mut rx, _op_rx) = make_chatwidget_manual(/*model_override*/ None).await;
    chat.thread_id = Some(ThreadId::new());
    chat.set_parent_owned_thread();
    chat.dispatch_command(SlashCommand::Recover);
    assert!(!matches!(rx.try_recv(), Ok(AppEvent::OpenRecovery)));
}

#[tokio::test]
async fn recovery_preview_popup_is_bounded_and_requires_explicit_acceptance() {
    let (mut chat, mut rx, _op_rx) = make_chatwidget_manual(/*model_override*/ None).await;
    let thread_id =
        ThreadId::from_string("019c2d47-4935-7423-a190-05691f566092").expect("thread id");
    chat.thread_id = Some(thread_id);
    let request_id = chat.begin_recovery_preview(thread_id);
    chat.apply_recovery_preview(
        request_id,
        Ok(preview_response(thread_id, Some("secret-token"))),
    );

    let popup = render_bottom_popup(&chat, /*width*/ 80);
    insta::assert_snapshot!(&popup, @r#"
  Recovery preview
  Immutable preview for 019c2d47-4935-7423-a190-05691f566092

› 1. Create replacement lineage  Retain 10 of 12 items; exclude 2 items across
                                 1 failed turns.

  Press enter to confirm or esc to go back
"#);
    assert!(!popup.contains("secret-token"));

    chat.handle_key_event(KeyEvent::from(KeyCode::Enter));
    assert!(matches!(
        rx.try_recv(),
        Ok(AppEvent::CreateThreadRecovery { request_id: id }) if id == request_id
    ));
}

#[tokio::test]
async fn cancelling_recovery_preview_discards_token_without_creating_lineage() {
    let (mut chat, mut rx, _op_rx) = make_chatwidget_manual(/*model_override*/ None).await;
    let thread_id = ThreadId::new();
    chat.thread_id = Some(thread_id);
    let request_id = chat.begin_recovery_preview(thread_id);
    chat.apply_recovery_preview(
        request_id,
        Ok(preview_response(thread_id, Some("cancelled-token"))),
    );

    chat.handle_key_event(KeyEvent::from(KeyCode::Esc));

    assert!(chat.no_modal_or_popup_active());
    assert!(matches!(
        rx.try_recv(),
        Ok(AppEvent::CancelThreadRecovery { request_id: id }) if id == request_id
    ));
    assert!(chat.recovery.token.is_none());
    assert!(chat.begin_recovery_create(request_id).is_none());
}

#[tokio::test]
async fn stale_recovery_preview_does_not_open_a_popup_or_keep_a_token() {
    let (mut chat, mut rx, _op_rx) = make_chatwidget_manual(/*model_override*/ None).await;
    let thread_id = ThreadId::new();
    chat.thread_id = Some(thread_id);
    let first = chat.begin_recovery_preview(thread_id);
    let second = chat.begin_recovery_preview(thread_id);

    chat.apply_recovery_preview(first, Ok(preview_response(thread_id, Some("stale-token"))));
    assert!(chat.no_modal_or_popup_active());
    assert!(rx.try_recv().is_err());

    chat.apply_recovery_preview(
        second,
        Ok(preview_response(thread_id, Some("current-token"))),
    );
    assert!(!chat.no_modal_or_popup_active());
}
