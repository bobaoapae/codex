//! FORK: turns a provider's free-form stream of thinking and text into the
//! item discipline Codex's turn loop expects.
//!
//! Shared by the `claude_code` and `chatgpt_web` providers: both produce
//! interleaved reasoning and answer text without the Responses API's item
//! framing, and both must open an item before any delta and close it on
//! `OutputItemDone`.

use super::history;
use crate::client_common::ResponseEvent;
use codex_protocol::error::Result;
use codex_protocol::models::ContentItem;
use codex_protocol::models::MessagePhase;
use codex_protocol::models::ReasoningItemReasoningSummary;
use codex_protocol::models::ResponseItem;
use tokio::sync::mpsc;

/// Turns Claude's block stream into Codex items.
///
/// Codex's turn loop refuses a delta with no item open (`error_or_panic`) and
/// closes the open item on `OutputItemDone`. Claude interleaves thinking, tool
/// calls and answer text freely, so each run of same-kind blocks becomes one
/// Codex item: open on the first block of a run, close when the kind changes.
pub(crate) struct StreamAssembler<'a> {
    tx: &'a mpsc::Sender<Result<ResponseEvent>>,
    active: Option<ActiveItem>,
    streamed_any_text: bool,
    /// FORK: fingerprints of every item this turn produced, so the next request
    /// can drop them from its tail instead of reading them back to Claude.
    authored: Vec<u64>,
    /// FORK: whether the partial stream already painted the block that is being
    /// completed, so the completed block records without repainting it.
    painted_via_deltas: bool,
}

pub(crate) enum ActiveItem {
    Reasoning(String),
    Message(String),
}

impl<'a> StreamAssembler<'a> {
    pub(crate) fn new(tx: &'a mpsc::Sender<Result<ResponseEvent>>) -> Self {
        Self {
            tx,
            active: None,
            streamed_any_text: false,
            authored: Vec::new(),
            painted_via_deltas: false,
        }
    }

    pub(crate) fn streamed_any_text(&self) -> bool {
        self.streamed_any_text
    }

    pub(crate) fn take_authored(&mut self) -> Vec<u64> {
        std::mem::take(&mut self.authored)
    }

    /// Sends a finished item and remembers it as this turn's own output.
    pub(crate) async fn send_done(&mut self, item: ResponseItem) -> bool {
        self.authored.push(history::item_fingerprint(&item));
        self.send(ResponseEvent::OutputItemDone(item)).await
    }

    /// FORK: paints streamed assistant text as the CLI produces it.
    ///
    /// The completed `assistant` block still arrives afterwards and is what
    /// records the item; these deltas only paint, so the text is never
    /// accumulated here. Two things this must get right:
    ///
    /// - an item has to be open first. A delta with no active item is a harness
    ///   invariant violation, and in a debug build it panics outright;
    /// - the completed block must not repaint what the deltas already showed,
    ///   which is what `painted_via_deltas` tracks.
    pub(crate) async fn push_text_delta(&mut self, text: &str) -> bool {
        if !matches!(self.active, Some(ActiveItem::Message(_))) {
            if !self.close(MessagePhase::Commentary).await {
                return false;
            }
            if !self
                .send(ResponseEvent::OutputItemAdded(message_item(
                    String::new(),
                    &MessagePhase::Commentary,
                )))
                .await
            {
                return false;
            }
            self.active = Some(ActiveItem::Message(String::new()));
        }
        self.painted_via_deltas = true;
        self.send(ResponseEvent::OutputTextDelta(text.to_string()))
            .await
    }

    /// FORK: the same, for thinking.
    pub(crate) async fn push_reasoning_delta(&mut self, text: &str) -> bool {
        if !matches!(self.active, Some(ActiveItem::Reasoning(_))) {
            if !self.close(MessagePhase::Commentary).await {
                return false;
            }
            if !self
                .send(ResponseEvent::OutputItemAdded(
                    reasoning_item(String::new()),
                ))
                .await
            {
                return false;
            }
            self.active = Some(ActiveItem::Reasoning(String::new()));
        }
        self.painted_via_deltas = true;
        self.send(ResponseEvent::ReasoningSummaryDelta {
            delta: text.to_string(),
            summary_index: 0,
        })
        .await
    }

    /// FORK: forwards a tool the provider already executed.
    ///
    /// Its history items are fingerprinted like anything else this turn
    /// produced, so the next request's tail does not replay Claude's own trace
    /// back at it.
    pub(crate) async fn send_provider_tool(
        &mut self,
        executed: codex_api::ProviderExecutedTool,
    ) -> bool {
        for item in &executed.history_items {
            self.authored.push(history::item_fingerprint(item));
        }
        self.send(ResponseEvent::ProviderExecutedTool(Box::new(executed)))
            .await
    }

    /// Sends one event; `false` means the consumer is gone and we should stop.
    pub(crate) async fn send(&self, event: ResponseEvent) -> bool {
        self.tx.send(Ok(event)).await.is_ok()
    }

