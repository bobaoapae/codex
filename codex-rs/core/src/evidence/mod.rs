//! Host-side receipt projection for canonical tool and turn lifecycle events.
//!
//! Receipts are deliberately model-independent. This module only copies
//! bounded identity/status metadata and references to canonical items; it
//! never copies command arguments, tool payloads, or command output.

use std::collections::HashSet;

use chrono::SecondsFormat;
use chrono::Utc;
use codex_extension_items::ExtensionItem;
use codex_extension_items::receipt::ReceiptAttachedItem;
use codex_extension_items::receipt::ReceiptReference;
use codex_extension_items::receipt::ReceiptStatus;
use codex_history::InitialHistory;
use codex_history::RolloutItem;
use codex_hooks::PostToolUseEvidence;
use codex_hooks::PostToolUseEvidenceStatus;
use codex_protocol::ThreadId;
use codex_protocol::items::CollabAgentToolCallStatus;
use codex_protocol::items::CommandExecutionStatus;
use codex_protocol::items::DynamicToolCallStatus;
use codex_protocol::items::FileChangeItem;
use codex_protocol::items::McpToolCallStatus;
use codex_protocol::items::TurnItem;
use codex_protocol::protocol::Event;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::HookExecutionMode;
use codex_protocol::protocol::ItemCompletedEvent;
use codex_protocol::protocol::TurnAbortReason;
use codex_protocol::protocol::TurnAbortedEvent;
use codex_protocol::protocol::TurnCompleteEvent;
use serde_json::Value;
use sha2::Digest;
use sha2::Sha256;

const RECEIPT_SCHEMA_VERSION: u64 = 1;
const RECEIPT_SOURCE: &str = "codex.core";
const HOOK_RECEIPT_SOURCE: &str = "codex.post_tool_use_hook";

/// A derived receipt event and its deterministic identity.
#[derive(Debug)]
pub(crate) struct DerivedReceiptEvent {
    pub id: String,
    pub event: Event,
}

/// Builds an automatic receipt for a canonical lifecycle event.
pub(crate) fn receipt_event_for_event(
    thread_id: ThreadId,
    event: &EventMsg,
) -> Option<DerivedReceiptEvent> {
    match event {
        EventMsg::ItemCompleted(item) => {
            receipt_for_turn_item(thread_id, &item.turn_id, &item.item, item.completed_at_ms)
        }
        EventMsg::TurnComplete(turn) => receipt_for_turn_complete(thread_id, turn),
        EventMsg::TurnAborted(turn) => receipt_for_turn_aborted(thread_id, turn),
        _ => None,
    }
}

/// Builds the receipt produced by a trusted synchronous `PostToolUse` hook.
pub(crate) fn receipt_event_for_hook_evidence(
    thread_id: ThreadId,
    turn_id: &str,
    evidence: &PostToolUseEvidence,
) -> Result<DerivedReceiptEvent, String> {
    if evidence.attribution.execution_mode != HookExecutionMode::Sync {
        return Err("evidence requires a synchronous hook".to_string());
    }
    if evidence
        .refs
        .iter()
        .any(|reference| !is_canonical_reference_id(&reference.id))
    {
        return Err("evidence references must identify canonical items".to_string());
    }
    let receipt_id = deterministic_receipt_id(
        "hook.evidence",
        &format!(
            "{thread_id}:{turn_id}:{}:{}:{}",
            evidence.attribution.handler_id, evidence.kind, evidence.subject
        ),
    );
    let mut receipt = ReceiptAttachedItem::new(
        receipt_id,
        RECEIPT_SCHEMA_VERSION,
        evidence.kind.clone(),
        evidence.subject.clone(),
        receipt_status(evidence.status),
        now_timestamp(),
        HOOK_RECEIPT_SOURCE,
    )
    .map_err(|error| error.to_string())?;
    receipt.thread_id = Some(thread_id.to_string());
    receipt.turn_id = Some(turn_id.to_string());
    receipt.tags = evidence.tags.clone();
    receipt.refs = evidence
        .refs
        .iter()
        .map(|reference| ReceiptReference {
            kind: reference.kind.clone(),
            id: reference.id.clone(),
        })
        .collect();
    receipt.metadata = evidence.metadata.clone();
    receipt.provenance = Some(serde_json::json!({
        "handlerId": evidence.attribution.handler_id.clone(),
        "handlerType": evidence.attribution.handler_type,
        "executionMode": evidence.attribution.execution_mode,
        "source": evidence.attribution.source,
    }));
    receipt.validate().map_err(|error| error.to_string())?;
    Ok(receipt_event(thread_id, turn_id.to_string(), receipt))
}

