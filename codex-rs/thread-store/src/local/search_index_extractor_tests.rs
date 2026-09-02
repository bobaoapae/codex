use super::*;

use codex_protocol::ResponseItemId;
use codex_protocol::items::AgentMessageContent;
use codex_protocol::items::AgentMessageItem;
use codex_protocol::items::PlanItem;
use codex_protocol::items::TurnItem;
use codex_protocol::items::UserMessageItem;
use codex_protocol::models::AgentMessageInputContent;
use codex_protocol::models::ContentItem;
use codex_protocol::models::MessagePhase;
use codex_protocol::models::ResponseItem;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::ItemCompletedEvent;
use codex_protocol::protocol::TurnCompleteEvent;
use codex_protocol::protocol::UserMessageEvent;
use codex_protocol::user_input::UserInput;
use codex_rollout::CompactedItem;
use codex_rollout::ResponseItemEnvelope;
use codex_rollout::RolloutItem;
use codex_state::SearchSourceKind;
use serde_json::json;

fn record(ordinal: u64, item: RolloutItem) -> ExtractRecord {
    ExtractRecord {
        ordinal,
        event_time_ms: Some(ordinal as i64),
        item,
    }
}

fn user_event(message: &str) -> RolloutItem {
    RolloutItem::EventMsg(EventMsg::UserMessage(UserMessageEvent {
        message: message.to_string(),
        ..Default::default()
    }))
}

#[test]
fn extraction_is_allowlisted_and_never_includes_encrypted_or_tool_content() {
    let records = vec![
        record(1, user_event("find this user text")),
        record(
            2,
            RolloutItem::EventMsg(EventMsg::TurnComplete(TurnCompleteEvent {
                turn_id: "turn-1".to_string(),
                last_agent_message: Some("find this final answer".to_string()),
                error: None,
                started_at: None,
                completed_at: None,
                duration_ms: None,
                time_to_first_token_ms: None,
            })),
        ),
        record(
            3,
            RolloutItem::ResponseItem(ResponseItemEnvelope::new(ResponseItem::AgentMessage {
                id: Some(ResponseItemId::from_server("encrypted".to_string())),
                author: "agent".to_string(),
                recipient: "parent".to_string(),
                content: vec![AgentMessageInputContent::EncryptedContent {
                    encrypted_content: "secret ciphertext".to_string(),
                }],
                internal_chat_message_metadata_passthrough: None,
            })),
        ),
        record(
            4,
            RolloutItem::EventMsg(EventMsg::AgentReasoning(
                codex_protocol::protocol::AgentReasoningEvent {
                    text: "reasoning must not be indexed".to_string(),
                },
            )),
        ),
    ];

    let candidates = extract_candidates(records);
    assert_eq!(candidates.len(), 2);
    assert_eq!(candidates[0].source_kind, SearchSourceKind::User);
    assert_eq!(candidates[0].content, "find this user text");
    assert_eq!(candidates[1].source_kind, SearchSourceKind::FinalAssistant);
    assert_eq!(candidates[1].content, "find this final answer");
    assert!(
        candidates
            .iter()
            .all(|candidate| !candidate.content.contains("secret")
                && !candidate.content.contains("reasoning"))
    );
}

#[test]
fn response_item_and_item_completed_are_deduplicated_with_turn_complete_precedence() {
    let response = RolloutItem::ResponseItem(ResponseItemEnvelope {
        item: ResponseItem::Message {
            id: Some(ResponseItemId::from_server("response-1".to_string())),
            role: "assistant".to_string(),
            content: vec![ContentItem::OutputText {
                text: "same final answer".to_string(),
            }],
            phase: Some(MessagePhase::FinalAnswer),
            internal_chat_message_metadata_passthrough: Some(
                codex_protocol::models::InternalChatMessageMetadataPassthrough {
                    turn_id: Some("turn-1".to_string()),
                    ..Default::default()
                },
            ),
        },
        metadata: None,
    });
    let completed = RolloutItem::EventMsg(EventMsg::ItemCompleted(ItemCompletedEvent {
        thread_id: Default::default(),
        turn_id: "turn-1".to_string(),
        item: TurnItem::AgentMessage(AgentMessageItem {
            id: "item-1".to_string(),
            content: vec![AgentMessageContent::Text {
                text: "same final answer".to_string(),
            }],
            phase: Some(MessagePhase::FinalAnswer),
            memory_citation: None,
            delivery: None,
            questions: None,
        }),
        started_at_ms: None,
        completed_at_ms: 3,
    }));
    let complete = RolloutItem::EventMsg(EventMsg::TurnComplete(TurnCompleteEvent {
        turn_id: "turn-1".to_string(),
        last_agent_message: Some("same final answer".to_string()),
        error: None,
        started_at: None,
        completed_at: None,
        duration_ms: None,
        time_to_first_token_ms: None,
    }));

    let candidates = extract_candidates([
        record(1, response),
        record(2, completed),
        record(3, complete),
    ]);
    assert_eq!(candidates.len(), 1);
    assert_eq!(candidates[0].source_kind, SearchSourceKind::FinalAssistant);
    assert_eq!(candidates[0].source_key, "turn:turn-1");
    assert_eq!(candidates[0].content, "same final answer");
}

