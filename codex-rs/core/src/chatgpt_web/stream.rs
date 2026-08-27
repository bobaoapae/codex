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
use super::driver::page_scripts::DomProgress;
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

    /// The (normalized) user text this tracker anchors on.
    pub(crate) fn anchor(&self) -> &str {
        &self.anchor
    }

    /// FORK: forgets which item is open, without forgetting what was emitted.
    ///
    /// The connector turn spans several `stream()` calls, each with a fresh
    /// assembler; after a reattach nothing is open, so a message that grew
    /// across the boundary must reopen (the `Rewrite` path) rather than emit a
    /// bare suffix into an empty assembler.
    pub(crate) fn reset_open(&mut self) {
        self.open = None;
    }

    /// Whether the conversation shows our message as its last user turn.
    fn anchored(&self, conv: &Conversation) -> bool {
        let Some(index) = conv.last_user_turn_index() else {
            return false;
        };
        anchor_matches(&conv.turns[index].text, &self.anchor)
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

        let newest_text = reply
            .iter()
            .rev()
            .find(|turn| matches!(classify(turn), Classified::Text(_)));
        // FORK (verified live): a finished Pro conversation keeps
        // `async_status: 4` after its final message landed with
        // `end_turn: true`; waiting for the status to clear held the turn
        // until the watchdog. A newest text that says the turn ended is the
        // turn ending, whatever the async flag says.
        let ended = newest_text.is_some_and(|turn| {
            turn.end_turn == Some(true) && turn.status == "finished_successfully"
        });
        let idle = !conv.is_generating && (matches!(conv.async_status, None | Some(0)) || ended);
        if !idle {
            return deltas;
        }
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

/// FORK (verified live): whether `shown` (the last user message as the API or
/// the page renders it) is the message we sent. The rendered text may carry a
/// leading `@Codex Native ` mention (connector mode) or trailing UI chrome, so
/// the check is "contains the start of what we sent", not equality.
fn anchor_matches(shown: &str, anchor: &str) -> bool {
    let shown: String = shown.split_whitespace().collect::<Vec<_>>().join(" ");
    let needle: String = anchor.split_whitespace().collect::<Vec<_>>().join(" ");
    let needle: String = needle.chars().take(80).collect();
    if needle.is_empty() {
        return false;
    }
    shown.contains(&needle)
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
pub(crate) const MAX_CONSECUTIVE_READ_FAILURES: u32 = 8;

/// FORK (verified live): `GET /backend-api/conversation/<id>` is rate limited
/// per account, and a 2.5 s poll that also retries 429s three times inside
/// the driver kept the account in "Too many requests" for minutes — every
/// read failed, the turn never saw the finished answer, and a single manual
/// GET from another tab got 429 too. After a 429 the poll backs off from
/// [`RATE_LIMIT_COOLDOWN_MIN`] doubling to [`RATE_LIMIT_COOLDOWN_MAX`] until a
/// read succeeds; 429s do not count as read failures (the stall watchdog still
/// bounds the wait).
pub(crate) const RATE_LIMIT_COOLDOWN_MIN: Duration = Duration::from_secs(20);
pub(crate) const RATE_LIMIT_COOLDOWN_MAX: Duration = Duration::from_secs(120);

/// Next cooldown after another 429: doubles, capped.
pub(crate) fn next_rate_limit_cooldown(current: Duration) -> Duration {
    (current * 2).min(RATE_LIMIT_COOLDOWN_MAX)
}

/// After a 429 the account stays close to its limit: for this long after the
/// last one, polls run no faster than [`RATE_LIMIT_SLOW_POLL`].
pub(crate) const RATE_LIMIT_SLOW_WINDOW: Duration = Duration::from_secs(300);
pub(crate) const RATE_LIMIT_SLOW_POLL: Duration = Duration::from_secs(15);

/// The poll interval to use now: the configured one, or the slow one while a
/// recent 429 is still fresh.
pub(crate) fn effective_poll_interval(
    configured: Duration,
    last_rate_limit: Option<Instant>,
) -> Duration {
    match last_rate_limit {
        Some(at) if at.elapsed() < RATE_LIMIT_SLOW_WINDOW => configured.max(RATE_LIMIT_SLOW_POLL),
        _ => configured,
    }
}

/// Where the DOM view of the conversation comes from (the driver in
/// production, canned progress in tests). `Ok(None)` = no tab shows the
/// conversation right now.
pub(crate) trait DomSource: Send + Sync {
    fn read_dom<'a>(
        &'a self,
        conversation_id: &'a str,
    ) -> BoxFuture<'a, DriverResult<Option<DomProgress>>>;
}

/// FORK (verified live): API reads are the scarce resource. While the reply
/// streams, the answer text is refreshed from the API at most every
/// [`API_STREAM_INTERVAL`]; while the DOM shows nothing changing the API is
/// still consulted every [`API_SAFETY_INTERVAL`] (a Pro run continues
/// server-side even if the tab navigated away); once the DOM says the reply
/// finished the API is read right away and then every
/// [`API_AFTER_FINISH_INTERVAL`] until it confirms `end_turn`.
pub(crate) const API_STREAM_INTERVAL: Duration = Duration::from_secs(30);
pub(crate) const API_SAFETY_INTERVAL: Duration = Duration::from_secs(60);
pub(crate) const API_AFTER_FINISH_INTERVAL: Duration = Duration::from_secs(10);

/// Decides, tick by tick, whether the backend must be read. Pure; the loops
/// feed it what the DOM showed and when the API was last read.
#[derive(Debug)]
pub(crate) struct PollScheduler {
    last_api: Option<Instant>,
    last_dom: Option<DomProgress>,
    /// Whether the DOM claims the reply is finished (kept until the API
    /// confirms or contradicts it).
    dom_finished: bool,
    stream_interval: Duration,
    safety_interval: Duration,
    after_finish_interval: Duration,
}

/// What one DOM tick decided.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct DomStep {
    /// The page changed since the previous tick (feeds the stall watchdog).
    pub(crate) changed: bool,
    /// The backend must be read now.
    pub(crate) read_api: bool,
}

