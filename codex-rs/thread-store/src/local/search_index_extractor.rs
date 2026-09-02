//! Privacy-preserving extraction for the local full-text search projection.
//!
//! Rollouts are the source of truth.  This module deliberately accepts only
//! user text, terminal assistant text, compaction summaries, and explicitly
//! represented plan/receipt metadata.  It never serializes a whole rollout
//! item into the index: doing so would make tool arguments, command output,
//! reasoning, and encrypted payloads searchable by accident.

use std::cmp::Ordering;

use codex_protocol::items::TurnItem;
use codex_protocol::models::ContentItem;
use codex_protocol::models::MessagePhase;
use codex_protocol::models::ResponseItem;
use codex_protocol::protocol::EventMsg;
use codex_rollout::RolloutItem;
use codex_state::SearchSourceKind;
use serde_json::Value;

const MAX_CONTENT_BYTES: usize = 1_000_000;
const DEDUPE_ORDINAL_DISTANCE: u64 = 16;
const MAX_RECEIPT_TAGS: usize = 32;
const MAX_RECEIPT_FIELD_BYTES: usize = 256;

/// One bounded candidate extracted from a rollout line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct IndexedCandidate {
    pub source_kind: SearchSourceKind,
    /// Identity local to the physical rollout.  The projector prefixes it
    /// with the rollout id before sending it to WorkflowStore.
    pub source_key: String,
    pub ordinal: u64,
    pub content: String,
    pub event_time_ms: Option<i64>,
    pub turn_id: Option<String>,
    /// Higher priority wins when the same message was persisted both as a
    /// raw Responses item and as an ItemCompleted/TurnComplete item.
    priority: u8,
}

/// A decoded rollout record used by the pure extractor.
#[derive(Debug, Clone)]
pub(crate) struct ExtractRecord {
    pub ordinal: u64,
    pub event_time_ms: Option<i64>,
    pub item: RolloutItem,
}

/// Extract and deduplicate the allowlisted text classes from decoded records.
pub(crate) fn extract_candidates(
    records: impl IntoIterator<Item = ExtractRecord>,
) -> Vec<IndexedCandidate> {
    let mut candidates = Vec::new();
    for record in records {
        candidates.extend(extract_item_candidates(
            &record.item,
            record.ordinal,
            record.event_time_ms,
        ));
    }
    deduplicate_candidates(candidates)
}

