use super::*;
use crate::chatgpt_web::driver::api::RawConversation;
use crate::chatgpt_web::driver::api::normalize;
use std::sync::Mutex;

fn fixture(name: &str) -> Conversation {
    let raw: RawConversation = match name {
        "in_progress" => serde_json::from_str(include_str!("fixtures/conv_in_progress.json")),
        "finished" => serde_json::from_str(include_str!("fixtures/conv_finished.json")),
        "thoughts" => serde_json::from_str(include_str!("fixtures/conv_thoughts.json")),
        "image_assets" => serde_json::from_str(include_str!("fixtures/conv_image_assets.json")),
        "api_tool" => serde_json::from_str(include_str!("fixtures/conv_api_tool.json")),
        "stopped_old" => {
            serde_json::from_str(include_str!("fixtures/conv_stopped_old_in_progress.json"))
        }
        other => panic!("unknown fixture {other}"),
    }
    .expect("fixture parses");
    normalize(&raw)
}

fn last_user_text(conv: &Conversation) -> String {
    let index = conv.last_user_turn_index().expect("user turn");
    conv.turns[index].text.clone()
}

fn kinds(deltas: &[Delta]) -> Vec<&'static str> {
    deltas
        .iter()
        .map(|delta| match delta {
            Delta::OpenReasoning { .. } => "open_reasoning",
            Delta::Reasoning(_) => "reasoning",
            Delta::OpenText { .. } => "open_text",
            Delta::Text(_) => "text",
            Delta::Rewrite { .. } => "rewrite",
            Delta::Note(_) => "note",
            Delta::Progress => "progress",
            Delta::PartialCompletion { .. } => "partial",
            Delta::Done { .. } => "done",
        })
        .collect()
}

#[test]
fn an_unanchored_snapshot_yields_nothing() {
    let conv = fixture("finished");
    let mut tracker = ReplyTracker::new("something we never sent");
    assert!(tracker.observe(&conv, TrackMode::None).is_empty());
}

#[test]
fn a_finished_reply_opens_text_and_completes_on_end_turn() {
    let conv = fixture("finished");
    let mut tracker = ReplyTracker::new(&last_user_text(&conv));
    let deltas = tracker.observe(&conv, TrackMode::None);
    assert_eq!(
        kinds(&deltas),
        vec!["open_text", "text", "progress", "done"]
    );
    assert!(matches!(
        deltas.last(),
        Some(Delta::Done {
            reason: DoneReason::EndTurn
        })
    ));
    assert_eq!(
        tracker.text_chars(),
        "O arquivo contém três notas curtas sobre Rust."
            .chars()
            .count()
    );
    // Once done, nothing more is ever emitted.
    assert!(tracker.observe(&conv, TrackMode::None).is_empty());
}

#[test]
fn an_in_progress_reply_streams_reasoning_then_text_and_does_not_complete() {
    let conv = fixture("in_progress");
    let mut tracker = ReplyTracker::new(&last_user_text(&conv));
    let deltas = tracker.observe(&conv, TrackMode::None);
    assert_eq!(
        kinds(&deltas),
        vec![
            "open_reasoning",
            "reasoning",
            "open_text",
            "text",
            "progress"
        ]
    );
    // `async_status: 1` and an in-progress message: not idle.
    assert!(
        !deltas
            .iter()
            .any(|delta| matches!(delta, Delta::Done { .. }))
    );
}

#[test]
fn growth_is_emitted_as_a_suffix_and_a_rewrite_when_not_a_prefix() {
    let mut conv = fixture("in_progress");
    let mut tracker = ReplyTracker::new(&last_user_text(&conv));
    let _ = tracker.observe(&conv, TrackMode::None);

    let text_index = conv
        .turns
        .iter()
        .rposition(|turn| turn.role == "assistant" && turn.content_type == "text")
        .expect("text turn");
    conv.turns[text_index].text.push_str(" E mais.");
    let deltas = tracker.observe(&conv, TrackMode::None);
    assert_eq!(kinds(&deltas), vec!["text", "progress"]);
    assert_eq!(deltas[0], Delta::Text(" E mais.".to_string()));

    conv.turns[text_index].text = "Texto regenerado.".to_string();
    let deltas = tracker.observe(&conv, TrackMode::None);
    assert_eq!(kinds(&deltas), vec!["rewrite", "progress"]);
    assert!(matches!(
        &deltas[0],
        Delta::Rewrite { kind: ItemKind::Text, full_text, .. } if full_text == "Texto regenerado."
    ));
}

