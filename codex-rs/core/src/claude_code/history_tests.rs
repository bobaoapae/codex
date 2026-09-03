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

/// FORK: a message carrying the harness annotations the real pipeline attaches,
/// one kind per content entry.
fn annotated(role: &str, entries: &[(&str, &str)]) -> ResponseItem {
    ResponseItem::Message {
        id: None,
        role: role.to_string(),
        content: entries
            .iter()
            .map(|(_, text)| ContentItem::InputText {
                text: (*text).to_string(),
            })
            .collect(),
        phase: None,
        internal_chat_message_metadata_passthrough: Some(
            codex_protocol::models::InternalChatMessageMetadataPassthrough {
                content_item_kinds: Some(
                    entries
                        .iter()
                        .map(|(kind, _)| ContentItemKind((*kind).to_string()))
                        .collect(),
                ),
                ..Default::default()
            },
        ),
    }
}

fn established(input: &[ResponseItem]) -> ClaudeSessionContinuity {
    ClaudeSessionContinuity {
        session_id: Some("session-1".to_string()),
        delivered_items: input.len(),
        delivered_fingerprint: fingerprint(input),
        account_dir: None,
        echoed: Vec::new(),
    }
}

/// Continuity as it stands right after Claude answered: the items it authored
/// are recorded, but Codex has not appended them to `input` yet.
fn established_with_echo(
    input: &[ResponseItem],
    authored: &[ResponseItem],
) -> ClaudeSessionContinuity {
    ClaudeSessionContinuity {
        echoed: authored.iter().map(item_fingerprint).collect(),
        ..established(input)
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
fn fork_invariant_local_claude_drops_encrypted_agent_messages() {
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
            call_id: Some("call-1".to_string()),
            name: None,
            namespace: None,
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
        call_id: Some("call-1".to_string()),
        name: None,
        namespace: None,
        output: FunctionCallOutputPayload::from_text(output),
        internal_chat_message_metadata_passthrough: None,
    }];

    let plan = plan_request(&input, &ClaudeSessionContinuity::default());

    assert!(plan.turn_text.contains("HEAD"));
    assert!(plan.turn_text.contains("TAIL"));
    assert!(plan.turn_text.contains("characters elided"));
}

/// FORK: harness fragments are recognized by their annotated kind, not by a
/// text marker. Everything the child cannot act on is dropped; the role's own
/// instructions and the actual task survive.
#[test]
fn harness_kinds_are_dropped_but_role_instructions_survive() {
    let bundle = annotated(
        "developer",
        &[
            (
                "agents_md.instructions",
                "## Memory\n\nYou have access to a memory folder at C:\\Users\\Joao\\.codex\\memories.",
            ),
            (
                "multi_agent.mode_instructions",
                "You are an agent in a team of agents. Use `spawn_agent` and `functions.exec`.",
            ),
            (
                "plugins.recommendations",
                "<recommended_plugins>\n- Airtable\n</recommended_plugins>",
            ),
            (
                "generic.turn_aborted",
                "The previous turn was aborted by the user.",
            ),
        ],
    );
    let role = annotated(
        "developer",
        &[(
            "unknown",
            "Voce e um agente Claude sob a direcao do agente principal.",
        )],
    );

    let plan = plan_request(
        &[bundle, role, user("faca a auditoria")],
        &ClaudeSessionContinuity::default(),
    );

    assert!(plan.turn_text.contains("sob a direcao do agente principal"));
    assert!(plan.turn_text.contains("faca a auditoria"));
    assert!(plan.turn_text.contains("aborted by the user"));
    assert!(!plan.turn_text.contains("memory folder"));
    assert!(!plan.turn_text.contains("spawn_agent"));
    assert!(!plan.turn_text.contains("Airtable"));
}

/// FORK: the regression this replaced. The old stripper cut from `## Memory` to
/// the end of the message, so a role instruction appended after the harness
/// bundle disappeared with it.
#[test]
fn a_role_instruction_after_the_memory_section_survives() {
    let bundle = annotated(
        "developer",
        &[
            ("agents_md.instructions", "## Memory\n\nmemory folder here."),
            ("unknown", "Reporte so na resposta final."),
        ],
    );

    let plan = plan_request(&[bundle], &ClaudeSessionContinuity::default());

    assert!(plan.turn_text.contains("Reporte so na resposta final."));
    assert!(!plan.turn_text.contains("memory folder"));
}

/// A message the harness never annotated (an old rollout) still gets the
/// tag-shaped blocks removed — but only up to the closing tag.
#[test]
fn unannotated_messages_fall_back_to_tag_stripping() {
    let noisy = ResponseItem::Message {
        id: None,
        role: "user".to_string(),
        content: vec![ContentItem::InputText {
            text: "Contexto real da tarefa.\n<recommended_plugins>\n- Airtable\n\
</recommended_plugins>\nInstrucao que vem depois."
                .to_string(),
        }],
        phase: None,
        internal_chat_message_metadata_passthrough: None,
    };

    let plan = plan_request(&[noisy], &ClaudeSessionContinuity::default());

    assert!(plan.turn_text.contains("Contexto real da tarefa."));
    assert!(!plan.turn_text.contains("Airtable"));
    // The old stripper cut to the end of the message and lost this line.
    assert!(plan.turn_text.contains("Instrucao que vem depois."));
}