/// Extract the allowlisted content from one typed rollout item.
pub(crate) fn extract_item_candidates(
    item: &RolloutItem,
    ordinal: u64,
    event_time_ms: Option<i64>,
) -> Vec<IndexedCandidate> {
    let mut candidates = Vec::new();
    match item {
        RolloutItem::ResponseItem(envelope) => {
            let ResponseItem::Message {
                id,
                role,
                content,
                phase,
                internal_chat_message_metadata_passthrough,
            } = &envelope.item
            else {
                return candidates;
            };
            let is_user = role.eq_ignore_ascii_case("user");
            let is_assistant = role.eq_ignore_ascii_case("assistant");
            let text = if is_user {
                collect_input_text(content)
            } else if is_assistant && *phase == Some(MessagePhase::FinalAnswer) {
                collect_output_text(content)
            } else {
                None
            };
            let Some(content) = bounded_content(text) else {
                return candidates;
            };
            let turn_id = internal_chat_message_metadata_passthrough
                .as_ref()
                .and_then(|metadata| metadata.turn_id.clone());
            let key = turn_id
                .as_deref()
                .map(|turn_id| format!("turn:{turn_id}"))
                .or_else(|| id.as_ref().map(|id| format!("item:{id}")))
                .unwrap_or_else(|| format!("ordinal:{ordinal}"));
            candidates.push(IndexedCandidate {
                source_kind: if is_user {
                    SearchSourceKind::User
                } else {
                    SearchSourceKind::FinalAssistant
                },
                source_key: key,
                ordinal,
                content,
                event_time_ms,
                turn_id,
                priority: 1,
            });
        }
        RolloutItem::Compacted(compacted) => {
            if let Some(content) = bounded_content(Some(compacted.message.clone())) {
                candidates.push(IndexedCandidate {
                    source_kind: SearchSourceKind::CompactionSummary,
                    source_key: format!("ordinal:{ordinal}"),
                    ordinal,
                    content,
                    event_time_ms,
                    turn_id: None,
                    priority: 1,
                });
            }
        }
        RolloutItem::EventMsg(event) => match event {
            EventMsg::UserMessage(event) => {
                if let Some(content) = bounded_content(Some(event.message.clone())) {
                    let source_key = event
                        .client_id
                        .as_deref()
                        .filter(|client_id| !client_id.is_empty())
                        .map_or_else(
                            || format!("ordinal:{ordinal}"),
                            |client_id| format!("client:{client_id}"),
                        );
                    candidates.push(IndexedCandidate {
                        source_kind: SearchSourceKind::User,
                        source_key,
                        ordinal,
                        content,
                        event_time_ms,
                        turn_id: None,
                        priority: 2,
                    });
                }
            }
            EventMsg::AgentMessage(event) => {
                if event.phase == Some(MessagePhase::FinalAnswer)
                    && let Some(content) = bounded_content(Some(event.message.clone()))
                {
                    candidates.push(IndexedCandidate {
                        source_kind: SearchSourceKind::FinalAssistant,
                        source_key: format!("ordinal:{ordinal}"),
                        ordinal,
                        content,
                        event_time_ms,
                        turn_id: None,
                        priority: 1,
                    });
                }
            }
            EventMsg::TurnComplete(event) => {
                if let Some(content) = bounded_content(event.last_agent_message.clone()) {
                    candidates.push(IndexedCandidate {
                        source_kind: SearchSourceKind::FinalAssistant,
                        source_key: format!("turn:{}", event.turn_id),
                        ordinal,
                        content,
                        event_time_ms,
                        turn_id: Some(event.turn_id.clone()),
                        priority: 3,
                    });
                }
            }
            EventMsg::ItemCompleted(event) => match &event.item {
                TurnItem::UserMessage(item) => {
                    if let Some(content) = bounded_content(Some(item.message())) {
                        candidates.push(IndexedCandidate {
                            source_kind: SearchSourceKind::User,
                            source_key: format!("item:{}", item.id),
                            ordinal,
                            content,
                            event_time_ms,
                            turn_id: Some(event.turn_id.clone()),
                            priority: 2,
                        });
                    }
                }
                TurnItem::AgentMessage(item) => {
                    if item.phase == Some(MessagePhase::FinalAnswer)
                        && let Some(content) = bounded_content(Some(
                            item.content
                                .iter()
                                .map(|content| match content {
                                    codex_protocol::items::AgentMessageContent::Text { text } => {
                                        text.as_str()
                                    }
                                })
                                .collect::<Vec<_>>()
                                .join("\n"),
                        ))
                    {
                        candidates.push(IndexedCandidate {
                            source_kind: SearchSourceKind::FinalAssistant,
                            source_key: format!("turn:{}", event.turn_id),
                            ordinal,
                            content,
                            event_time_ms,
                            turn_id: Some(event.turn_id.clone()),
                            priority: 2,
                        });
                    }
                }
                // A PlanItem is the only typed persisted representation of a
                // plan in older rollouts.  It is safe to index its text, while
                // PlanUpdate remains progress metadata and is intentionally
                // excluded.
                TurnItem::Plan(item) => {
                    if let Some(content) = bounded_content(Some(item.text.clone())) {
                        candidates.push(IndexedCandidate {
                            source_kind: SearchSourceKind::ApprovedPlan,
                            source_key: format!("plan:{}", item.id),
                            ordinal,
                            content,
                            event_time_ms,
                            turn_id: Some(event.turn_id.clone()),
                            priority: 1,
                        });
                    }
                }
                _ => {}
            },
            _ => {}
        },
        // AgentMessage, Reasoning, function/tool calls and all other raw
        // Responses items are deliberately not searchable.  In particular,
        // encrypted agent content must never be treated as text.
        RolloutItem::SessionMeta(_)
        | RolloutItem::InterAgentCommunication(_)
        | RolloutItem::InterAgentCommunicationMetadata { .. }
        | RolloutItem::TurnContext(_)
        | RolloutItem::WorldState(_)
        | RolloutItem::SecurityRiskScore(_)
        | RolloutItem::RealtimeItem(_)
        | RolloutItem::RetainedContext(_)
        | RolloutItem::TokenUsageRecord(_) => {}
    }
    candidates
}