#[test]
fn thoughts_then_text_become_reasoning_then_text() {
    let conv = fixture("thoughts");
    let mut tracker = ReplyTracker::new(&last_user_text(&conv));
    let deltas = tracker.observe(&conv, TrackMode::None);
    assert_eq!(
        kinds(&deltas),
        vec![
            "open_reasoning",
            "reasoning",
            "open_text",
            "text",
            "progress",
            "done"
        ]
    );
    let Delta::Reasoning(reasoning) = &deltas[1] else {
        panic!("expected reasoning");
    };
    assert!(!reasoning.is_empty());
}

#[test]
fn an_unchanged_idle_conversation_does_not_report_progress() {
    let conv = fixture("in_progress");
    let mut tracker = ReplyTracker::new(&last_user_text(&conv));
    let _ = tracker.observe(&conv, TrackMode::None);
    let deltas = tracker.observe(&conv, TrackMode::None);
    assert!(deltas.is_empty(), "got {deltas:?}");
}

#[test]
fn assets_complete_only_once_the_snapshot_is_stable() {
    let conv = fixture("image_assets");
    let mut tracker = ReplyTracker::new(&last_user_text(&conv));
    let first = tracker.observe(&conv, TrackMode::None);
    // The final assistant text carries end_turn: true, so the reply completes
    // right away — and the generated image was noted before it.
    assert!(
        first.iter().any(
            |delta| matches!(delta, Delta::Note(note) if note.starts_with("[generated image:"))
        )
    );
    assert!(matches!(first.last(), Some(Delta::Done { .. })));
}

#[test]
fn an_asset_only_reply_completes_on_stability() {
    let mut conv = fixture("image_assets");
    // Drop the closing text so only the tool asset remains.
    conv.turns
        .retain(|turn| !(turn.role == "assistant" && turn.end_turn == Some(true)));
    let mut tracker = ReplyTracker::new(&last_user_text(&conv));
    let first = tracker.observe(&conv, TrackMode::None);
    assert!(
        !first
            .iter()
            .any(|delta| matches!(delta, Delta::Done { .. }))
    );
    let second = tracker.observe(&conv, TrackMode::None);
    assert!(matches!(
        second.last(),
        Some(Delta::Done {
            reason: DoneReason::Stable
        })
    ));
}

#[test]
fn a_pending_api_tool_request_blocks_completion_only_in_connector_mode() {
    let conv = fixture("api_tool");
    assert!(
        conv.api_tool_requests
            .iter()
            .any(|request| !request.has_result),
        "fixture must have a pending request"
    );
    let mut connector = ReplyTracker::new(&last_user_text(&conv));
    for _ in 0..STABLE_POLLS_WITHOUT_ASSETS + 2 {
        let deltas = connector.observe(&conv, TrackMode::Connector);
        assert!(
            !deltas
                .iter()
                .any(|delta| matches!(delta, Delta::Done { .. }))
        );
    }
    // Without a connector the same idle snapshot eventually settles (fallback).
    let mut none = ReplyTracker::new(&last_user_text(&conv));
    let mut done = false;
    for _ in 0..STABLE_POLLS_WITHOUT_ASSETS + 2 {
        if none
            .observe(&conv, TrackMode::None)
            .iter()
            .any(|delta| matches!(delta, Delta::Done { .. }))
        {
            done = true;
        }
    }
    // The fixture has no assistant prose at all, so even the fallback waits.
    assert!(!done);
}

#[test]
fn an_old_in_progress_message_before_the_last_user_turn_does_not_block() {
    let conv = fixture("stopped_old");
    let mut tracker = ReplyTracker::new(&last_user_text(&conv));
    let deltas = tracker.observe(&conv, TrackMode::None);
    assert!(matches!(
        deltas.last(),
        Some(Delta::Done {
            reason: DoneReason::EndTurn
        })
    ));
    let Delta::Text(text) = &deltas[1] else {
        panic!("expected the haiku, got {deltas:?}");
    };
    assert!(text.starts_with("Luz fria"));
}

#[test]
fn a_partial_completion_is_reported_once_stable() {
    let mut conv = fixture("finished");
    let last = conv.turns.len() - 1;
    conv.turns[last].status = "finished_partial_completion".to_string();
    conv.turns[last].end_turn = None;
    let mut tracker = ReplyTracker::new(&last_user_text(&conv));
    let first = tracker.observe(&conv, TrackMode::None);
    assert!(
        !first
            .iter()
            .any(|delta| matches!(delta, Delta::PartialCompletion { .. }))
    );
    let second = tracker.observe(&conv, TrackMode::None);
    assert!(matches!(
        second.last(),
        Some(Delta::PartialCompletion { .. })
    ));
}

