//! Response-item inspection and item-level provenance.

pub(super) use super::preview::PreviewBudget;
use super::types::CompactionSurvival;
use super::types::ContextCacheMetrics;
use super::types::ContextInspectionGroup;
use super::types::ContextInspectionItem;
use super::types::ContextLogicalOrigin;
use super::types::ContextVisibility;
use crate::client_common::Prompt;
use crate::context_manager::estimate_item_token_count;
use codex_protocol::models::AgentMessageInputContent;
use codex_protocol::models::BaseInstructions;
use codex_protocol::models::FunctionCallOutputContentItem;
use codex_protocol::models::ResponseItem;
use codex_protocol::protocol::TokenUsage;
use codex_protocol::protocol::TokenUsageInfo;
use codex_utils_output_truncation::approx_token_count;
use std::collections::HashMap;

const MAX_DUPLICATE_TRACKED_ITEMS: usize = 4_096;

pub(super) fn inspect_base_instructions(
    base_instructions: &BaseInstructions,
    preview_budget: &mut PreviewBudget,
) -> ContextInspectionGroup {
    let text = &base_instructions.text;
    let bytes = text.len();
    ContextInspectionGroup {
        index: 0,
        item_count: usize::from(!text.is_empty()),
        role: "system".to_string(),
        content_kind: "baseInstructions".to_string(),
        logical_origin: ContextLogicalOrigin::BaseInstructions,
        visibility: ContextVisibility::Model,
        estimated_tokens: i64::try_from(approx_token_count(text)).unwrap_or(i64::MAX),
        serialized_bytes: bytes,
        survives_compaction: CompactionSurvival::True,
        encrypted: false,
        duplicate_group: None,
        duplicate_count: 1,
        preview: preview_budget.take_text(text, /*allow_urls*/ false),
    }
}

pub(super) fn inspect_tools(
    prompt: &Prompt,
    _preview_budget: &mut PreviewBudget,
) -> ContextInspectionGroup {
    let serialized_bytes = serde_json::to_vec(prompt.tools.as_ref())
        .map(|bytes| bytes.len())
        .unwrap_or_default();
    ContextInspectionGroup {
        index: 1,
        item_count: prompt.tools.len(),
        role: "developer".to_string(),
        content_kind: "tools".to_string(),
        logical_origin: ContextLogicalOrigin::ThreadContext,
        visibility: ContextVisibility::Model,
        estimated_tokens: approx_tokens_from_bytes(serialized_bytes),
        serialized_bytes,
        survives_compaction: CompactionSurvival::True,
        encrypted: false,
        duplicate_group: None,
        duplicate_count: 1,
        // Tool schemas are aggregated payloads and are never previewed.
        preview: None,
    }
}

pub(super) fn inspect_items(
    items: &[ResponseItem],
    inherited_item_count: Option<usize>,
    replacement_items: &[ResponseItem],
    current_turn_id: Option<&str>,
    preview_budget: &mut PreviewBudget,
) -> Vec<ContextInspectionItem> {
    let serialized = items
        .iter()
        .map(|item| serde_json::to_vec(item).unwrap_or_default())
        .collect::<Vec<_>>();
    let mut duplicate_counts = HashMap::<Vec<u8>, usize>::new();
    for bytes in serialized.iter().take(MAX_DUPLICATE_TRACKED_ITEMS) {
        *duplicate_counts.entry(bytes.clone()).or_default() += 1;
    }
    let mut duplicate_first_index = HashMap::<Vec<u8>, usize>::new();
    let mut replacement_matches = vec![false; replacement_items.len()];
    items
        .iter()
        .enumerate()
        .map(|(index, item)| {
            let replacement_index = replacement_items
                .iter()
                .enumerate()
                .find(|(replacement_index, replacement)| {
                    !replacement_matches[*replacement_index] && *replacement == item
                })
                .map(|(replacement_index, _)| {
                    replacement_matches[replacement_index] = true;
                    replacement_index
                });
            let origin = logical_origin(
                item,
                index,
                inherited_item_count,
                replacement_index.is_some(),
                current_turn_id,
            );
            let encrypted = is_encrypted(item);
            let key = &serialized[index];
            let duplicate_count = if index < MAX_DUPLICATE_TRACKED_ITEMS {
                duplicate_counts.get(key).copied().unwrap_or(1)
            } else {
                1
            };
            let duplicate_group = if duplicate_count > 1 {
                let first_index = *duplicate_first_index.entry(key.clone()).or_insert(index);
                Some(format!("group-{first_index}"))
            } else {
                None
            };
            ContextInspectionItem {
                index,
                role: response_role(item).to_string(),
                content_kind: content_kind(item),
                logical_origin: origin,
                visibility: response_visibility(item),
                estimated_tokens: estimate_item_token_count(item),
                serialized_bytes: serialized[index].len(),
                survives_compaction: compaction_survival(origin),
                duplicate_group,
                duplicate_count,
                encrypted,
                preview: if encrypted {
                    None
                } else {
                    preview_budget.take_item(item)
                },
            }
        })
        .collect()
}