#[test]
fn compacted_message_and_plan_text_are_safe_representations() {
    let compacted = RolloutItem::Compacted(CompactedItem {
        message: "compact summary".to_string(),
        replacement_history: Some(vec![ResponseItemEnvelope::new(
            ResponseItem::AgentMessage {
                id: None,
                author: "agent".to_string(),
                recipient: "parent".to_string(),
                content: vec![AgentMessageInputContent::EncryptedContent {
                    encrypted_content: "replacement ciphertext".to_string(),
                }],
                internal_chat_message_metadata_passthrough: None,
            },
        )]),
        guardian_history: None,
        retained_context: None,
        mcp_resource_origins: None,
        window_number: None,
        first_window_id: None,
        previous_window_id: None,
        window_id: None,
        compaction_response_id: None,
        latest_token_usage_record: None,
    });
    let plan = RolloutItem::EventMsg(EventMsg::ItemCompleted(ItemCompletedEvent {
        thread_id: Default::default(),
        turn_id: "turn-plan".to_string(),
        item: TurnItem::Plan(PlanItem {
            id: "plan-1".to_string(),
            text: "approved plan text".to_string(),
        }),
        started_at_ms: None,
        completed_at_ms: 2,
    }));

    let candidates = extract_candidates([record(1, compacted), record(2, plan)]);
    assert_eq!(candidates.len(), 2);
    assert_eq!(
        candidates[0].source_kind,
        SearchSourceKind::CompactionSummary
    );
    assert_eq!(candidates[0].content, "compact summary");
    assert_eq!(candidates[1].source_kind, SearchSourceKind::ApprovedPlan);
    assert_eq!(candidates[1].content, "approved plan text");
    assert!(
        candidates
            .iter()
            .all(|candidate| !candidate.content.contains("ciphertext"))
    );
}

#[test]
fn receipt_metadata_extracts_only_bounded_public_fields() {
    let value = json!({
        "type": "event_msg",
        "payload": {
            "type": "item_completed",
            "turn_id": "turn-receipt",
            "item": {
                "type": "Extension",
                "kind": "receipt.attached",
                "receiptId": "receipt-1",
                "subject": "test suite",
                "status": "pass",
                "source": "hook",
                "tags": {"platform": "windows", "secret": "not payload"},
                "payload": {"should": "never be indexed"}
            }
        }
    });
    let candidate = extract_receipt_candidate(&value, 7, Some(7)).expect("receipt candidate");
    assert_eq!(candidate.source_kind, SearchSourceKind::ReceiptMetadata);
    assert_eq!(candidate.source_key, "receipt:receipt-1");
    assert!(candidate.content.contains("test suite"));
    assert!(candidate.content.contains("platform=windows"));
    assert!(!candidate.content.contains("should"));
}

#[test]
fn user_message_item_extracts_text_without_attachments() {
    let item = RolloutItem::EventMsg(EventMsg::ItemCompleted(ItemCompletedEvent {
        thread_id: Default::default(),
        turn_id: "turn-user".to_string(),
        item: TurnItem::UserMessage(UserMessageItem::new(&[
            UserInput::Text {
                text: "text only".to_string(),
                text_elements: Vec::new(),
            },
            UserInput::Image {
                image_url: "data:image/png;base64,private".to_string(),
                detail: None,
            },
        ])),
        started_at_ms: None,
        completed_at_ms: 2,
    }));
    let candidates = extract_candidates([record(1, item)]);
    assert_eq!(candidates.len(), 1);
    assert_eq!(candidates[0].content, "text only");
    assert!(!candidates[0].content.contains("data:image"));
}
