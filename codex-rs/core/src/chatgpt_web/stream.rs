//! FORK: turns successive snapshots of `/backend-api/conversation/<id>` into
//! the stream of deltas Codex's turn loop expects.
//!
//! ChatGPT has no event stream we can read from outside the page, so the
//! provider polls the conversation and diffs it. `ReplyTracker` is the pure
//! part (snapshot in, deltas out; tested on real captures) and `PollLoop` is
//! the async driver around it (timing, interrupts, the stall watchdog).

use super::driver::DriverError;
use super::driver::DriverErrorKind;
use super::driver::DriverResult;
use super::driver::api;
use super::driver::api::Conversation;
use super::driver::api::Turn;
use crate::claude_code::assembler::StreamAssembler;
use crate::client_common::ResponseEvent;
use codex_protocol::models::MessagePhase;
use futures::future::BoxFuture;
use std::collections::HashMap;
use std::collections::HashSet;
use std::time::Duration;
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

/// How the reply is judged complete.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TrackMode {
    /// `tools = "none"`.
    None,
    /// `tools = "connector"`: every `api_tool` request must have its result.
    Connector,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DoneReason {
    /// The newest answer message carries `end_turn: true`.
    EndTurn,
    /// The conversation is idle and did not change across polls.
    Stable,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ItemKind {
    Reasoning,
    Text,
}

/// One observed change.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Delta {
    OpenReasoning {
        message_id: String,
    },
    Reasoning(String),
    OpenText {
        message_id: String,
    },
    Text(String),
    /// The message's text is no longer an extension of what was emitted
    /// (regenerated/edited): close the item and start over with `full_text`.
    Rewrite {
        message_id: String,
        kind: ItemKind,
        full_text: String,
    },
    /// A tool turn produced an asset (generated image, file).
    Note(String),
    /// Something changed since the previous poll (feeds the stall watchdog).
    Progress,
    /// The newest answer ended in `finished_partial_completion` with no
    /// `end_turn`: ChatGPT stopped short (upstream error or stop button).
    PartialCompletion {
        message_id: String,
    },
    Done {
        reason: DoneReason,
    },
}

/// Polls of an idle, asset-less, unchanged conversation before its reply is
/// accepted without `end_turn: true`. Guards against a 20-minute stall when
/// ChatGPT ends a message with `end_turn: false` (observed after some tool
/// phases) while still keeping the fail-closed behaviour for the seconds the
/// stop button blinks between phases.
pub(crate) const STABLE_POLLS_WITHOUT_ASSETS: u32 = 8;

#[derive(Debug, Clone)]
struct Emitted {
    kind: ItemKind,
    text: String,
}

/// Pure diff state for one reply.
#[derive(Debug)]
pub(crate) struct ReplyTracker {
    /// First 120 chars (trimmed) of the user message we sent; the reply is
    /// only trusted once the API shows it as the last user turn.
    anchor: String,
    emitted: HashMap<String, Emitted>,
    /// Message id of the item currently open in the assembler.
    open: Option<String>,
    notes: HashSet<String>,
    last_fingerprint: Option<u64>,
    stable_polls: u32,
    /// Characters of answer text emitted so far (for the usage estimate).
    text_chars: usize,
    done: bool,
}

impl ReplyTracker {
    pub(crate) fn new(sent_text: &str) -> Self {
        Self {
            anchor: anchor_of(sent_text),
            emitted: HashMap::new(),
            open: None,
            notes: HashSet::new(),
            last_fingerprint: None,
            stable_polls: 0,
            text_chars: 0,
            done: false,
        }
    }

    pub(crate) fn text_chars(&self) -> usize {
        self.text_chars
    }

    /// Whether the conversation shows our message as its last user turn.
    fn anchored(&self, conv: &Conversation) -> bool {
        let Some(index) = conv.last_user_turn_index() else {
            return false;
        };
        anchor_of(&conv.turns[index].text) == self.anchor
    }