fn response_role(item: &ResponseItem) -> &str {
    match item {
        ResponseItem::Message { role, .. } => role,
        ResponseItem::AdditionalTools { role, .. } => role,
        ResponseItem::AgentMessage { .. }
        | ResponseItem::Reasoning { .. }
        | ResponseItem::LocalShellCall { .. }
        | ResponseItem::FunctionCall { .. }
        | ResponseItem::ToolSearchCall { .. }
        | ResponseItem::CustomToolCall { .. }
        | ResponseItem::WebSearchCall { .. }
        | ResponseItem::ImageGenerationCall { .. }
        | ResponseItem::Compaction { .. }
        | ResponseItem::ContextCompaction { .. } => "assistant",
        ResponseItem::FunctionCallOutput { .. }
        | ResponseItem::CustomToolCallOutput { .. }
        | ResponseItem::ToolSearchOutput { .. } => "tool",
        ResponseItem::CompactionTrigger { .. } => "system",
        ResponseItem::Other => "unknown",
    }
}

fn content_kind(item: &ResponseItem) -> String {
    if let ResponseItem::Message {
        internal_chat_message_metadata_passthrough,
        ..
    } = item
        && let Some(kinds) = internal_chat_message_metadata_passthrough
            .as_ref()
            .and_then(|metadata| metadata.content_item_kinds.as_ref())
    {
        if kinds.is_empty() {
            return "unknown".to_string();
        }
        let first = &kinds[0].0;
        if kinds.iter().all(|kind| kind.0 == *first) {
            return first.clone();
        }
        return "mixed".to_string();
    }
    match item {
        ResponseItem::Message { .. } => "unknown".to_string(),
        ResponseItem::AdditionalTools { .. } => "tools".to_string(),
        ResponseItem::AgentMessage { .. } => "agentMessage".to_string(),
        ResponseItem::Reasoning { .. } => "reasoning".to_string(),
        ResponseItem::LocalShellCall { .. } => "localShellCall".to_string(),
        ResponseItem::FunctionCall { .. } => "functionCall".to_string(),
        ResponseItem::ToolSearchCall { .. } => "toolSearchCall".to_string(),
        ResponseItem::FunctionCallOutput { .. } => "toolOutput".to_string(),
        ResponseItem::CustomToolCall { .. } => "customToolCall".to_string(),
        ResponseItem::CustomToolCallOutput { .. } => "toolOutput".to_string(),
        ResponseItem::ToolSearchOutput { .. } => "toolOutput".to_string(),
        ResponseItem::WebSearchCall { .. } => "webSearchCall".to_string(),
        ResponseItem::ImageGenerationCall { .. } => "imageGenerationCall".to_string(),
        ResponseItem::Compaction { .. } | ResponseItem::ContextCompaction { .. } => {
            "compaction".to_string()
        }
        ResponseItem::CompactionTrigger { .. } | ResponseItem::Other => "unknown".to_string(),
    }
}

fn response_visibility(item: &ResponseItem) -> ContextVisibility {
    match item {
        ResponseItem::CompactionTrigger { .. } | ResponseItem::Other => ContextVisibility::Internal,
        _ => ContextVisibility::Model,
    }
}

fn logical_origin(
    item: &ResponseItem,
    index: usize,
    inherited_item_count: Option<usize>,
    is_replacement: bool,
    current_turn_id: Option<&str>,
) -> ContextLogicalOrigin {
    if is_replacement {
        return ContextLogicalOrigin::CompactionReplacement;
    }
    if is_tool_output(item) {
        return ContextLogicalOrigin::ToolOutput;
    }
    if inherited_item_count.is_some_and(|count| index < count) {
        return ContextLogicalOrigin::InheritedHistory;
    }
    if current_turn_id.is_some_and(|turn_id| item.turn_id() == Some(turn_id)) {
        return ContextLogicalOrigin::NewOutput;
    }
    if inherited_item_count.is_some_and(|count| index >= count)
        && !is_contextual_item(item)
        && !matches!(
            item,
            ResponseItem::Compaction { .. } | ResponseItem::ContextCompaction { .. }
        )
    {
        return ContextLogicalOrigin::NewOutput;
    }
    match item {
        ResponseItem::Message { .. } => {
            if contextual_origin(item).is_some() {
                contextual_origin(item).unwrap_or(ContextLogicalOrigin::Unknown)
            } else {
                ContextLogicalOrigin::Unknown
            }
        }
        ResponseItem::AgentMessage { .. }
        | ResponseItem::Reasoning { .. }
        | ResponseItem::FunctionCall { .. }
        | ResponseItem::CustomToolCall { .. }
        | ResponseItem::ToolSearchCall { .. }
        | ResponseItem::LocalShellCall { .. }
        | ResponseItem::WebSearchCall { .. }
        | ResponseItem::ImageGenerationCall { .. } => ContextLogicalOrigin::Unknown,
        ResponseItem::Compaction { .. } | ResponseItem::ContextCompaction { .. } => {
            ContextLogicalOrigin::Derived
        }
        ResponseItem::AdditionalTools { .. } => ContextLogicalOrigin::ThreadContext,
        ResponseItem::CompactionTrigger { .. } | ResponseItem::Other => {
            ContextLogicalOrigin::Unknown
        }
        ResponseItem::FunctionCallOutput { .. }
        | ResponseItem::CustomToolCallOutput { .. }
        | ResponseItem::ToolSearchOutput { .. } => ContextLogicalOrigin::ToolOutput,
    }
}