/// Canned snapshots returned in order; the last one repeats.
struct Snapshots(Mutex<Vec<DriverResult<Conversation>>>);

impl ConversationSource for Snapshots {
    fn read<'a>(&'a self, _: &'a str) -> BoxFuture<'a, DriverResult<Conversation>> {
        Box::pin(async move {
            let mut snapshots = self.0.lock().unwrap();
            if snapshots.len() > 1 {
                snapshots.remove(0)
            } else {
                snapshots[0].clone()
            }
        })
    }
}

fn poll_loop<'a>(source: &'a dyn ConversationSource, anchor: &str) -> PollLoop<'a> {
    PollLoop {
        source,
        conversation_id: "conv".to_string(),
        tracker: ReplyTracker::new(anchor),
        mode: TrackMode::None,
        poll_interval: Duration::from_millis(5),
        idle_timeout: Some(Duration::from_secs(5)),
        sent_at: Instant::now(),
        dom: None,
        anchor: anchor.to_string(),
    }
}

#[tokio::test]
async fn the_poll_loop_emits_items_in_order_and_completes() {
    let in_progress = fixture("in_progress");
    let mut finished = in_progress.clone();
    finished.is_generating = false;
    finished.any_in_progress = false;
    finished.async_status = None;
    for turn in &mut finished.turns {
        if turn.role == "assistant" && turn.content_type == "text" {
            turn.status = "finished_successfully".to_string();
            turn.end_turn = Some(true);
            turn.text.push_str(" Fim.");
        }
    }
    let anchor = last_user_text(&in_progress);
    let source = Snapshots(Mutex::new(vec![Ok(in_progress), Ok(finished)]));
    let (tx, mut rx) = tokio::sync::mpsc::channel(64);
    let mut assembler = StreamAssembler::new(&tx);
    let consumer_dropped = CancellationToken::new();

    let outcome = poll_loop(&source, &anchor)
        .run(&mut assembler, &consumer_dropped)
        .await;

    assert!(matches!(
        outcome,
        PollOutcome::Done {
            reason: DoneReason::EndTurn,
            ..
        }
    ));
    drop(tx);
    let mut events = Vec::new();
    while let Some(event) = rx.recv().await {
        events.push(event.expect("ok"));
    }
    let names: Vec<&str> = events
        .iter()
        .map(|event| match event {
            ResponseEvent::OutputItemAdded(codex_protocol::models::ResponseItem::Reasoning {
                ..
            }) => "added_reasoning",
            ResponseEvent::OutputItemAdded(_) => "added_message",
            ResponseEvent::OutputItemDone(codex_protocol::models::ResponseItem::Reasoning {
                ..
            }) => "done_reasoning",
            ResponseEvent::OutputItemDone(_) => "done_message",
            ResponseEvent::ReasoningSummaryPartAdded { .. } => "part_added",
            ResponseEvent::ReasoningSummaryDelta { .. } => "reasoning_delta",
            ResponseEvent::OutputTextDelta(_) => "text_delta",
            _ => "other",
        })
        .collect();
    assert_eq!(
        names,
        vec![
            "added_reasoning",
            "part_added",
            "reasoning_delta",
            "done_reasoning",
            "added_message",
            "text_delta",
            "text_delta",
            "done_message",
        ]
    );
    let Some(ResponseEvent::OutputItemDone(codex_protocol::models::ResponseItem::Message {
        phase,
        content,
        ..
    })) = events.last()
    else {
        panic!("last event should close the message");
    };
    assert_eq!(*phase, Some(MessagePhase::FinalAnswer));
    let codex_protocol::models::ContentItem::OutputText { text } = &content[0] else {
        panic!("text content");
    };
    assert!(text.ends_with(" Fim."));
}

#[tokio::test]
async fn a_dropped_consumer_interrupts_the_poll_loop() {
    let conv = fixture("in_progress");
    let anchor = last_user_text(&conv);
    let source = Snapshots(Mutex::new(vec![Ok(conv)]));
    let (tx, _rx) = tokio::sync::mpsc::channel(64);
    let mut assembler = StreamAssembler::new(&tx);
    let consumer_dropped = CancellationToken::new();
    consumer_dropped.cancel();

    let outcome = poll_loop(&source, &anchor)
        .run(&mut assembler, &consumer_dropped)
        .await;
    assert!(matches!(outcome, PollOutcome::Interrupted));
}

