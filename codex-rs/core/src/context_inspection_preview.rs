//! Bounded, redacted context previews.

use super::MAX_PREVIEW_CHARS;
use super::MAX_PREVIEW_SOURCE_CHARS;
use super::MAX_PREVIEW_TOKENS;
use super::REDACTED;
use super::SECRET_KEYS;
use codex_protocol::models::AgentMessageInputContent;
use codex_protocol::models::ContentItem;
use codex_protocol::models::ResponseItem;
use codex_utils_output_truncation::approx_token_count;

pub(crate) struct PreviewBudget {
    enabled: bool,
    remaining_tokens: usize,
}

impl PreviewBudget {
    pub(crate) fn new(enabled: bool) -> Self {
        Self {
            enabled,
            remaining_tokens: MAX_PREVIEW_TOKENS,
        }
    }

    pub(crate) fn take_item(&mut self, item: &ResponseItem) -> Option<String> {
        let text = preview_text(item)?;
        self.take_text(&text, /*allow_urls*/ false)
    }

    pub(crate) fn take_text(&mut self, text: &str, allow_urls: bool) -> Option<String> {
        if !self.enabled || self.remaining_tokens == 0 {
            return None;
        }
        let text = clip_chars(text, MAX_PREVIEW_SOURCE_CHARS);
        if !allow_urls && contains_url(&text) {
            return None;
        }
        let redacted = redact_secrets(&text);
        let redacted = clip_chars(&redacted, MAX_PREVIEW_CHARS);
        let redacted = clip_to_tokens(&redacted, self.remaining_tokens);
        let tokens = approx_token_count(&redacted);
        if tokens == 0 {
            return None;
        }
        self.remaining_tokens = self.remaining_tokens.saturating_sub(tokens);
        Some(redacted)
    }
}

fn preview_text(item: &ResponseItem) -> Option<String> {
    match item {
        ResponseItem::Message { content, .. } => {
            if content.iter().any(|content| {
                matches!(
                    content,
                    ContentItem::InputImage { .. } | ContentItem::InputAudio { .. }
                )
            }) {
                return None;
            }
            let text = content
                .iter()
                .filter_map(|content| match content {
                    ContentItem::InputText { text } | ContentItem::OutputText { text } => {
                        Some(text.as_str())
                    }
                    ContentItem::InputImage { .. } | ContentItem::InputAudio { .. } => None,
                })
                .collect::<Vec<_>>()
                .join("\n");
            (!text.is_empty()).then_some(text)
        }
        ResponseItem::AgentMessage { content, .. } => {
            let mut text = Vec::new();
            for content in content {
                match content {
                    AgentMessageInputContent::InputText { text: value } => {
                        text.push(value.as_str())
                    }
                    AgentMessageInputContent::EncryptedContent { .. } => return None,
                }
            }
            (!text.is_empty()).then(|| text.join("\n"))
        }
        // Tool arguments, outputs, stdout, provider payloads, reasoning, and media URLs are
        // intentionally never previewed.
        ResponseItem::AdditionalTools { .. }
        | ResponseItem::Reasoning { .. }
        | ResponseItem::LocalShellCall { .. }
        | ResponseItem::FunctionCall { .. }
        | ResponseItem::ToolSearchCall { .. }
        | ResponseItem::FunctionCallOutput { .. }
        | ResponseItem::CustomToolCall { .. }
        | ResponseItem::CustomToolCallOutput { .. }
        | ResponseItem::ToolSearchOutput { .. }
        | ResponseItem::WebSearchCall { .. }
        | ResponseItem::ImageGenerationCall { .. }
        | ResponseItem::Compaction { .. }
        | ResponseItem::CompactionTrigger { .. }
        | ResponseItem::ContextCompaction { .. }
        | ResponseItem::Other => None,
    }
}

fn clip_chars(text: &str, max_chars: usize) -> String {
    text.chars().take(max_chars).collect()
}

fn clip_to_tokens(text: &str, max_tokens: usize) -> String {
    let max_bytes = max_tokens.saturating_mul(4).max(1);
    let mut end = 0;
    for (index, character) in text.char_indices() {
        let next = index.saturating_add(character.len_utf8());
        if next > max_bytes {
            break;
        }
        end = next;
    }
    text[..end].to_string()
}