/// Returns receipt identifiers already present in a hydrated history so a
/// resumed session does not re-append a receipt observed before restart.
pub(crate) fn receipt_ids_from_history(history: &InitialHistory) -> HashSet<String> {
    history
        .get_rollout_items()
        .iter()
        .filter_map(receipt_id_from_rollout_item)
        .collect()
}

fn receipt_for_turn_item(
    thread_id: ThreadId,
    turn_id: &str,
    item: &TurnItem,
    completed_at_ms: i64,
) -> Option<DerivedReceiptEvent> {
    let (kind, subject, status, metadata) = match item {
        TurnItem::CommandExecution(item) => {
            if item.status == CommandExecutionStatus::InProgress {
                return None;
            }
            (
                "tool.execution",
                "command execution",
                command_status(item.status),
                serde_json::json!({
                    "toolType": "command",
                    "status": command_status_label(item.status),
                    "exitCode": item.exit_code,
                    "durationMs": item.duration.map(|duration| duration.as_millis()),
                }),
            )
        }
        TurnItem::DynamicToolCall(item) => {
            let status = match item.status {
                DynamicToolCallStatus::Completed if item.success != Some(false) => {
                    ReceiptStatus::Pass
                }
                DynamicToolCallStatus::Completed => ReceiptStatus::Fail,
                DynamicToolCallStatus::Failed => ReceiptStatus::Fail,
                DynamicToolCallStatus::InProgress => return None,
            };
            (
                "tool.execution",
                "dynamic tool execution",
                status,
                serde_json::json!({
                    "toolType": "dynamic",
                    "status": dynamic_status_label(item.status),
                    "success": item.success,
                    "durationMs": item.duration.map(|duration| duration.as_millis()),
                }),
            )
        }
        TurnItem::McpToolCall(item) => {
            let status = match item.status {
                McpToolCallStatus::Completed => ReceiptStatus::Pass,
                McpToolCallStatus::Failed => ReceiptStatus::Fail,
                McpToolCallStatus::InProgress => return None,
            };
            (
                "tool.execution",
                "MCP tool execution",
                status,
                serde_json::json!({
                    "toolType": "mcp",
                    "status": mcp_status_label(item.status),
                    "durationMs": item.duration.map(|duration| duration.as_millis()),
                }),
            )
        }
        TurnItem::CollabAgentToolCall(item) => {
            let status = match item.status {
                CollabAgentToolCallStatus::Completed => ReceiptStatus::Pass,
                CollabAgentToolCallStatus::Failed => ReceiptStatus::Fail,
                CollabAgentToolCallStatus::Interrupted => ReceiptStatus::Inconclusive,
                CollabAgentToolCallStatus::InProgress => return None,
            };
            (
                "tool.execution",
                "agent tool execution",
                status,
                serde_json::json!({
                    "toolType": "agent",
                    "status": collab_status_label(item.status),
                }),
            )
        }
        TurnItem::FileChange(item) => file_change_receipt_fields(item)?,
        TurnItem::Extension(_)
        | TurnItem::UserMessage(_)
        | TurnItem::FunctionCallOutput(_)
        | TurnItem::HookPrompt(_)
        | TurnItem::AgentMessage(_)
        | TurnItem::Plan(_)
        | TurnItem::Reasoning(_)
        | TurnItem::SubAgentActivity(_)
        | TurnItem::WebSearch(_)
        | TurnItem::ImageView(_)
        | TurnItem::ImageGeneration(_)
        | TurnItem::EnteredReviewMode(_)
        | TurnItem::ExitedReviewMode(_)
        | TurnItem::ContextCompaction(_) => return None,
    };

    let receipt_id =
        deterministic_receipt_id(kind, &format!("{thread_id}:{turn_id}:{}", item.id()));
    let mut receipt = ReceiptAttachedItem::new(
        receipt_id,
        RECEIPT_SCHEMA_VERSION,
        kind,
        subject,
        status,
        timestamp_from_millis(completed_at_ms),
        RECEIPT_SOURCE,
    )
    .ok()?;
    receipt.thread_id = Some(thread_id.to_string());
    receipt.turn_id = Some(turn_id.to_string());
    receipt.refs = vec![
        ReceiptReference {
            kind: "item".to_string(),
            id: item.id(),
        },
        ReceiptReference {
            kind: "turn".to_string(),
            id: turn_id.to_string(),
        },
    ];
    receipt.metadata = Some(metadata);
    receipt.validate().ok()?;
    Some(receipt_event(thread_id, turn_id.to_string(), receipt))
}