fn contextual_origin(item: &ResponseItem) -> Option<ContextLogicalOrigin> {
    let ResponseItem::Message {
        internal_chat_message_metadata_passthrough,
        ..
    } = item
    else {
        return None;
    };
    let kinds = internal_chat_message_metadata_passthrough
        .as_ref()?
        .content_item_kinds
        .as_ref()?;
    let kind = kinds.first()?.0.as_str();
    if kind.is_empty()
        || kind == "unknown"
        || kind.starts_with("user.")
        || kind.starts_with("assistant.")
        || kind.starts_with("tool.")
    {
        return None;
    }
    if kind.starts_with("token_budget.")
        || kind.starts_with("current_time.")
        || kind.starts_with("rollout_budget.")
    {
        Some(ContextLogicalOrigin::TurnContext)
    } else if kind.starts_with("environments.")
        || kind.starts_with("permissions.")
        || kind.starts_with("tools.")
        || kind.starts_with("collaboration_mode.")
        || kind.starts_with("multi_agent.")
        || kind.starts_with("personality.")
        || kind.starts_with("network_proxy.")
    {
        Some(ContextLogicalOrigin::WorldState)
    } else {
        Some(ContextLogicalOrigin::ThreadContext)
    }
}

fn is_contextual_item(item: &ResponseItem) -> bool {
    contextual_origin(item).is_some()
}

fn is_tool_output(item: &ResponseItem) -> bool {
    matches!(
        item,
        ResponseItem::FunctionCallOutput { .. }
            | ResponseItem::CustomToolCallOutput { .. }
            | ResponseItem::ToolSearchOutput { .. }
    )
}

fn is_encrypted(item: &ResponseItem) -> bool {
    match item {
        ResponseItem::AgentMessage { content, .. } => content
            .iter()
            .any(|content| matches!(content, AgentMessageInputContent::EncryptedContent { .. })),
        ResponseItem::Reasoning { .. } | ResponseItem::Compaction { .. } => true,
        ResponseItem::ContextCompaction {
            encrypted_content, ..
        } => encrypted_content.is_some(),
        ResponseItem::FunctionCall {
            encrypted_function_args,
            ..
        } => encrypted_function_args.is_some(),
        ResponseItem::FunctionCallOutput { output, .. }
        | ResponseItem::CustomToolCallOutput { output, .. } => {
            output.content_items().is_some_and(|items| {
                items.iter().any(|item| {
                    matches!(item, FunctionCallOutputContentItem::EncryptedContent { .. })
                })
            })
        }
        _ => false,
    }
}

fn compaction_survival(origin: ContextLogicalOrigin) -> CompactionSurvival {
    match origin {
        ContextLogicalOrigin::BaseInstructions
        | ContextLogicalOrigin::ThreadContext
        | ContextLogicalOrigin::TurnContext
        | ContextLogicalOrigin::WorldState
        | ContextLogicalOrigin::CompactionReplacement => CompactionSurvival::True,
        ContextLogicalOrigin::ToolOutput => CompactionSurvival::False,
        ContextLogicalOrigin::InheritedHistory
        | ContextLogicalOrigin::NewOutput
        | ContextLogicalOrigin::Derived
        | ContextLogicalOrigin::Unknown => CompactionSurvival::Unknown,
    }
}

fn approx_tokens_from_bytes(bytes: usize) -> i64 {
    i64::try_from(bytes.saturating_add(3) / 4).unwrap_or(i64::MAX)
}

pub(super) fn cache_metrics_from_usage(usage: &TokenUsage) -> ContextCacheMetrics {
    let cached_input_tokens = usage.cached_input_tokens.max(0);
    let cache_write_input_tokens = usage.cache_write_input_tokens.max(0);
    ContextCacheMetrics {
        cached_input_tokens: Some(cached_input_tokens),
        uncached_input_tokens: Some(
            (usage.input_tokens - cached_input_tokens - cache_write_input_tokens).max(0),
        ),
        cache_write_input_tokens: Some(cache_write_input_tokens),
    }
}

pub(super) fn active_tokens(info: &TokenUsageInfo) -> i64 {
    info.total_token_usage.total_tokens.max(0)
}

pub(super) fn active_tokens_from_usage(usage: &TokenUsage) -> i64 {
    usage.total_tokens.max(0)
}
