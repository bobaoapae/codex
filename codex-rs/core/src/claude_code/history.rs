//! Rendering Codex conversation items into the plain-text turns the Claude Code
//! CLI accepts, plus the bookkeeping that decides whether a request can extend
//! an existing Claude session or has to start a new one.
//!
//! Codex owns the conversation: every request carries the full history in
//! `Prompt::input`. Claude Code owns a session of its own, so the adapter tracks
//! how much of that history a given Claude session has already seen and forwards
//! only the tail. If the leading portion of the history changes — a compaction,
//! a fork, an edited turn — the prefix fingerprint stops matching and the next
//! request replays everything into a fresh Claude session.

use codex_protocol::models::ContentItem;
use codex_protocol::models::FunctionCallOutputContentItem;
use codex_protocol::models::ResponseItem;
use std::collections::hash_map::DefaultHasher;
use std::hash::Hash;
use std::hash::Hasher;

/// Tool output longer than this is elided in the middle: the head and tail carry
/// the signal (command, first errors, exit status) and the middle rarely does.
const MAX_TOOL_OUTPUT_CHARS: usize = 6_000;

/// What the adapter should send to Claude for one request.
#[derive(Debug, PartialEq)]
pub(super) struct RequestPlan {
    /// Text of the user turn to write on the CLI's stdin.
    pub(super) turn_text: String,
    /// When true the caller must start a brand new Claude session rather than
    /// resuming the recorded one.
    pub(super) restart_session: bool,
    /// Number of history items this request leaves delivered, to be recorded on
    /// success.
    pub(super) delivered_items: usize,
    /// Fingerprint of the delivered prefix, recorded alongside the count.
    pub(super) delivered_fingerprint: u64,
}

/// Per-thread continuity state for the Claude session backing a Codex thread.
#[derive(Debug, Default, Clone)]
pub(crate) struct ClaudeSessionContinuity {
    /// Claude session id to `--resume`, once one turn has completed.
    pub(super) session_id: Option<String>,
    /// How many leading history items that session has already seen.
    pub(super) delivered_items: usize,
    /// Fingerprint of those leading items.
    pub(super) delivered_fingerprint: u64,
}

/// Decides between extending the recorded Claude session and replaying history.
pub(super) fn plan_request(
    input: &[ResponseItem],
    continuity: &ClaudeSessionContinuity,
) -> RequestPlan {
    let can_extend = continuity.session_id.is_some()
        && continuity.delivered_items > 0
        && continuity.delivered_items <= input.len()
        && fingerprint(&input[..continuity.delivered_items]) == continuity.delivered_fingerprint;

    let (tail, restart_session) = if can_extend {
        (&input[continuity.delivered_items..], false)
    } else {
        (input, true)
    };

    RequestPlan {
        turn_text: render_turn(tail, restart_session),
        restart_session,
        delivered_items: input.len(),
        delivered_fingerprint: fingerprint(input),
    }
}

/// Stable fingerprint of a history prefix.
///
/// Only the rendered text is hashed: ids and other transport metadata change
/// between requests without changing what the model was told.
fn fingerprint(items: &[ResponseItem]) -> u64 {
    let mut hasher = DefaultHasher::new();
    items.len().hash(&mut hasher);
    for item in items {
        render_item(item).hash(&mut hasher);
    }
    hasher.finish()
}

fn render_turn(items: &[ResponseItem], is_replay: bool) -> String {
    let rendered: Vec<String> = items
        .iter()
        .map(render_item)
        .filter(|text| !text.trim().is_empty())
        .collect();

    if rendered.is_empty() {
        return "(no new input; continue from the previous turn)".to_string();
    }

    if !is_replay {
        return rendered.join("\n\n");
    }

    format!(
        "You are running as an agent inside Codex. Below is the conversation so \
far, transcribed from the Codex session. Continue it: do the work and reply to \
the last message.\n\n<codex_transcript>\n{}\n</codex_transcript>",
        rendered.join("\n\n")
    )
}