#[tokio::test]
async fn no_progress_past_the_idle_timeout_stalls() {
    let conv = fixture("in_progress");
    let anchor = last_user_text(&conv);
    let source = Snapshots(Mutex::new(vec![Ok(conv)]));
    let (tx, _rx) = tokio::sync::mpsc::channel(64);
    let mut assembler = StreamAssembler::new(&tx);
    let consumer_dropped = CancellationToken::new();
    let mut looped = poll_loop(&source, &anchor);
    looped.idle_timeout = Some(Duration::from_millis(60));

    let outcome = looped.run(&mut assembler, &consumer_dropped).await;
    assert!(
        matches!(outcome, PollOutcome::Stalled { .. }),
        "got {outcome:?}"
    );
}

#[tokio::test]
async fn a_404_after_the_grace_window_fails_the_turn() {
    let source = Snapshots(Mutex::new(vec![Err(DriverError::new(
        DriverErrorKind::ConversationNotFound,
        "404",
    ))]));
    let (tx, _rx) = tokio::sync::mpsc::channel(64);
    let mut assembler = StreamAssembler::new(&tx);
    let consumer_dropped = CancellationToken::new();
    let mut looped = poll_loop(&source, "x");
    looped.sent_at = Instant::now() - Duration::from_secs(60);

    let outcome = looped.run(&mut assembler, &consumer_dropped).await;
    assert!(
        matches!(outcome, PollOutcome::Failed(err) if err.kind == DriverErrorKind::ConversationNotFound)
    );
}

// ---------------------------------------------------------------------------
// FORK: DOM-driven scheduling (the 429 fix).
// ---------------------------------------------------------------------------

fn dom(anchor: &str, chars: u64, generating: bool, done: bool) -> DomProgress {
    DomProgress {
        url: "https://chatgpt.com/c/conv".to_string(),
        generating,
        streaming: u64::from(generating),
        last_user_text: anchor.to_string(),
        assistant_turns: u64::from(chars > 0),
        assistant_chars: chars,
        last_assistant_done: done,
        last_assistant_id: (chars > 0).then(|| "m1".to_string()),
    }
}

#[test]
fn the_scheduler_reads_the_api_only_when_the_page_finishes_or_an_interval_elapses() {
    let t0 = Instant::now();
    let mut scheduler = PollScheduler::with_intervals(
        t0,
        Duration::from_secs(30),
        Duration::from_secs(60),
        Duration::from_secs(10),
    );
    let anchor = anchor_of("hello there");

    // Streaming: the page changes every tick but the API is left alone.
    let step = scheduler.on_dom(
        Some(dom("hello there", 10, true, false)),
        &anchor,
        t0 + Duration::from_secs(3),
    );
    assert_eq!(
        step,
        DomStep {
            changed: true,
            read_api: false
        }
    );
    let step = scheduler.on_dom(
        Some(dom("hello there", 20, true, false)),
        &anchor,
        t0 + Duration::from_secs(6),
    );
    assert_eq!(
        step,
        DomStep {
            changed: true,
            read_api: false
        }
    );
    // Unchanged page: no progress, still no read.
    let step = scheduler.on_dom(
        Some(dom("hello there", 20, true, false)),
        &anchor,
        t0 + Duration::from_secs(9),
    );
    assert_eq!(
        step,
        DomStep {
            changed: false,
            read_api: false
        }
    );
    // Growth past the stream interval: one read to refresh the text.
    let step = scheduler.on_dom(
        Some(dom("hello there", 30, true, false)),
        &anchor,
        t0 + Duration::from_secs(31),
    );
    assert_eq!(
        step,
        DomStep {
            changed: true,
            read_api: true
        }
    );
    scheduler.on_api_read(t0 + Duration::from_secs(31));
    // Finished on the page: read right away (well within the interval).
    let step = scheduler.on_dom(
        Some(dom("hello there", 30, false, true)),
        &anchor,
        t0 + Duration::from_secs(33),
    );
    assert_eq!(
        step,
        DomStep {
            changed: true,
            read_api: true
        }
    );
    scheduler.on_api_read(t0 + Duration::from_secs(33));
    // Still finished but the API did not confirm yet: re-read on the
    // after-finish cadence only.
    let step = scheduler.on_dom(
        Some(dom("hello there", 30, false, true)),
        &anchor,
        t0 + Duration::from_secs(36),
    );
    assert_eq!(
        step,
        DomStep {
            changed: false,
            read_api: false
        }
    );
    let step = scheduler.on_dom(
        Some(dom("hello there", 30, false, true)),
        &anchor,
        t0 + Duration::from_secs(44),
    );
    assert_eq!(
        step,
        DomStep {
            changed: false,
            read_api: true
        }
    );
}