fn file_change_receipt_fields(
    item: &FileChangeItem,
) -> Option<(&'static str, &'static str, ReceiptStatus, Value)> {
    let status = match item.status.as_ref()? {
        codex_protocol::protocol::PatchApplyStatus::Completed => ReceiptStatus::Pass,
        codex_protocol::protocol::PatchApplyStatus::Failed => ReceiptStatus::Fail,
        codex_protocol::protocol::PatchApplyStatus::Declined => ReceiptStatus::Blocked,
    };
    Some((
        "file.change",
        "file change",
        status,
        serde_json::json!({
            "changeCount": item.changes.len(),
            "status": match item.status.as_ref() {
                Some(codex_protocol::protocol::PatchApplyStatus::Completed) => "completed",
                Some(codex_protocol::protocol::PatchApplyStatus::Failed) => "failed",
                Some(codex_protocol::protocol::PatchApplyStatus::Declined) => "declined",
                None => "inconclusive",
            },
            "autoApproved": item.auto_approved,
        }),
    ))
}

fn receipt_for_turn_complete(
    thread_id: ThreadId,
    event: &TurnCompleteEvent,
) -> Option<DerivedReceiptEvent> {
    let status = event
        .error
        .as_ref()
        .map_or(ReceiptStatus::Pass, |_| ReceiptStatus::Fail);
    let receipt_id =
        deterministic_receipt_id("turn.outcome", &format!("{thread_id}:{}", event.turn_id));
    let mut receipt = ReceiptAttachedItem::new(
        receipt_id,
        RECEIPT_SCHEMA_VERSION,
        "turn.outcome",
        "turn outcome",
        status,
        timestamp_from_millis(event.completed_at.unwrap_or_default().saturating_mul(1_000)),
        RECEIPT_SOURCE,
    )
    .ok()?;
    receipt.thread_id = Some(thread_id.to_string());
    receipt.turn_id = Some(event.turn_id.clone());
    receipt.refs = vec![ReceiptReference {
        kind: "turn".to_string(),
        id: event.turn_id.clone(),
    }];
    receipt.metadata = Some(serde_json::json!({
        "outcome": if event.error.is_some() { "failed" } else { "completed" },
        "hasError": event.error.is_some(),
        "durationMs": event.duration_ms,
    }));
    receipt.validate().ok()?;
    Some(receipt_event(thread_id, event.turn_id.clone(), receipt))
}

fn receipt_for_turn_aborted(
    thread_id: ThreadId,
    event: &TurnAbortedEvent,
) -> Option<DerivedReceiptEvent> {
    let turn_id = event.turn_id.clone()?;
    let receipt_id = deterministic_receipt_id("turn.outcome", &format!("{thread_id}:{turn_id}"));
    let mut receipt = ReceiptAttachedItem::new(
        receipt_id,
        RECEIPT_SCHEMA_VERSION,
        "turn.outcome",
        "turn outcome",
        ReceiptStatus::Inconclusive,
        timestamp_from_millis(event.completed_at.unwrap_or_default().saturating_mul(1_000)),
        RECEIPT_SOURCE,
    )
    .ok()?;
    receipt.thread_id = Some(thread_id.to_string());
    receipt.turn_id = Some(turn_id.clone());
    receipt.refs = vec![ReceiptReference {
        kind: "turn".to_string(),
        id: turn_id.clone(),
    }];
    receipt.metadata = Some(serde_json::json!({
        "outcome": "aborted",
        "reason": abort_reason_label(event.reason.clone()),
        "durationMs": event.duration_ms,
    }));
    receipt.validate().ok()?;
    Some(receipt_event(thread_id, turn_id, receipt))
}