/// FORK: the whole point of the filter is size. A realistic injected bundle
/// used to reach a 37k-character median; what survives must be a task brief.
#[test]
fn a_realistic_bundle_leaves_a_small_turn() {
    let filler = "x".repeat(20_000);
    let bundle = annotated(
        "developer",
        &[
            ("agents_md.instructions", filler.as_str()),
            ("plugins.usage_instructions", filler.as_str()),
            ("multi_agent.mode_instructions", filler.as_str()),
            ("model.base_instructions", filler.as_str()),
            ("token_budget.context_window", filler.as_str()),
            ("personality.spec_instructions", filler.as_str()),
        ],
    );

    let plan = plan_request(
        &[bundle, user("audite o modulo de rede")],
        &ClaudeSessionContinuity::default(),
    );

    assert!(plan.turn_text.contains("audite o modulo de rede"));
    assert!(
        plan.turn_text.len() < 4_000,
        "turn_text was {} chars",
        plan.turn_text.len()
    );
}

/// FORK: the adapter now records Claude's own tool calls in Codex history. The
/// live Claude session already contains them, so a replay must not feed the
/// agent its own trace back as if it were new transcript.
#[test]
fn claude_own_tool_trace_is_not_replayed_to_claude() {
    let call = ResponseItem::FunctionCall {
        id: None,
        name: "Bash".to_string(),
        namespace: Some(super::super::tools::CLAUDE_TOOL_NAMESPACE.to_string()),
        arguments: r#"{"command":"cargo test"}"#.to_string(),
        encrypted_function_args: None,
        call_id: "toolu_1".to_string(),
        internal_chat_message_metadata_passthrough: None,
    };
    let output = ResponseItem::FunctionCallOutput {
        id: None,
        call_id: Some("toolu_1".to_string()),
        name: Some("Bash".to_string()),
        namespace: Some(super::super::tools::CLAUDE_TOOL_NAMESPACE.to_string()),
        output: FunctionCallOutputPayload {
            body: codex_protocol::models::FunctionCallOutputBody::Text(
                "test result: ok".to_string(),
            ),
            success: Some(true),
        },
        internal_chat_message_metadata_passthrough: None,
    };

    let plan = plan_request(
        &[call, output, user("agora documente")],
        &ClaudeSessionContinuity::default(),
    );

    assert!(plan.turn_text.contains("agora documente"));
    assert!(!plan.turn_text.contains("cargo test"));
    assert!(!plan.turn_text.contains("test result: ok"));
}

#[test]
fn empty_tail_still_produces_a_turn() {
    let delivered = vec![user("go")];
    let continuity = established(&delivered);

    let plan = plan_request(&delivered, &continuity);

    assert!(!plan.restart_session);
    assert!(!plan.turn_text.trim().is_empty());
}

/// FORK: Claude's own answer comes back in the next request's tail, because
/// Codex appends it after the request that produced it. The live session already
/// has it — resending it makes Claude read its own reply as new input.
#[test]
fn follow_up_does_not_echo_claude_own_answer() {
    let delivered = vec![user("build the thing")];
    let answer = assistant("built it");
    let continuity = established_with_echo(&delivered, std::slice::from_ref(&answer));
    let mut input = delivered;
    input.push(answer);
    input.push(user("now add tests"));

    let plan = plan_request(&input, &continuity);

    assert!(!plan.restart_session);
    assert!(!plan.turn_text.contains("built it"), "{}", plan.turn_text);
    assert!(plan.turn_text.contains("now add tests"));
    // The session has still seen everything up to here.
    assert_eq!(plan.delivered_items, 3);
}

/// FORK: an in-place retry after an Anthropic failure records both attempts'
/// items. The partial answer the failed attempt delivered is in the live Claude
/// session too, so the next request must drop it along with the retry's answer
/// — otherwise the abandoned half comes back as fresh input.
#[test]
fn follow_up_drops_the_items_a_retried_attempt_authored() {
    let delivered = vec![user("build the thing")];
    let partial = assistant("I started by");
    let answer = assistant("built it");
    // What `state.record` stores after the retry: the failed attempt's
    // fingerprints seeded the assembler, so both are echoed.
    let continuity = established_with_echo(&delivered, &[partial.clone(), answer.clone()]);
    let mut input = delivered;
    input.push(partial);
    input.push(answer);
    input.push(user("now add tests"));

    let plan = plan_request(&input, &continuity);

    assert!(!plan.restart_session);
    assert!(
        !plan.turn_text.contains("I started by"),
        "{}",
        plan.turn_text
    );
    assert!(!plan.turn_text.contains("built it"), "{}", plan.turn_text);
    assert!(plan.turn_text.contains("now add tests"));
    assert_eq!(plan.delivered_items, 4);
}

/// A replay rebuilds the conversation from scratch, so Claude's turns belong in
/// it — dropping them would hand it a transcript of only one side.
#[test]
fn replay_keeps_claude_own_answer() {
    let answer = assistant("built it");
    let input = vec![
        user("build the thing"),
        answer.clone(),
        user("now add tests"),
    ];
    let continuity = ClaudeSessionContinuity {
        echoed: vec![item_fingerprint(&answer)],
        ..ClaudeSessionContinuity::default()
    };

    let plan = plan_request(&input, &continuity);

    assert!(plan.restart_session);
    assert!(plan.turn_text.contains("built it"));
}
