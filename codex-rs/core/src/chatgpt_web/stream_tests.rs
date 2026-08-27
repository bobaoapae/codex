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
        connector_rx: None,
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