impl PollScheduler {
    /// `sent_at` counts as the last backend contact: the send itself already
    /// touched the API, and the first streaming read waits a full interval.
    pub(crate) fn new(sent_at: Instant) -> Self {
        Self {
            last_api: Some(sent_at),
            last_dom: None,
            dom_finished: false,
            stream_interval: API_STREAM_INTERVAL,
            safety_interval: API_SAFETY_INTERVAL,
            after_finish_interval: API_AFTER_FINISH_INTERVAL,
        }
    }

    #[cfg(test)]
    pub(crate) fn with_intervals(
        sent_at: Instant,
        stream: Duration,
        safety: Duration,
        after_finish: Duration,
    ) -> Self {
        Self {
            stream_interval: stream,
            safety_interval: safety,
            after_finish_interval: after_finish,
            ..Self::new(sent_at)
        }
    }

    fn since_api(&self, now: Instant) -> Duration {
        self.last_api
            .map(|at| now.saturating_duration_since(at))
            .unwrap_or(Duration::MAX)
    }

    /// Whether the DOM progress describes our reply: the last user message on
    /// the page is the one we sent.
    fn anchored(progress: &DomProgress, anchor: &str) -> bool {
        anchor_matches(&progress.last_user_text, anchor)
    }

    /// Feeds one DOM observation (`None` = the tab does not show the
    /// conversation) and returns what to do.
    pub(crate) fn on_dom(
        &mut self,
        progress: Option<DomProgress>,
        anchor: &str,
        now: Instant,
    ) -> DomStep {
        let Some(progress) = progress else {
            // No page to watch: the API is the only source, on the slow cadence.
            let read_api = self.since_api(now) >= self.stream_interval;
            self.last_dom = None;
            return DomStep {
                changed: false,
                read_api,
            };
        };
        if !Self::anchored(&progress, anchor) {
            // The page has not caught up with our message yet (or shows
            // another reply); nothing to learn from it.
            self.last_dom = None;
            return DomStep {
                changed: false,
                read_api: self.since_api(now) >= self.stream_interval,
            };
        }
        let changed = self.last_dom.as_ref() != Some(&progress);
        let grew = self.last_dom.as_ref().is_none_or(|previous| {
            progress.assistant_chars > previous.assistant_chars
                || progress.assistant_turns > previous.assistant_turns
        });
        let finished = !progress.generating
            && progress.streaming == 0
            && progress.assistant_turns > 0
            && progress.last_assistant_done;
        let just_finished = finished && !self.dom_finished;
        self.dom_finished = finished;
        self.last_dom = Some(progress);
        let since_api = self.since_api(now);
        let read_api = if finished {
            just_finished || since_api >= self.after_finish_interval
        } else if grew {
            since_api >= self.stream_interval
        } else {
            since_api >= self.safety_interval
        };
        DomStep { changed, read_api }
    }

    /// The API was just read.
    pub(crate) fn on_api_read(&mut self, now: Instant) {
        self.last_api = Some(now);
    }
}