fn receipt_event(
    thread_id: ThreadId,
    turn_id: String,
    receipt: ReceiptAttachedItem,
) -> DerivedReceiptEvent {
    DerivedReceiptEvent {
        id: receipt.receipt_id.clone(),
        event: Event {
            id: turn_id.clone(),
            msg: EventMsg::ItemCompleted(ItemCompletedEvent {
                thread_id,
                turn_id,
                item: TurnItem::Extension(ExtensionItem::ReceiptAttached(receipt)),
                started_at_ms: None,
                completed_at_ms: Utc::now().timestamp_millis(),
            }),
        },
    }
}

fn receipt_id_from_rollout_item(item: &RolloutItem) -> Option<String> {
    let RolloutItem::EventMsg(EventMsg::ItemCompleted(event)) = item else {
        return None;
    };
    let TurnItem::Extension(ExtensionItem::ReceiptAttached(receipt)) = &event.item else {
        return None;
    };
    Some(receipt.receipt_id.clone())
}

fn is_canonical_reference_id(id: &str) -> bool {
    !id.is_empty()
        && !id.contains('/')
        && !id.contains('\\')
        && !id.contains(':')
        && !id.starts_with('.')
}

fn deterministic_receipt_id(kind: &str, identity: &str) -> String {
    let mut digest = Sha256::new();
    digest.update(kind.as_bytes());
    digest.update([0]);
    digest.update(identity.as_bytes());
    let digest = digest.finalize();
    let hex = digest
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    format!("auto-{kind}-{hex}")
}

fn now_timestamp() -> String {
    Utc::now().to_rfc3339_opts(SecondsFormat::Millis, true)
}

fn timestamp_from_millis(timestamp_ms: i64) -> String {
    if timestamp_ms <= 0 {
        return now_timestamp();
    }
    chrono::DateTime::<Utc>::from_timestamp_millis(timestamp_ms)
        .map(|timestamp| timestamp.to_rfc3339_opts(SecondsFormat::Millis, true))
        .unwrap_or_else(now_timestamp)
}

fn receipt_status(status: PostToolUseEvidenceStatus) -> ReceiptStatus {
    match status {
        PostToolUseEvidenceStatus::Pass => ReceiptStatus::Pass,
        PostToolUseEvidenceStatus::Fail => ReceiptStatus::Fail,
        PostToolUseEvidenceStatus::Blocked => ReceiptStatus::Blocked,
        PostToolUseEvidenceStatus::Inconclusive => ReceiptStatus::Inconclusive,
        PostToolUseEvidenceStatus::Informational => ReceiptStatus::Informational,
    }
}

fn command_status(status: CommandExecutionStatus) -> ReceiptStatus {
    match status {
        CommandExecutionStatus::Completed => ReceiptStatus::Pass,
        CommandExecutionStatus::Failed => ReceiptStatus::Fail,
        CommandExecutionStatus::Declined => ReceiptStatus::Blocked,
        CommandExecutionStatus::InProgress => ReceiptStatus::Inconclusive,
    }
}

fn command_status_label(status: CommandExecutionStatus) -> &'static str {
    match status {
        CommandExecutionStatus::InProgress => "in_progress",
        CommandExecutionStatus::Completed => "completed",
        CommandExecutionStatus::Failed => "failed",
        CommandExecutionStatus::Declined => "declined",
    }
}

fn dynamic_status_label(status: DynamicToolCallStatus) -> &'static str {
    match status {
        DynamicToolCallStatus::InProgress => "in_progress",
        DynamicToolCallStatus::Completed => "completed",
        DynamicToolCallStatus::Failed => "failed",
    }
}

fn mcp_status_label(status: McpToolCallStatus) -> &'static str {
    match status {
        McpToolCallStatus::InProgress => "in_progress",
        McpToolCallStatus::Completed => "completed",
        McpToolCallStatus::Failed => "failed",
    }
}

fn collab_status_label(status: CollabAgentToolCallStatus) -> &'static str {
    match status {
        CollabAgentToolCallStatus::InProgress => "in_progress",
        CollabAgentToolCallStatus::Completed => "completed",
        CollabAgentToolCallStatus::Failed => "failed",
        CollabAgentToolCallStatus::Interrupted => "interrupted",
    }
}

fn abort_reason_label(reason: TurnAbortReason) -> &'static str {
    match reason {
        TurnAbortReason::Interrupted => "interrupted",
        TurnAbortReason::Replaced => "replaced",
        TurnAbortReason::ReviewEnded => "review_ended",
        TurnAbortReason::BudgetLimited => "budget_limited",
    }
}

#[cfg(test)]
#[path = "tests.rs"]
mod tests;