    /// Diffs one snapshot against what was already emitted.
    pub(crate) fn observe(&mut self, conv: &Conversation, mode: TrackMode) -> Vec<Delta> {
        if self.done || !self.anchored(conv) {
            return Vec::new();
        }
        let reply = conv.reply_turns();
        let mut deltas: Vec<Delta> = Vec::new();

        for turn in reply {
            match classify(turn) {
                Classified::Reasoning(text) => {
                    self.diff(turn, ItemKind::Reasoning, text, &mut deltas);
                }
                Classified::Text(text) => {
                    self.diff(turn, ItemKind::Text, text, &mut deltas);
                }
                Classified::Skip => {}
            }
            for asset in &turn.assets {
                if turn.role == "user" {
                    continue;
                }
                let key = format!("{}:{}", turn.message_id, asset.file_id);
                if self.notes.insert(key) {
                    let what = match asset.kind {
                        api::AssetKind::Image => "generated image",
                        api::AssetKind::File => "generated file",
                    };
                    let name = asset.name.clone().unwrap_or_else(|| asset.file_id.clone());
                    deltas.push(Delta::Note(format!("[{what}: {name}]")));
                }
            }
        }

        let fingerprint = api::fingerprint(conv);
        let changed = self.last_fingerprint != Some(fingerprint);
        if changed {
            self.stable_polls = 0;
        } else {
            self.stable_polls = self.stable_polls.saturating_add(1);
        }
        self.last_fingerprint = Some(fingerprint);
        if changed || !deltas.is_empty() {
            deltas.push(Delta::Progress);
        }

        let idle = !conv.is_generating && matches!(conv.async_status, None | Some(0));
        if !idle {
            return deltas;
        }
        let newest_text = reply
            .iter()
            .rev()
            .find(|turn| matches!(classify(turn), Classified::Text(_)));
        if let Some(turn) = newest_text
            && turn.status == "finished_partial_completion"
            && turn.end_turn != Some(true)
            && self.stable_polls >= 1
        {
            self.done = true;
            deltas.push(Delta::PartialCompletion {
                message_id: turn.message_id.clone(),
            });
            return deltas;
        }
        if mode == TrackMode::Connector
            && !conv
                .api_tool_requests
                .iter()
                .all(|request| request.has_result)
        {
            return deltas;
        }
        let done_end_turn = newest_text.is_some_and(|turn| {
            turn.end_turn == Some(true) && turn.status == "finished_successfully"
        });
        let has_assets = reply
            .iter()
            .any(|turn| turn.role != "user" && !turn.assets.is_empty());
        let done_stable = has_assets && self.stable_polls >= 1;
        let done_fallback = !has_assets
            && newest_text.is_some()
            && self.stable_polls >= STABLE_POLLS_WITHOUT_ASSETS;
        if done_end_turn {
            self.done = true;
            deltas.push(Delta::Done {
                reason: DoneReason::EndTurn,
            });
        } else if done_stable || done_fallback {
            self.done = true;
            deltas.push(Delta::Done {
                reason: DoneReason::Stable,
            });
        }
        deltas
    }

    fn diff(&mut self, turn: &Turn, kind: ItemKind, text: String, deltas: &mut Vec<Delta>) {
        let message_id = turn.message_id.clone();
        match self.emitted.get(&message_id) {
            None => {
                if text.is_empty() {
                    return;
                }
                deltas.push(open_delta(kind, &message_id));
                deltas.push(text_delta(kind, text.clone()));
                if kind == ItemKind::Text {
                    self.text_chars += text.chars().count();
                }
                self.open = Some(message_id.clone());
                self.emitted.insert(message_id, Emitted { kind, text });
            }
            Some(previous) => {
                if previous.text == text {
                    return;
                }
                let is_extension = previous.kind == kind && text.starts_with(&previous.text);
                if is_extension && self.open.as_deref() == Some(message_id.as_str()) {
                    let suffix = text[previous.text.len()..].to_string();
                    if kind == ItemKind::Text {
                        self.text_chars += suffix.chars().count();
                    }
                    deltas.push(text_delta(kind, suffix));
                } else {
                    if kind == ItemKind::Text {
                        self.text_chars += text.chars().count();
                    }
                    deltas.push(Delta::Rewrite {
                        message_id: message_id.clone(),
                        kind,
                        full_text: text.clone(),
                    });
                    self.open = Some(message_id.clone());
                }
                self.emitted.insert(message_id, Emitted { kind, text });
            }
        }
    }
}

