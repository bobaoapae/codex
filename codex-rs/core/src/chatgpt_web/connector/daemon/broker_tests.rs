use super::*;
use crate::chatgpt_web::connector::contract::ToolKind;
use pretty_assertions::assert_eq;
use serde_json::json;

const TOKEN_A: &str = "turn_aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
const TOKEN_B: &str = "turn_bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";

fn config() -> BrokerConfig {
    BrokerConfig {
        call_timeout: Duration::from_millis(500),
        batch_window: Duration::from_millis(15),
        heartbeat_timeout: Duration::from_millis(200),
        retire_cap: 2,
        exec_default_yield_ms: 10_000,
    }
}

fn tools() -> Arc<[ToolSummary]> {
    vec![ToolSummary {
        name: "exec_command".into(),
        namespace: None,
        kind: ToolKind::Function,
        description: "run".into(),
        schema: None,
    }]
    .into()
}

fn registration(session: &str, token: &str, ttl: Duration) -> TurnRegistration {
    TurnRegistration {
        session_id: session.into(),
        turn_token: token.into(),
        trace: format!("thread/{token}"),
        ttl,
        tools: tools(),
        exec_tool: ExecTool::ExecCommand,
        apply_patch: true,
    }
}

fn exec(cmd: &str) -> CallTarget {
    CallTarget::Function {
        namespace: None,
        name: "exec_command".into(),
        arguments: json!({"cmd": cmd}),
    }
}

fn ok(text: &str) -> BrokerResult {
    BrokerResult {
        content: vec![ResultContent::Text { text: text.into() }],
        is_error: false,
        structured: None,
    }
}

#[tokio::test]
async fn a_claim_mints_one_binding_and_repeats_it() {
    let broker = TurnBroker::new(config());
    broker.register_session("s1", 1);
    broker
        .register_turn(registration("s1", TOKEN_A, Duration::from_secs(60)))
        .expect("registers");

    let first = broker.claim(TOKEN_A).expect("claims");
    let second = broker.claim(TOKEN_A).expect("claims again");
    assert_eq!(first.binding, second.binding);
    assert_eq!(first.tools.apply_patch, true);
    assert_eq!(first.tools.exec_tool, ExecTool::ExecCommand);
    assert_eq!(broker.stats(), (1, 1));
}

#[tokio::test]
async fn unknown_expired_and_orphaned_tokens_have_distinct_messages() {
    let broker = TurnBroker::new(config());
    assert!(matches!(broker.claim(TOKEN_A), Err(ClaimError::Unknown)));

    broker.register_session("s1", 1);
    broker
        .register_turn(registration("s1", TOKEN_A, Duration::from_millis(0)))
        .expect("registers");
    assert!(matches!(broker.claim(TOKEN_A), Err(ClaimError::Expired)));

    broker
        .register_turn(registration("s1", TOKEN_B, Duration::from_secs(60)))
        .expect("registers");
    broker.remove_session("s1", "Codex session disconnected");
    let err = broker.claim(TOKEN_B).expect_err("retired");
    assert!(matches!(err, ClaimError::Retired { .. }));
    assert!(err.to_string().contains("already finished"));
    assert!(err.to_string().contains("thread/"));
}

#[tokio::test]
async fn duplicate_and_orphan_registrations_are_rejected() {
    let broker = TurnBroker::new(config());
    assert_eq!(
        broker.register_turn(registration("nope", TOKEN_A, Duration::from_secs(1))),
        Err(RegisterTurnError::UnknownSession)
    );
    broker.register_session("s1", 1);
    broker
        .register_turn(registration("s1", TOKEN_A, Duration::from_secs(1)))
        .expect("registers");
    assert_eq!(
        broker.register_turn(registration("s1", TOKEN_A, Duration::from_secs(1))),
        Err(RegisterTurnError::Duplicate)
    );
}

#[tokio::test]
async fn calls_within_the_window_are_delivered_as_one_batch_and_completed_individually() {
    let broker = TurnBroker::new(config());
    broker.register_session("s1", 1);
    broker
        .register_turn(registration("s1", TOKEN_A, Duration::from_secs(60)))
        .expect("registers");
    let claim = broker.claim(TOKEN_A).expect("claims");

    let b1 = Arc::clone(&broker);
    let binding1 = claim.binding.clone();
    let call_one = tokio::spawn(async move { b1.invoke(&binding1, exec("one")).await });
    let b2 = Arc::clone(&broker);
    let binding2 = claim.binding.clone();
    let call_two = tokio::spawn(async move { b2.invoke(&binding2, exec("two")).await });

    let polled = broker
        .next_batches("s1", 0, Duration::from_secs(1))
        .await
        .expect("session");
    assert_eq!(polled.batches.len(), 1, "one batch: {polled:?}");
    assert_eq!(polled.batches[0].turn_token, TOKEN_A);
    assert_eq!(polled.batches[0].calls.len(), 2);
    assert!(broker.has_in_flight(TOKEN_A));

    let ids: Vec<String> = polled.batches[0]
        .calls
        .iter()
        .map(|call| call.call_id.clone())
        .collect();
    for (id, text) in ids.iter().zip(["done one", "done two"]) {
        assert!(id.starts_with("call_"));
        broker.complete("s1", id, ok(text)).expect("completes");
    }
    let results = (call_one.await.expect("join"), call_two.await.expect("join"));
    assert!(!results.0.is_error && !results.1.is_error);
    assert!(!broker.has_in_flight(TOKEN_A));

    // Acked: nothing left to redeliver.
    let again = broker
        .next_batches("s1", polled.seq, Duration::from_millis(20))
        .await
        .expect("session");
    assert!(again.batches.is_empty());
}