fn render_item(item: &ResponseItem) -> String {
    match item {
        ResponseItem::Message { role, content, .. } => {
            let text = content
                .iter()
                .filter_map(|content_item| match content_item {
                    ContentItem::InputText { text } | ContentItem::OutputText { text } => {
                        Some(text.as_str())
                    }
                    ContentItem::InputImage { .. } => Some("[image omitted]"),
                    ContentItem::InputAudio { .. } => Some("[audio omitted]"),
                })
                .collect::<Vec<_>>()
                .join("\n");
            let text = strip_codex_harness_sections(&text);
            if text.trim().is_empty() {
                String::new()
            } else {
                format!("<{role}>\n{text}\n</{role}>")
            }
        }
        ResponseItem::Reasoning { .. } => {
            // Claude reasons for itself; replaying another model's reasoning is
            // noise at best and misleading at worst.
            String::new()
        }
        ResponseItem::FunctionCall {
            name, arguments, ..
        } => format!(
            "<tool_call name=\"{name}\">\n{}\n</tool_call>",
            truncate_middle(arguments, MAX_TOOL_OUTPUT_CHARS)
        ),
        ResponseItem::FunctionCallOutput { output, .. } => {
            let text = output
                .content_items()
                .map(|items| {
                    items
                        .iter()
                        .filter_map(|content_item| match content_item {
                            FunctionCallOutputContentItem::InputText { text } => {
                                Some(text.as_str())
                            }
                            FunctionCallOutputContentItem::InputImage { .. } => {
                                Some("[image omitted]")
                            }
                            FunctionCallOutputContentItem::InputAudio { .. } => {
                                Some("[audio omitted]")
                            }
                            // Encrypted payloads are opaque to us and meaningless
                            // to a different provider's model.
                            FunctionCallOutputContentItem::EncryptedContent { .. } => None,
                        })
                        .collect::<Vec<_>>()
                        .join("\n")
                })
                .unwrap_or_else(|| output.text_content().unwrap_or_default().to_string());
            format!(
                "<tool_result>\n{}\n</tool_result>",
                truncate_middle(&text, MAX_TOOL_OUTPUT_CHARS)
            )
        }
        ResponseItem::LocalShellCall { action, .. } => format!(
            "<tool_call name=\"shell\">\n{}\n</tool_call>",
            truncate_middle(
                &serde_json::to_string(action).unwrap_or_default(),
                MAX_TOOL_OUTPUT_CHARS,
            )
        ),
        ResponseItem::CustomToolCall { name, input, .. } => format!(
            "<tool_call name=\"{name}\">\n{}\n</tool_call>",
            truncate_middle(input, MAX_TOOL_OUTPUT_CHARS)
        ),
        ResponseItem::CustomToolCallOutput { output, .. } => format!(
            "<tool_result>\n{}\n</tool_result>",
            truncate_middle(
                output.text_content().unwrap_or_default(),
                MAX_TOOL_OUTPUT_CHARS,
            )
        ),
        other => {
            // Anything else is transport bookkeeping (tool search, additional
            // tools, agent-to-agent envelopes). Keep a compact trace rather than
            // dropping it silently.
            let rendered = serde_json::to_string(other).unwrap_or_default();
            if rendered.len() > MAX_TOOL_OUTPUT_CHARS {
                String::new()
            } else {
                format!("<codex_item>{rendered}</codex_item>")
            }
        }
    }
}

/// Instructions that describe the Codex harness rather than the task.
///
/// A Claude agent has none of the tools they talk about — the collaboration
/// namespace, `functions.exec`, the Codex memory folder — so replaying them
/// burns context and provokes attempts that can only fail. Observed in
/// production: an agent spent a turn trying to read `.codex/memories` because
/// this text told it to.
///
/// Each entry starts a harness section; everything from that line to the end of
/// the message is dropped. The role instructions written by the user's agent
/// role sit *before* these markers and survive.
///
/// Matching is line-anchored rather than substring-based: the harness emits CRLF,
/// so a marker carrying its own newlines silently fails to match.
const HARNESS_SECTION_MARKERS: &[&str] = &[
    "## Memory",
    "You are an agent in a team of agents",
    "<multi_agent_mode>",
    "<recommended_plugins>",
];

/// Removes Codex-harness scaffolding from a message before it reaches Claude.
fn strip_codex_harness_sections(text: &str) -> String {
    let mut kept = Vec::new();
    for line in text.split_inclusive('\n') {
        let trimmed = line.trim();
        if HARNESS_SECTION_MARKERS
            .iter()
            .any(|marker| trimmed == *marker || trimmed.starts_with(marker))
        {
            break;
        }
        kept.push(line);
    }
    kept.concat().trim_end().to_string()
}

fn truncate_middle(text: &str, max_chars: usize) -> String {
    let char_count = text.chars().count();
    if char_count <= max_chars {
        return text.to_string();
    }
    let head_len = max_chars / 2;
    let tail_len = max_chars - head_len;
    let head: String = text.chars().take(head_len).collect();
    let tail: String = text.chars().skip(char_count - tail_len).collect();
    let elided = char_count - head_len - tail_len;
    format!("{head}\n… [{elided} characters elided] …\n{tail}")
}

#[cfg(test)]
#[path = "history_tests.rs"]
mod tests;