/// Extract metadata from a future/unknown `receipt.attached` extension item.
///
/// This is intentionally performed on the raw line before typed decoding so
/// an older binary can still index a bounded receipt representation introduced
/// by a newer extension.  Only the public metadata fields are copied; payload
/// and arbitrary nested objects are never inspected.
pub(crate) fn extract_receipt_candidate(
    value: &Value,
    ordinal: u64,
    event_time_ms: Option<i64>,
) -> Option<IndexedCandidate> {
    let payload = value.get("payload")?;
    if !value
        .get("type")
        .and_then(Value::as_str)
        .is_some_and(|kind| kind.eq_ignore_ascii_case("event_msg"))
        || !payload
            .get("type")
            .and_then(Value::as_str)
            .is_some_and(|kind| kind.eq_ignore_ascii_case("item_completed"))
    {
        return None;
    }
    let item = payload.get("item")?;
    if !item
        .get("type")
        .and_then(Value::as_str)
        .is_some_and(|kind| kind.eq_ignore_ascii_case("extension"))
        || item.get("kind").and_then(Value::as_str) != Some("receipt.attached")
    {
        return None;
    }
    let subject = bounded_field(item.get("subject").and_then(Value::as_str))?;
    let status = bounded_field(item.get("status").and_then(Value::as_str))?;
    let source = bounded_field(item.get("source").and_then(Value::as_str))?;
    let mut words = vec!["receipt.attached".to_string(), subject, status, source];
    if let Some(tags) = item.get("tags").and_then(Value::as_object) {
        for (index, (key, value)) in tags.iter().enumerate() {
            if index >= MAX_RECEIPT_TAGS {
                break;
            }
            let Some(key) = bounded_field(Some(key)) else {
                continue;
            };
            let Some(value) = value.as_str().and_then(|value| bounded_field(Some(value))) else {
                continue;
            };
            words.push(format!("{key}={value}"));
        }
    }
    let content = bounded_content(Some(words.join(" ")))?;
    let source_key = item
        .get("receiptId")
        .or_else(|| item.get("receipt_id"))
        .and_then(Value::as_str)
        .and_then(|id| bounded_field(Some(id)))
        .map_or_else(
            || format!("ordinal:{ordinal}"),
            |id| format!("receipt:{id}"),
        );
    Some(IndexedCandidate {
        source_kind: SearchSourceKind::ReceiptMetadata,
        source_key,
        ordinal,
        content,
        event_time_ms,
        turn_id: payload
            .get("turn_id")
            .and_then(Value::as_str)
            .map(str::to_string),
        priority: 1,
    })
}

fn collect_input_text(content: &[ContentItem]) -> Option<String> {
    let text = content
        .iter()
        .filter_map(|content| match content {
            ContentItem::InputText { text } => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n");
    (!text.trim().is_empty()).then_some(text)
}

fn collect_output_text(content: &[ContentItem]) -> Option<String> {
    let text = content
        .iter()
        .filter_map(|content| match content {
            ContentItem::OutputText { text } => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n");
    (!text.trim().is_empty()).then_some(text)
}

fn bounded_content(content: Option<String>) -> Option<String> {
    let content = content?.replace('\0', " ");
    let content = content.trim();
    if content.is_empty() {
        return None;
    }
    Some(truncate_utf8(content, MAX_CONTENT_BYTES).to_string())
}

fn bounded_field(value: Option<&str>) -> Option<String> {
    let value = value?.trim();
    (!value.is_empty()).then(|| truncate_utf8(value, MAX_RECEIPT_FIELD_BYTES).to_string())
}

fn truncate_utf8(value: &str, max_bytes: usize) -> &str {
    if value.len() <= max_bytes {
        return value;
    }
    let mut end = max_bytes;
    while end > 0 && !value.is_char_boundary(end) {
        end -= 1;
    }
    &value[..end]
}

pub(crate) fn deduplicate_candidates(
    mut candidates: Vec<IndexedCandidate>,
) -> Vec<IndexedCandidate> {
    candidates.sort_by(|left, right| {
        left.ordinal
            .cmp(&right.ordinal)
            .then_with(|| left.source_kind.as_str().cmp(right.source_kind.as_str()))
            .then_with(|| left.source_key.cmp(&right.source_key))
    });
    let mut selected = Vec::with_capacity(candidates.len());
    for candidate in candidates {
        let duplicate = selected.iter().position(|existing: &IndexedCandidate| {
            existing.source_kind == candidate.source_kind
                && (existing.source_key == candidate.source_key
                    || (existing.content == candidate.content
                        && existing.ordinal.abs_diff(candidate.ordinal) <= DEDUPE_ORDINAL_DISTANCE))
        });
        if let Some(index) = duplicate {
            if candidate.priority > selected[index].priority {
                selected[index] = candidate;
            }
        } else {
            selected.push(candidate);
        }
    }
    selected.sort_by(|left, right| {
        left.ordinal
            .cmp(&right.ordinal)
            .then_with(|| compare_source_kind(left.source_kind, right.source_kind))
            .then_with(|| left.source_key.cmp(&right.source_key))
    });
    selected
}

fn compare_source_kind(left: SearchSourceKind, right: SearchSourceKind) -> Ordering {
    left.as_str().cmp(right.as_str())
}

#[cfg(test)]
#[path = "search_index_extractor_tests.rs"]
mod tests;