fn contains_url(text: &str) -> bool {
    let lower = text.to_ascii_lowercase();
    lower.contains("http://") || lower.contains("https://") || lower.contains("data:")
}

fn redact_secrets(text: &str) -> String {
    let mut redacted = text.to_string();
    for key in SECRET_KEYS {
        redacted = redact_key_values(&redacted, key);
    }
    redacted = redact_bearer_values(&redacted);
    redact_bare_tokens(&redacted)
}

fn redact_key_values(text: &str, key: &str) -> String {
    let mut value = text.to_string();
    let lower_key = key.to_ascii_lowercase();
    let mut search_from = 0;
    while search_from < value.len() {
        let lower = value[search_from..].to_ascii_lowercase();
        let Some(relative) = lower.find(&lower_key) else {
            break;
        };
        let start = search_from.saturating_add(relative);
        let end_key = start.saturating_add(key.len());
        let boundary_before = start == 0 || !value.as_bytes()[start - 1].is_ascii_alphanumeric();
        let boundary_after = end_key >= value.len()
            || !value.as_bytes()[end_key].is_ascii_alphanumeric()
                && value.as_bytes()[end_key] != b'_';
        if !boundary_before || !boundary_after {
            search_from = end_key;
            continue;
        }
        let Some((value_start, quoted)) = secret_value_start(&value, end_key) else {
            search_from = end_key;
            continue;
        };
        let value_end = secret_value_end(&value, value_start, quoted);
        if value_end <= value_start {
            search_from = end_key;
            continue;
        }
        value.replace_range(value_start..value_end, REDACTED);
        search_from = value_start.saturating_add(REDACTED.len());
    }
    value
}

fn secret_value_start(text: &str, key_end: usize) -> Option<(usize, bool)> {
    let bytes = text.as_bytes();
    let mut index = key_end;
    while index < bytes.len() && bytes[index].is_ascii_whitespace() {
        index += 1;
    }
    if index < bytes.len() && bytes[index] == b'"' {
        index += 1;
        while index < bytes.len() && bytes[index].is_ascii_whitespace() {
            index += 1;
        }
    }
    if index >= bytes.len() || !matches!(bytes[index], b':' | b'=') {
        return None;
    }
    index += 1;
    while index < bytes.len() && bytes[index].is_ascii_whitespace() {
        index += 1;
    }
    let quoted = index < bytes.len() && bytes[index] == b'"';
    Some((index + usize::from(quoted), quoted))
}

fn secret_value_end(text: &str, start: usize, quoted: bool) -> usize {
    let bytes = text.as_bytes();
    let mut index = start;
    while index < bytes.len() {
        if quoted {
            if bytes[index] == b'"' {
                break;
            }
        } else if bytes[index].is_ascii_whitespace()
            || matches!(bytes[index], b',' | b';' | b'&' | b'}' | b']' | b'"')
        {
            break;
        }
        index += 1;
    }
    index
}

fn redact_bearer_values(text: &str) -> String {
    let mut result = text.to_string();
    let mut search_from = 0;
    while search_from < result.len() {
        let lower = result[search_from..].to_ascii_lowercase();
        let Some(relative) = lower.find("bearer ") else {
            break;
        };
        let start = search_from
            .saturating_add(relative)
            .saturating_add("bearer ".len());
        let end = result[start..]
            .find(char::is_whitespace)
            .map_or(result.len(), |offset| start.saturating_add(offset));
        if end <= start {
            break;
        }
        result.replace_range(start..end, REDACTED);
        search_from = start.saturating_add(REDACTED.len());
    }
    result
}

fn redact_bare_tokens(text: &str) -> String {
    text.split_inclusive(char::is_whitespace)
        .map(|segment| {
            let token_end = segment
                .char_indices()
                .rev()
                .find_map(|(index, character)| (!character.is_whitespace()).then_some(index))
                .map_or(0, |index| index.saturating_add(1));
            let (token, suffix) = segment.split_at(token_end);
            let lower = token.trim_matches(|character: char| {
                !character.is_ascii_alphanumeric() && character != '-' && character != '_'
            });
            let is_secret = lower.len() >= 12
                && (lower.starts_with("sk-")
                    || lower.starts_with("ghp_")
                    || lower.starts_with("xoxb-")
                    || lower.starts_with("eyj"));
            if is_secret {
                format!("{REDACTED}{suffix}")
            } else {
                segment.to_string()
            }
        })
        .collect()
}