#[tokio::test]
async fn an_unacked_batch_is_redelivered_until_the_seq_is_echoed() {
    let broker = TurnBroker::new(config());
    broker.register_session("s1", 1);
    broker
        .register_turn(registration("s1", TOKEN_A, Duration::from_secs(60)))
        .expect("registers");
    let claim = broker.claim(TOKEN_A).expect("claims");
    let b = Arc::clone(&broker);
    let binding = claim.binding.clone();
    let call = tokio::spawn(async move { b.invoke(&binding, exec("x")).await });

    let first = broker
        .next_batches("s1", 0, Duration::from_secs(1))
        .await
        .expect("session");
    assert_eq!(first.batches.len(), 1);
    let redelivered = broker
        .next_batches("s1", 0, Duration::from_millis(10))
        .await
        .expect("session");
    assert_eq!(redelivered, first, "same batch again until acked");

    let call_id = first.batches[0].calls[0].call_id.clone();
    broker.complete("s1", &call_id, ok("y")).expect("completes");
    let acked = broker
        .next_batches("s1", first.seq, Duration::from_millis(10))
        .await
        .expect("session");
    assert!(acked.batches.is_empty());
    assert!(!call.await.expect("join").is_error);
}

#[tokio::test]
async fn completing_requires_delivery_and_the_owning_session() {
    let broker = TurnBroker::new(config());
    broker.register_session("s1", 1);
    broker.register_session("s2", 2);
    broker
        .register_turn(registration("s1", TOKEN_A, Duration::from_secs(60)))
        .expect("registers");
    let claim = broker.claim(TOKEN_A).expect("claims");
    let b = Arc::clone(&broker);
    let binding = claim.binding.clone();
    let call = tokio::spawn(async move { b.invoke(&binding, exec("x")).await });

    assert_eq!(
        broker.complete("s1", "call_missing", ok("")),
        Err(CompleteError::UnknownCall)
    );
    let batch = broker
        .next_batches("s1", 0, Duration::from_secs(1))
        .await
        .expect("session");
    let call_id = batch.batches[0].calls[0].call_id.clone();
    assert_eq!(
        broker.complete("s2", &call_id, ok("")),
        Err(CompleteError::WrongSession)
    );
    broker
        .complete("s1", &call_id, ok("fine"))
        .expect("completes");
    assert_eq!(call.await.expect("join"), ok("fine"));
}

#[tokio::test]
async fn a_timed_out_call_tells_chatgpt_how_to_poll_instead() {
    let broker = TurnBroker::new(config());
    broker.register_session("s1", 1);
    broker
        .register_turn(registration("s1", TOKEN_A, Duration::from_secs(60)))
        .expect("registers");
    let claim = broker.claim(TOKEN_A).expect("claims");
    let result = broker.invoke(&claim.binding, exec("sleep")).await;
    assert!(result.is_error);
    let ResultContent::Text { text } = &result.content[0] else {
        panic!("text");
    };
    assert!(text.contains("did not finish exec_command"), "{text}");
    assert!(text.contains("codex_write_stdin"));
    assert!(!broker.has_in_flight(TOKEN_A));
}

#[tokio::test]
async fn session_death_fails_pending_calls_and_retires_its_turns() {
    let broker = TurnBroker::new(config());
    broker.register_session("s1", 1);
    broker
        .register_turn(registration("s1", TOKEN_A, Duration::from_secs(60)))
        .expect("registers");
    let claim = broker.claim(TOKEN_A).expect("claims");
    let b = Arc::clone(&broker);
    let binding = claim.binding.clone();
    let call = tokio::spawn(async move { b.invoke(&binding, exec("x")).await });
    broker
        .next_batches("s1", 0, Duration::from_secs(1))
        .await
        .expect("delivered");

    tokio::time::sleep(Duration::from_millis(250)).await;
    broker.sweep();

    let result = call.await.expect("join");
    assert!(result.is_error);
    assert_eq!(
        result.content,
        vec![ResultContent::Text {
            text: "Codex session disconnected".into()
        }]
    );
    assert_eq!(broker.stats(), (0, 0));
    assert!(matches!(
        broker.claim(TOKEN_A),
        Err(ClaimError::Retired { .. })
    ));
}

#[tokio::test]
async fn the_retired_list_is_bounded() {
    let broker = TurnBroker::new(config());
    broker.register_session("s1", 1);
    for token in [TOKEN_A, TOKEN_B, "turn_cccccccccccccccccccccccccccccccc"] {
        broker
            .register_turn(registration("s1", token, Duration::from_secs(60)))
            .expect("registers");
        broker.revoke(token, "done");
    }
    // Cap is 2: the oldest is forgotten and reads as unknown.
    assert!(matches!(broker.claim(TOKEN_A), Err(ClaimError::Unknown)));
    assert!(matches!(
        broker.claim(TOKEN_B),
        Err(ClaimError::Retired { .. })
    ));
}

#[tokio::test]
async fn an_expired_turn_is_swept_and_reads_as_finished() {
    let broker = TurnBroker::new(config());
    broker.register_session("s1", 1);
    broker
        .register_turn(registration("s1", TOKEN_A, Duration::from_millis(1)))
        .expect("registers");
    tokio::time::sleep(Duration::from_millis(5)).await;
    broker.sweep();
    let err = broker.claim(TOKEN_A).expect_err("retired");
    assert!(err.to_string().contains("expired"), "{err}");
    // The session is still alive: it polled recently.
    assert_eq!(broker.stats(), (1, 0));
}