    pub(crate) async fn push_text(&mut self, text: &str) -> bool {
        if !matches!(self.active, Some(ActiveItem::Message(_))) {
            if !self.close(MessagePhase::Commentary).await {
                return false;
            }
            if !self
                .send(ResponseEvent::OutputItemAdded(message_item(
                    String::new(),
                    &MessagePhase::Commentary,
                )))
                .await
            {
                return false;
            }
            self.active = Some(ActiveItem::Message(String::new()));
        }
        if let Some(ActiveItem::Message(buffer)) = self.active.as_mut() {
            buffer.push_str(text);
        }
        self.streamed_any_text = true;
        // FORK: the partial stream already showed this block character by
        // character; repeating it here would print it twice.
        if std::mem::take(&mut self.painted_via_deltas) {
            return true;
        }
        self.send(ResponseEvent::OutputTextDelta(text.to_string()))
            .await
    }

    pub(crate) async fn push_reasoning(&mut self, text: &str) -> bool {
        if !matches!(self.active, Some(ActiveItem::Reasoning(_))) {
            if !self.close(MessagePhase::Commentary).await {
                return false;
            }
            if !self
                .send(ResponseEvent::OutputItemAdded(
                    reasoning_item(String::new()),
                ))
                .await
            {
                return false;
            }
            self.active = Some(ActiveItem::Reasoning(String::new()));
        }
        if let Some(ActiveItem::Reasoning(buffer)) = self.active.as_mut() {
            buffer.push_str(text);
        }
        // FORK: as above — the partial stream already painted this block.
        if std::mem::take(&mut self.painted_via_deltas) {
            return true;
        }
        self.send(ResponseEvent::ReasoningSummaryDelta {
            delta: text.to_string(),
            // One summary part per item: Claude's blocks are a single narrative,
            // not indexed summary sections.
            summary_index: 0,
        })
        .await
    }

    /// Closes the open item, if any. `phase` applies only to assistant text.
    pub(crate) async fn close(&mut self, phase: MessagePhase) -> bool {
        match self.active.take() {
            None => true,
            Some(ActiveItem::Reasoning(text)) => self.send_done(reasoning_item(text)).await,
            Some(ActiveItem::Message(text)) => self.send_done(message_item(text, &phase)).await,
        }
    }

    /// Emits a complete assistant message that was never streamed.
    pub(crate) async fn emit_message(&mut self, text: String, phase: MessagePhase) -> bool {
        if !self
            .send(ResponseEvent::OutputItemAdded(message_item(
                text.clone(),
                &phase,
            )))
            .await
        {
            return false;
        }
        self.streamed_any_text = true;
        self.send_done(message_item(text, &phase)).await
    }
}

pub(crate) fn message_item(text: String, phase: &MessagePhase) -> ResponseItem {
    ResponseItem::Message {
        id: None,
        role: "assistant".to_string(),
        content: vec![ContentItem::OutputText { text }],
        phase: Some(phase.clone()),
        internal_chat_message_metadata_passthrough: None,
    }
}

pub(crate) fn reasoning_item(text: String) -> ResponseItem {
    ResponseItem::Reasoning {
        id: None,
        summary: vec![ReasoningItemReasoningSummary::SummaryText { text }],
        content: None,
        encrypted_content: None,
        internal_chat_message_metadata_passthrough: None,
    }
}

impl StreamAssembler<'_> {
    /// FORK: whether a reasoning item is currently open.
    pub(crate) fn reasoning_open(&self) -> bool {
        matches!(self.active, Some(ActiveItem::Reasoning(_)))
    }

    /// FORK: whether an assistant message item is currently open.
    pub(crate) fn message_open(&self) -> bool {
        matches!(self.active, Some(ActiveItem::Message(_)))
    }
}

impl StreamAssembler<'_> {
    /// FORK: opens a reasoning item if one is not already open, closing any
    /// open message as commentary. Used by providers that learn about item
    /// boundaries before they have any text for the item (chatgpt_web polls).
    pub(crate) async fn open_reasoning(&mut self) -> bool {
        if self.reasoning_open() {
            return true;
        }
        if !self.close(MessagePhase::Commentary).await {
            return false;
        }
        if !self
            .send(ResponseEvent::OutputItemAdded(
                reasoning_item(String::new()),
            ))
            .await
        {
            return false;
        }
        self.active = Some(ActiveItem::Reasoning(String::new()));
        true
    }

    /// FORK: the same, for an assistant message item.
    pub(crate) async fn open_message(&mut self) -> bool {
        if self.message_open() {
            return true;
        }
        if !self.close(MessagePhase::Commentary).await {
            return false;
        }
        if !self
            .send(ResponseEvent::OutputItemAdded(message_item(
                String::new(),
                &MessagePhase::Commentary,
            )))
            .await
        {
            return false;
        }
        self.active = Some(ActiveItem::Message(String::new()));
        true
    }
}
