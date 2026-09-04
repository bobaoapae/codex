use super::*;
use codex_protocol::AgentPath;

#[test]
fn generated_message_id_is_uuid_v7_and_payload_round_trips() {
    let mut communication = InterAgentCommunication::new(
        AgentPath::root(),
        AgentPath::morpheus(),
        Vec::new(),
        "hello durable mailbox".to_string(),
        true,
    );
    let message_id = ensure_message_id(&mut communication);
    let uuid = Uuid::parse_str(&message_id).expect("message IDs are UUIDs");
    assert_eq!(uuid.get_version_num(), 7);

    let start_options = TurnStartOptions {
        guardian_ticket: None,
        turn_trigger: Some("mailbox".to_string()),
        final_output_json_schema: Some(serde_json::json!({"type": "object"})),
        service_tier: Some("fast".to_string()),
        parent_turn_id: Some("parent".to_string()),
        root_turn_id: Some("root".to_string()),
        cyber_access_program: Some(CyberAccessProgram::Standard),
    };
    let encoded = payload(&communication, &start_options).expect("payload serializes");
    let (decoded_communication, decoded_options) =
        decode_payload(encoded.clone(), &message_id).expect("payload decodes");

    assert_eq!(decoded_communication, communication);
    assert_eq!(
        payload(&decoded_communication, &decoded_options).unwrap(),
        encoded
    );
}

#[test]
fn payload_message_id_must_match_mailbox_row() {
    let mut communication = InterAgentCommunication::new(
        AgentPath::root(),
        AgentPath::morpheus(),
        Vec::new(),
        "hello".to_string(),
        false,
    );
    let message_id = ensure_message_id(&mut communication);
    let payload = payload(&communication, &TurnStartOptions::default()).unwrap();

    let error = decode_payload(payload, "different-message-id").unwrap_err();
    assert!(error.contains("does not match"));
    assert_eq!(message_id, communication.id.unwrap().to_string());
}

#[test]
fn canonical_history_uuid_is_the_dedupe_authority() {
    let mut communication = InterAgentCommunication::new(
        AgentPath::root(),
        AgentPath::morpheus(),
        Vec::new(),
        "already canonical".to_string(),
        false,
    );
    let message_id = "01984de2-8f74-7c91-a3b2-5c5e937cf318";
    communication.id = Some(ResponseItemId::from_server(message_id.to_string()));

    assert!(history_contains_mailbox_id(
        std::slice::from_ref(&RolloutItem::InterAgentCommunication(communication.clone())),
        message_id,
    ));
    assert!(history_contains_mailbox_id(
        std::slice::from_ref(&RolloutItem::ResponseItem(
            communication.to_model_input_item().into()
        )),
        message_id,
    ));
    let legacy_response = ResponseItem::Message {
        id: None,
        role: "assistant".to_string(),
        content: vec![codex_protocol::models::ContentItem::OutputText {
            text: serde_json::to_string(&communication).expect("serialize communication"),
        }],
        phase: None,
        internal_chat_message_metadata_passthrough: None,
    };
    assert!(history_contains_mailbox_id(
        std::slice::from_ref(&RolloutItem::ResponseItem(legacy_response.into())),
        message_id,
    ));

    communication.id = None;
    assert!(!history_contains_mailbox_id(
        std::slice::from_ref(&RolloutItem::InterAgentCommunication(communication)),
        message_id,
    ));
}

#[test]
fn model_context_scan_matches_consumed_communications() {
    let mut communication = InterAgentCommunication::new(
        AgentPath::root(),
        AgentPath::morpheus(),
        Vec::new(),
        "applied earlier".to_string(),
        false,
    );
    let message_id = "01984de2-8f74-7c91-a3b2-5c5e937cf319";
    communication.id = Some(ResponseItemId::from_server(message_id.to_string()));

    // The consumption path records the communication as an AgentMessage
    // carrying the durable UUID.
    let consumed = communication.to_model_input_item();
    assert!(model_context_contains_mailbox_id(
        std::slice::from_ref(&consumed).iter(),
        message_id,
    ));

    // Legacy assistant-message encoding is recognized too.
    let legacy = ResponseItem::Message {
        id: None,
        role: "assistant".to_string(),
        content: vec![codex_protocol::models::ContentItem::OutputText {
            text: serde_json::to_string(&communication).expect("serialize communication"),
        }],
        phase: None,
        internal_chat_message_metadata_passthrough: None,
    };
    assert!(model_context_contains_mailbox_id(
        std::slice::from_ref(&legacy).iter(),
        message_id,
    ));

    // Unrelated items and other UUIDs do not match.
    assert!(!model_context_contains_mailbox_id(
        std::slice::from_ref(&consumed).iter(),
        "01984de2-8f74-7c91-a3b2-000000000000",
    ));
    let user_message = ResponseItem::Message {
        id: None,
        role: "user".to_string(),
        content: vec![codex_protocol::models::ContentItem::InputText {
            text: "hello".to_string(),
        }],
        phase: None,
        internal_chat_message_metadata_passthrough: None,
    };
    assert!(!model_context_contains_mailbox_id(
        std::slice::from_ref(&user_message).iter(),
        message_id,
    ));
}
