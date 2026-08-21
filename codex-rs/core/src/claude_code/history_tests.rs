use super::*;
use codex_protocol::models::AgentMessageInputContent;
use codex_protocol::models::FunctionCallOutputPayload;

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

fn agent_message(text: &str) -> ResponseItem {
    ResponseItem::AgentMessage {
        id: None,
        author: "/root".to_string(),
        recipient: "/root/worker".to_string(),
        content: vec![AgentMessageInputContent::InputText {
            text: text.to_string(),
        }],
        internal_chat_message_metadata_passthrough: None,
    }
}

fn established(input: &[ResponseItem]) -> ClaudeSessionContinuity {
    ClaudeSessionContinuity {
        session_id: Some("session-1".to_string()),
        delivered_items: input.len(),
        delivered_fingerprint: fingerprint(input),
        account_dir: None,
    }
}

#[test]
fn first_request_replays_the_whole_conversation() {
    let input = vec![user("build the thing")];

    let plan = plan_request(&input, &ClaudeSessionContinuity::default());

    assert!(plan.restart_session);
    assert!(plan.turn_text.contains("<codex_transcript>"));
    assert!(plan.turn_text.contains("build the thing"));
    assert_eq!(plan.delivered_items, 1);
}

#[test]
fn follow_up_sends_only_the_new_items() {
    let delivered = vec![user("build the thing"), assistant("done")];
    let continuity = established(&delivered);
    let mut input = delivered;
    input.push(user("now add tests"));

    let plan = plan_request(&input, &continuity);

    assert!(!plan.restart_session);
    assert!(!plan.turn_text.contains("<codex_transcript>"));
    assert!(plan.turn_text.contains("now add tests"));
    assert!(!plan.turn_text.contains("build the thing"));
    assert_eq!(plan.delivered_items, 3);
}

#[test]
fn delegated_agent_message_is_rendered_as_the_current_user_instruction() {
    let plan = plan_request(
        &[agent_message(
            "Message Type: NEW_TASK\nTask name: /root/worker\nPayload:\nImplement the fix",
        )],
        &ClaudeSessionContinuity::default(),
    );

    assert!(plan.turn_text.contains("<user>"));
    assert!(plan.turn_text.contains("Implement the fix"));
    assert!(plan.turn_text.contains("parent Codex agent /root"));
    assert!(!plan.turn_text.contains("<codex_item>"));
    assert!(!plan.turn_text.contains("\"author\""));
}

#[test]
fn encrypted_agent_message_is_not_rendered_for_local_claude() {
    let encrypted = ResponseItem::AgentMessage {
        id: None,
        author: "/root".to_string(),
        recipient: "/root/worker".to_string(),
        content: vec![AgentMessageInputContent::EncryptedContent {
            encrypted_content: "ciphertext".to_string(),
        }],
        internal_chat_message_metadata_passthrough: None,
    };

    let plan = plan_request(&[encrypted], &ClaudeSessionContinuity::default());

    assert!(!plan.turn_text.contains("ciphertext"));
    assert!(!plan.turn_text.contains("<codex_item>"));
}

#[test]
fn rewritten_history_forces_a_fresh_session() {
    let delivered = vec![user("build the thing"), assistant("done")];
    let continuity = established(&delivered);
    // A compaction replaces the prefix the Claude session was built on.
    let input = vec![user("summary of earlier work"), user("now add tests")];

    let plan = plan_request(&input, &continuity);

    assert!(plan.restart_session);
    assert!(plan.turn_text.contains("summary of earlier work"));
    assert!(plan.turn_text.contains("now add tests"));
}

#[test]
fn truncated_history_forces_a_fresh_session() {
    let delivered = vec![user("one"), assistant("two"), user("three")];
    let continuity = established(&delivered);

    let plan = plan_request(&delivered[..1], &continuity);

    assert!(plan.restart_session);
}

