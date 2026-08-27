use super::*;
use crate::claude_code::history::fingerprint;

fn user(text: &str) -> ResponseItem {
    ResponseItem::Message {
        id: None,
        role: "user".to_string(),
        content: vec![ContentItem::InputText {
            text: text.to_string(),
        }],
        phase: None,
        internal_chat_message_metadata_passthrough: None,
    }
}

fn assistant(text: &str) -> ResponseItem {
    ResponseItem::Message {
        id: None,
        role: "assistant".to_string(),
        content: vec![ContentItem::OutputText {
            text: text.to_string(),
        }],
        phase: None,
        internal_chat_message_metadata_passthrough: None,
    }
}

fn recorded(input: &[ResponseItem], model: &str) -> ConversationContinuity {
    ConversationContinuity {
        conversation_id: Some("conv-1".to_string()),
        model_slug: Some(model.to_string()),
        delivered_items: input.len(),
        delivered_fingerprint: fingerprint(input),
        echoed: Vec::new(),
        message_landed_unanswered: false,
    }
}

fn texts(plan: &RequestPlan<'_>) -> Vec<String> {
    plan.items
        .iter()
        .map(|item| crate::claude_code::history::render_item(item))
        .collect()
}

#[test]
fn a_first_request_replays_everything_into_a_new_conversation() {
    let input = vec![user("hello")];
    let plan = plan_request(
        &input,
        &ConversationContinuity::default(),
        "chatgpt-web/instant",
        None,
    );
    assert!(plan.restart);
    assert!(!plan.is_compaction);
    assert_eq!(plan.items.len(), 1);
    assert_eq!(plan.delivered_items, 1);
}

#[test]
fn a_matching_prefix_extends_with_only_the_tail() {
    let first = vec![user("hello")];
    let continuity = recorded(&first, "chatgpt-web/instant");
    let input = vec![user("hello"), assistant("hi"), user("more")];
    let plan = plan_request(&input, &continuity, "chatgpt-web/instant", None);
    assert!(!plan.restart);
    assert_eq!(
        texts(&plan),
        vec!["<assistant>\nhi\n</assistant>", "<user>\nmore\n</user>"]
    );
    assert_eq!(plan.delivered_items, 3);
}

#[test]
fn echoed_items_are_dropped_from_an_extension_but_kept_on_replay() {
    let first = vec![user("hello")];
    let mut continuity = recorded(&first, "chatgpt-web/instant");
    continuity.echoed = vec![item_fingerprint(&assistant("hi"))];
    let input = vec![user("hello"), assistant("hi"), user("more")];

    let plan = plan_request(&input, &continuity, "chatgpt-web/instant", None);
    assert!(!plan.restart);
    assert_eq!(texts(&plan), vec!["<user>\nmore\n</user>"]);

    // A changed prefix forces a replay, and the replay keeps the echoed item.
    let changed = vec![user("hello!"), assistant("hi"), user("more")];
    let plan = plan_request(&changed, &continuity, "chatgpt-web/instant", None);
    assert!(plan.restart);
    assert_eq!(plan.items.len(), 3);
}

#[test]
fn a_different_model_restarts_even_with_a_matching_prefix() {
    let first = vec![user("hello")];
    let continuity = recorded(&first, "chatgpt-web/instant");
    let input = vec![user("hello"), assistant("hi"), user("more")];
    let plan = plan_request(&input, &continuity, "chatgpt-web/pro", None);
    assert!(plan.restart);
    assert_eq!(plan.items.len(), 3);
}

#[test]
fn a_shorter_history_than_delivered_restarts() {
    let first = vec![user("hello"), assistant("hi")];
    let continuity = recorded(&first, "chatgpt-web/instant");
    let input = vec![user("hello")];
    let plan = plan_request(&input, &continuity, "chatgpt-web/instant", None);
    assert!(plan.restart);
}

#[test]
fn the_compaction_prompt_is_a_disposable_replay() {
    let first = vec![user("hello")];
    let continuity = recorded(&first, "chatgpt-web/instant");
    let input = vec![
        user("hello"),
        assistant("hi"),
        user("Summarize the conversation so far."),
    ];
    let plan = plan_request(
        &input,
        &continuity,
        "chatgpt-web/instant",
        Some("Summarize the conversation so far.\n"),
    );
    assert!(plan.is_compaction);
    assert!(plan.restart);
    assert_eq!(
        plan.items.len(),
        3,
        "a compaction replays the whole history"
    );
}

#[test]
fn an_ordinary_user_message_is_not_mistaken_for_compaction() {
    let input = vec![user("hello")];
    let plan = plan_request(
        &input,
        &ConversationContinuity::default(),
        "chatgpt-web/instant",
        Some("Summarize the conversation so far."),
    );
    assert!(!plan.is_compaction);
}