fn anchor_of(text: &str) -> String {
    text.trim().chars().take(120).collect()
}

fn open_delta(kind: ItemKind, message_id: &str) -> Delta {
    match kind {
        ItemKind::Reasoning => Delta::OpenReasoning {
            message_id: message_id.to_string(),
        },
        ItemKind::Text => Delta::OpenText {
            message_id: message_id.to_string(),
        },
    }
}

fn text_delta(kind: ItemKind, text: String) -> Delta {
    match kind {
        ItemKind::Reasoning => Delta::Reasoning(text),
        ItemKind::Text => Delta::Text(text),
    }
}

enum Classified {
    Reasoning(String),
    Text(String),
    Skip,
}

/// What a reply turn contributes: thinking, answer text, or nothing.
///
/// Anything addressed to a tool (`recipient != all`) is a tool-call payload,
/// not prose; `reasoning_recap` and `code` blocks are ChatGPT-internal.
fn classify(turn: &Turn) -> Classified {
    let to_all = turn
        .recipient
        .as_deref()
        .is_none_or(|recipient| recipient == "all");
    match turn.role.as_str() {
        "assistant-thoughts" | "assistant" if turn.content_type == "thoughts" => {
            let text: Vec<String> = turn
                .thoughts
                .iter()
                .filter_map(|thought| {
                    let content = thought.content.as_deref().unwrap_or_default().trim();
                    let summary = thought.summary.as_deref().unwrap_or_default().trim();
                    match (summary.is_empty(), content.is_empty()) {
                        (true, true) => None,
                        (false, true) => Some(summary.to_string()),
                        (true, false) => Some(content.to_string()),
                        (false, false) => Some(format!("**{summary}**\n{content}")),
                    }
                })
                .collect();
            Classified::Reasoning(text.join("\n\n"))
        }
        "assistant"
            if to_all && matches!(turn.content_type.as_str(), "text" | "multimodal_text") =>
        {
            Classified::Text(turn.text.clone())
        }
        _ => Classified::Skip,
    }
}

/// Where a conversation snapshot comes from (the driver in production, canned
/// snapshots in tests).
pub(crate) trait ConversationSource: Send + Sync {
    fn read<'a>(&'a self, conversation_id: &'a str) -> BoxFuture<'a, DriverResult<Conversation>>;
}

/// How a poll loop ended.
#[derive(Debug)]
pub(crate) enum PollOutcome {
    Done {
        reason: DoneReason,
        text_chars: usize,
    },
    /// The consumer stopped polling the stream.
    Interrupted,
    /// No progress for `idle_timeout`.
    Stalled { seconds: u64 },
    /// ChatGPT stopped short of an answer.
    PartialCompletion,
    /// The conversation could not be read (after the tolerated window).
    Failed(DriverError),
}

/// Tolerated window of 404s right after a send, while the backend commits the
/// conversation.
const NOT_FOUND_GRACE: Duration = Duration::from_secs(30);

/// Consecutive read failures (of any other kind) before the turn fails.
const MAX_CONSECUTIVE_READ_FAILURES: u32 = 8;

pub(crate) struct PollLoop<'a> {
    pub(crate) source: &'a dyn ConversationSource,
    pub(crate) conversation_id: String,
    pub(crate) tracker: ReplyTracker,
    pub(crate) mode: TrackMode,
    pub(crate) poll_interval: Duration,
    /// `None` = wait forever.
    pub(crate) idle_timeout: Option<Duration>,
    pub(crate) sent_at: Instant,
    /// FORK: reserved for the connector mode (M6): a tool request arriving
    /// here suspends the poll. Never fires in `tools = "none"`.
    pub(crate) connector_rx: Option<tokio::sync::mpsc::Receiver<()>>,
}

