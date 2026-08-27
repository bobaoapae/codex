//! FORK: decides whether a request can extend the ChatGPT conversation that
//! already serves a thread, or has to replay the transcript into a new one.
//!
//! Same bookkeeping as `claude_code::history`: Codex owns the conversation and
//! sends the full history every request; the provider tracks how much of it the
//! live ChatGPT conversation has already seen and forwards only the tail. Two
//! things differ here: the recorded model matters (a ChatGPT conversation is
//! pinned to the model it was opened with, so a model switch is a restart), and
//! Codex's own compaction turn is recognised and answered from a disposable
//! conversation instead of polluting the one that serves the thread.

use crate::claude_code::history::fingerprint;
use crate::claude_code::history::item_fingerprint;
use codex_protocol::models::ContentItem;
use codex_protocol::models::ResponseItem;
use std::collections::HashSet;

/// Per-thread continuity of the ChatGPT conversation backing a Codex thread.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub(crate) struct ConversationContinuity {
    /// `chatgpt.com/c/<id>` of the conversation, once a turn completed.
    pub(crate) conversation_id: Option<String>,
    /// Codex model slug (`chatgpt-web/thinking`) the conversation was opened
    /// with. A different slug cannot extend it.
    pub(crate) model_slug: Option<String>,
    /// How many leading history items that conversation has already seen.
    pub(crate) delivered_items: usize,
    /// Fingerprint of those leading items.
    pub(crate) delivered_fingerprint: u64,
    /// Fingerprints of the items ChatGPT itself produced (and the provider's own
    /// commentary), so the next request's tail does not read them back.
    pub(crate) echoed: Vec<u64>,
    /// The last message landed in the conversation but its reply was never
    /// recorded (interrupt, stall, upstream error). The next extension says so
    /// instead of pretending the previous turn never happened.
    pub(crate) message_landed_unanswered: bool,
}

/// What the provider should send for one request.
#[derive(Debug)]
pub(crate) struct RequestPlan<'a> {
    /// Items to render: the undelivered tail on an extension, the whole
    /// history on a replay.
    pub(crate) items: Vec<&'a ResponseItem>,
    /// Start a new ChatGPT conversation instead of extending the recorded one.
    pub(crate) restart: bool,
    /// Number of history items this request leaves delivered.
    pub(crate) delivered_items: usize,
    /// Fingerprint of the delivered prefix.
    pub(crate) delivered_fingerprint: u64,
    /// The request is Codex's own history-compaction turn: answer it from a
    /// disposable conversation and leave the continuity untouched.
    pub(crate) is_compaction: bool,
}

/// Decides between extending the recorded conversation and replaying history.
///
/// `compact_prompt` is the summarization prompt Codex appends as the last user
/// message of a compaction turn; when the request ends with it the plan is a
/// replay into a throwaway conversation.
pub(crate) fn plan_request<'a>(
    input: &'a [ResponseItem],
    continuity: &ConversationContinuity,
    model_slug: &str,
    compact_prompt: Option<&str>,
) -> RequestPlan<'a> {
    let is_compaction = compact_prompt.is_some_and(|prompt| ends_with_user_text(input, prompt));
    if is_compaction {
        return RequestPlan {
            items: input.iter().collect(),
            restart: true,
            delivered_items: input.len(),
            delivered_fingerprint: fingerprint(input),
            is_compaction: true,
        };
    }

    let can_extend = continuity.conversation_id.is_some()
        && continuity.model_slug.as_deref() == Some(model_slug)
        && continuity.delivered_items > 0
        && continuity.delivered_items <= input.len()
        && fingerprint(&input[..continuity.delivered_items]) == continuity.delivered_fingerprint;

    let (tail, restart) = if can_extend {
        (&input[continuity.delivered_items..], false)
    } else {
        (input, true)
    };

    // A replay rebuilds the whole conversation, so ChatGPT's own turns belong
    // in it. Extending does not: the live conversation already holds them.
    let items: Vec<&ResponseItem> = if restart || continuity.echoed.is_empty() {
        tail.iter().collect()
    } else {
        let echoed: HashSet<u64> = continuity.echoed.iter().copied().collect();
        tail.iter()
            .filter(|item| !echoed.contains(&item_fingerprint(item)))
            .collect()
    };

    RequestPlan {
        items,
        restart,
        delivered_items: input.len(),
        delivered_fingerprint: fingerprint(input),
        is_compaction: false,
    }
}

/// Whether the last item is a user message whose text is exactly `text`.
fn ends_with_user_text(input: &[ResponseItem], text: &str) -> bool {
    let Some(ResponseItem::Message { role, content, .. }) = input.last() else {
        return false;
    };
    if role != "user" {
        return false;
    }
    let rendered: String = content
        .iter()
        .filter_map(|item| match item {
            ContentItem::InputText { text } | ContentItem::OutputText { text } => {
                Some(text.as_str())
            }
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n");
    rendered.trim() == text.trim()
}

#[cfg(test)]
#[path = "history_tests.rs"]
mod tests;