#[test]
fn the_scheduler_falls_back_to_slow_api_polls_without_a_page() {
    let t0 = Instant::now();
    let mut scheduler = PollScheduler::with_intervals(
        t0,
        Duration::from_secs(30),
        Duration::from_secs(60),
        Duration::from_secs(10),
    );
    let anchor = anchor_of("hello there");
    assert!(
        !scheduler
            .on_dom(None, &anchor, t0 + Duration::from_secs(5))
            .read_api
    );
    assert!(
        scheduler
            .on_dom(None, &anchor, t0 + Duration::from_secs(31))
            .read_api
    );
    // A page showing someone else's message is as good as no page.
    scheduler.on_api_read(t0 + Duration::from_secs(31));
    let foreign = dom("another prompt", 50, false, true);
    assert_eq!(
        scheduler.on_dom(Some(foreign), &anchor, t0 + Duration::from_secs(40)),
        DomStep {
            changed: false,
            read_api: false
        }
    );
    // A quiet, unfinished page is re-checked on the safety cadence.
    scheduler.on_dom(
        Some(dom("hello there", 5, true, false)),
        &anchor,
        t0 + Duration::from_secs(41),
    );
    assert!(
        !scheduler
            .on_dom(
                Some(dom("hello there", 5, true, false)),
                &anchor,
                t0 + Duration::from_secs(80)
            )
            .read_api
    );
    assert!(
        scheduler
            .on_dom(
                Some(dom("hello there", 5, true, false)),
                &anchor,
                t0 + Duration::from_secs(92)
            )
            .read_api
    );
}

/// Counts API reads and serves canned progress from the page.
struct Counted<'a> {
    api: &'a Snapshots,
    reads: std::sync::atomic::AtomicUsize,
    dom: Mutex<Vec<Option<DomProgress>>>,
}

impl ConversationSource for Counted<'_> {
    fn read<'a>(&'a self, id: &'a str) -> BoxFuture<'a, DriverResult<Conversation>> {
        self.reads.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        self.api.read(id)
    }
}

impl DomSource for Counted<'_> {
    fn read_dom<'a>(&'a self, _: &'a str) -> BoxFuture<'a, DriverResult<Option<DomProgress>>> {
        Box::pin(async move {
            let mut steps = self.dom.lock().unwrap();
            Ok(if steps.len() > 1 {
                steps.remove(0)
            } else {
                steps[0].clone()
            })
        })
    }
}

#[tokio::test]
async fn with_a_page_the_loop_reads_the_api_once_when_the_reply_finishes() {
    let finished = fixture("finished");
    let anchor = last_user_text(&finished);
    let api = Snapshots(Mutex::new(vec![Ok(finished)]));
    let source = Counted {
        api: &api,
        reads: std::sync::atomic::AtomicUsize::new(0),
        dom: Mutex::new(vec![
            Some(dom(&anchor, 0, true, false)),
            Some(dom(&anchor, 40, true, false)),
            Some(dom(&anchor, 80, true, false)),
            Some(dom(&anchor, 80, false, true)),
        ]),
    };
    let (tx, mut rx) = tokio::sync::mpsc::channel(64);
    let mut assembler = StreamAssembler::new(&tx);
    let consumer_dropped = CancellationToken::new();
    let mut lp = poll_loop(&source, &anchor);
    lp.dom = Some(&source);

    let outcome = lp.run(&mut assembler, &consumer_dropped).await;

    assert!(matches!(outcome, PollOutcome::Done { .. }), "{outcome:?}");
    assert_eq!(source.reads.load(std::sync::atomic::Ordering::SeqCst), 1);
    drop(tx);
    let mut saw_text = false;
    while let Some(event) = rx.recv().await {
        if matches!(event.expect("ok"), ResponseEvent::OutputTextDelta(_)) {
            saw_text = true;
        }
    }
    assert!(saw_text);
}

/// FORK (verified live on Pro): `async_status: 4` lingers after the final
/// message; `end_turn: true` on the newest text still completes the turn.
#[test]
fn a_finished_pro_reply_completes_despite_a_lingering_async_status() {
    let mut finished = fixture("finished");
    finished.async_status = Some(4);
    finished.is_generating = false;
    let mut tracker = ReplyTracker::new(&last_user_text(&finished));
    let deltas = tracker.observe(&finished, TrackMode::None);
    assert!(
        matches!(
            deltas.last(),
            Some(Delta::Done {
                reason: DoneReason::EndTurn
            })
        ),
        "{deltas:?}"
    );
}