#[test]
fn reasoning_is_not_replayed_but_tool_traffic_is() {
    let input = vec![
        ResponseItem::Reasoning {
            id: None,
            summary: Vec::new(),
            content: None,
            encrypted_content: Some("opaque".to_string()),
            internal_chat_message_metadata_passthrough: None,
        },
        ResponseItem::FunctionCall {
            id: None,
            name: "shell".to_string(),
            namespace: None,
            arguments: "{\"command\":[\"cargo\",\"test\"]}".to_string(),
            encrypted_function_args: None,
            call_id: "call-1".to_string(),
            internal_chat_message_metadata_passthrough: None,
        },
        ResponseItem::FunctionCallOutput {
            id: None,
            call_id: "call-1".to_string(),
            output: FunctionCallOutputPayload::from_text("2 passed".to_string()),
            internal_chat_message_metadata_passthrough: None,
        },
    ];

    let plan = plan_request(&input, &ClaudeSessionContinuity::default());

    assert!(!plan.turn_text.contains("opaque"));
    assert!(plan.turn_text.contains("cargo"));
    assert!(plan.turn_text.contains("2 passed"));
}

#[test]
fn oversized_tool_output_is_elided_in_the_middle() {
    let output = format!("HEAD{}TAIL", "x".repeat(MAX_TOOL_OUTPUT_CHARS * 2));
    let input = vec![ResponseItem::FunctionCallOutput {
        id: None,
        call_id: "call-1".to_string(),
        output: FunctionCallOutputPayload::from_text(output),
        internal_chat_message_metadata_passthrough: None,
    }];

    let plan = plan_request(&input, &ClaudeSessionContinuity::default());

    assert!(plan.turn_text.contains("HEAD"));
    assert!(plan.turn_text.contains("TAIL"));
    assert!(plan.turn_text.contains("characters elided"));
}

#[test]
fn harness_scaffolding_is_stripped_but_role_instructions_survive() {
    let developer = ResponseItem::Message {
        id: None,
        role: "developer".to_string(),
        content: vec![ContentItem::InputText {
            text: "Voce e um agente Claude sob a direcao do agente principal.\n\n\
## Memory\n\nYou have access to a memory folder at C:\\Users\\Joao\\.codex\\memories."
                .to_string(),
        }],
        phase: None,
        internal_chat_message_metadata_passthrough: None,
    };
    let team_prompt = ResponseItem::Message {
        id: None,
        role: "developer".to_string(),
        content: vec![ContentItem::InputText {
            text: "You are an agent in a team of agents collaborating to complete a task. \
Use `spawn_agent` and `functions.exec`."
                .to_string(),
        }],
        phase: None,
        internal_chat_message_metadata_passthrough: None,
    };

    let plan = plan_request(
        &[developer, team_prompt, user("faca a auditoria")],
        &ClaudeSessionContinuity::default(),
    );

    assert!(plan.turn_text.contains("sob a direcao do agente principal"));
    assert!(plan.turn_text.contains("faca a auditoria"));
    assert!(!plan.turn_text.contains("memory folder"));
    assert!(!plan.turn_text.contains("spawn_agent"));
}

#[test]
fn harness_sections_are_stripped_with_crlf_line_endings() {
    // The harness emits CRLF; a marker that carries its own "\n" never matches.
    let developer = ResponseItem::Message {
        id: None,
        role: "developer".to_string(),
        content: vec![ContentItem::InputText {
            text: "Instrucoes do role.\r\n\r\n## Memory\r\n\r\nYou have access to a memory \
folder at C:\\Users\\Joao\\.codex\\memories.\r\n"
                .to_string(),
        }],
        phase: None,
        internal_chat_message_metadata_passthrough: None,
    };

    let plan = plan_request(&[developer], &ClaudeSessionContinuity::default());

    assert!(plan.turn_text.contains("Instrucoes do role."));
    assert!(!plan.turn_text.contains("memory folder"));
    assert!(!plan.turn_text.contains("## Memory"));
}

#[test]
fn plugin_and_multi_agent_blocks_are_dropped() {
    let noisy = ResponseItem::Message {
        id: None,
        role: "user".to_string(),
        content: vec![ContentItem::InputText {
            text: "Contexto real da tarefa.\n<recommended_plugins>\n- Airtable\n\
</recommended_plugins>"
                .to_string(),
        }],
        phase: None,
        internal_chat_message_metadata_passthrough: None,
    };

    let plan = plan_request(&[noisy], &ClaudeSessionContinuity::default());

    assert!(plan.turn_text.contains("Contexto real da tarefa."));
    assert!(!plan.turn_text.contains("Airtable"));
}

#[test]
fn empty_tail_still_produces_a_turn() {
    let delivered = vec![user("go")];
    let continuity = established(&delivered);

    let plan = plan_request(&delivered, &continuity);

    assert!(!plan.restart_session);
    assert!(!plan.turn_text.trim().is_empty());
}