impl PollLoop<'_> {
    pub(crate) async fn run(
        mut self,
        assembler: &mut StreamAssembler<'_>,
        consumer_dropped: &CancellationToken,
    ) -> PollOutcome {
        let mut last_progress = Instant::now();
        let mut read_failures: u32 = 0;
        loop {
            let idle_deadline = self
                .idle_timeout
                .map(|timeout| last_progress + timeout)
                .unwrap_or_else(|| Instant::now() + Duration::from_secs(365 * 24 * 3600));
            tokio::select! {
                biased;
                _ = consumer_dropped.cancelled() => return PollOutcome::Interrupted,
                _ = tokio::time::sleep_until(idle_deadline) => {
                    let seconds = self.idle_timeout.map(|timeout| timeout.as_secs()).unwrap_or_default();
                    return PollOutcome::Stalled { seconds };
                }
                _ = tokio::time::sleep(self.poll_interval) => {}
            }

            let conv = match self.source.read(&self.conversation_id).await {
                Ok(conv) => {
                    read_failures = 0;
                    conv
                }
                Err(err) if err.kind == DriverErrorKind::ConversationNotFound => {
                    if self.sent_at.elapsed() <= NOT_FOUND_GRACE {
                        continue;
                    }
                    return PollOutcome::Failed(err);
                }
                Err(err) if matches!(err.kind, DriverErrorKind::LoginRequired) => {
                    return PollOutcome::Failed(err);
                }
                Err(err) => {
                    read_failures += 1;
                    tracing::warn!(
                        "chatgpt_web: reading conversation failed ({read_failures}): {err}"
                    );
                    if read_failures >= MAX_CONSECUTIVE_READ_FAILURES {
                        return PollOutcome::Failed(err);
                    }
                    continue;
                }
            };

            let deltas = self.tracker.observe(&conv, self.mode);
            for delta in deltas {
                let ok = match delta {
                    Delta::Progress => {
                        last_progress = Instant::now();
                        true
                    }
                    Delta::OpenReasoning { .. } => {
                        assembler.open_reasoning().await
                            && assembler
                                .send(ResponseEvent::ReasoningSummaryPartAdded { summary_index: 0 })
                                .await
                    }
                    Delta::Reasoning(text) => assembler.push_reasoning(&text).await,
                    Delta::OpenText { .. } => assembler.open_message().await,
                    Delta::Text(text) => assembler.push_text(&text).await,
                    Delta::Rewrite {
                        kind, full_text, ..
                    } => {
                        assembler.close(MessagePhase::Commentary).await
                            && match kind {
                                ItemKind::Reasoning => {
                                    assembler.open_reasoning().await
                                        && assembler.push_reasoning(&full_text).await
                                }
                                ItemKind::Text => {
                                    assembler.open_message().await
                                        && assembler.push_text(&full_text).await
                                }
                            }
                    }
                    Delta::Note(text) => {
                        assembler.close(MessagePhase::Commentary).await
                            && assembler.emit_message(text, MessagePhase::Commentary).await
                    }
                    Delta::PartialCompletion { .. } => {
                        assembler.close(MessagePhase::Commentary).await;
                        return PollOutcome::PartialCompletion;
                    }
                    Delta::Done { reason } => {
                        if !assembler.close(MessagePhase::FinalAnswer).await {
                            return PollOutcome::Interrupted;
                        }
                        return PollOutcome::Done {
                            reason,
                            text_chars: self.tracker.text_chars(),
                        };
                    }
                };
                if !ok {
                    return PollOutcome::Interrupted;
                }
            }
        }
    }
}

#[cfg(test)]
#[path = "stream_tests.rs"]
mod tests;