pub(crate) struct PollLoop<'a> {
    pub(crate) source: &'a dyn ConversationSource,
    pub(crate) conversation_id: String,
    pub(crate) tracker: ReplyTracker,
    pub(crate) mode: TrackMode,
    pub(crate) poll_interval: Duration,
    /// `None` = wait forever.
    pub(crate) idle_timeout: Option<Duration>,
    pub(crate) sent_at: Instant,
    /// FORK: the DOM reader; `None` polls the API alone (tests, no tab).
    pub(crate) dom: Option<&'a dyn DomSource>,
    /// The user text we sent, to anchor the DOM view on our reply.
    pub(crate) anchor: String,
}

impl PollLoop<'_> {
    pub(crate) async fn run(
        mut self,
        assembler: &mut StreamAssembler<'_>,
        consumer_dropped: &CancellationToken,
    ) -> PollOutcome {
        let mut last_progress = Instant::now();
        let mut read_failures: u32 = 0;
        let mut rate_limit_cooldown = RATE_LIMIT_COOLDOWN_MIN;
        let mut last_rate_limit: Option<Instant> = None;
        let mut scheduler = PollScheduler::new(self.sent_at);
        let anchor = anchor_of(&self.anchor);
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
                _ = tokio::time::sleep(effective_poll_interval(self.poll_interval, last_rate_limit)) => {}
            }

            // FORK: the page is the cheap source of progress; the API is read
            // only when the scheduler says so (see `PollScheduler`).
            if let Some(dom) = self.dom {
                let progress = match dom.read_dom(&self.conversation_id).await {
                    Ok(progress) => progress,
                    Err(err) => {
                        tracing::debug!("chatgpt_web: DOM progress read failed: {err}");
                        None
                    }
                };
                let step = scheduler.on_dom(progress, &anchor, Instant::now());
                if step.changed {
                    last_progress = Instant::now();
                }
                if !step.read_api {
                    continue;
                }
            }
            scheduler.on_api_read(Instant::now());

            let conv = match self.source.read(&self.conversation_id).await {
                Ok(conv) => {
                    read_failures = 0;
                    rate_limit_cooldown = RATE_LIMIT_COOLDOWN_MIN;
                    conv
                }
                Err(err) if err.kind == DriverErrorKind::RateLimited => {
                    last_rate_limit = Some(Instant::now());
                    tracing::warn!(
                        "chatgpt_web: conversation reads are rate limited; backing off {}s: {err}",
                        rate_limit_cooldown.as_secs()
                    );
                    tokio::select! {
                        biased;
                        _ = consumer_dropped.cancelled() => return PollOutcome::Interrupted,
                        _ = tokio::time::sleep(rate_limit_cooldown) => {}
                    }
                    rate_limit_cooldown = next_rate_limit_cooldown(rate_limit_cooldown);
                    continue;
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
                match apply_delta(assembler, delta, &mut last_progress).await {
                    DeltaStep::Continue => {}
                    DeltaStep::Interrupted => return PollOutcome::Interrupted,
                    DeltaStep::Partial => return PollOutcome::PartialCompletion,
                    DeltaStep::Done(reason) => {
                        return PollOutcome::Done {
                            reason,
                            text_chars: self.tracker.text_chars(),
                        };
                    }
                }
            }
        }
    }
}

/// What applying one delta means for the surrounding loop.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DeltaStep {
    /// Keep going.
    Continue,
    /// The consumer stopped polling; end the turn as interrupted.
    Interrupted,
    /// ChatGPT stopped short of an answer.
    Partial,
    /// The reply is complete.
    Done(DoneReason),
}

/// FORK: feeds one observed delta into the assembler.
///
/// Shared by the `tools = "none"` poll loop and the connector turn so both map
/// deltas onto the exact same `ResponseEvent` discipline. `Progress` bumps
/// `last_progress`, which the caller's stall watchdog reads.
pub(crate) async fn apply_delta(
    assembler: &mut StreamAssembler<'_>,
    delta: Delta,
    last_progress: &mut Instant,
) -> DeltaStep {
    let ok = match delta {
        Delta::Progress => {
            *last_progress = Instant::now();
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
                        assembler.open_message().await && assembler.push_text(&full_text).await
                    }
                }
        }
        Delta::Note(text) => {
            assembler.close(MessagePhase::Commentary).await
                && assembler.emit_message(text, MessagePhase::Commentary).await
        }
        Delta::PartialCompletion { .. } => {
            assembler.close(MessagePhase::Commentary).await;
            return DeltaStep::Partial;
        }
        Delta::Done { reason } => {
            if !assembler.close(MessagePhase::FinalAnswer).await {
                return DeltaStep::Interrupted;
            }
            return DeltaStep::Done(reason);
        }
    };
    if ok {
        DeltaStep::Continue
    } else {
        DeltaStep::Interrupted
    }
}

#[cfg(test)]
#[path = "stream_tests.rs"]
mod tests;
