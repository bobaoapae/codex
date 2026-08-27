use super::*;
use crate::chatgpt_web::history::ConversationContinuity;
use crate::chatgpt_web::history::plan_request;
use crate::config::ChatGptWebSettings;
use codex_protocol::protocol::SandboxPolicy;
use std::path::Path;

fn workspace(root: &Path) -> ChatGptWebWorkspace {
    ChatGptWebWorkspace {
        cwd: root.join("repo"),
        extra_roots: vec![root.join("repo"), root.join("other")],
        writable_roots: vec![root.join("repo")],
        sandbox: SandboxPolicy::new_read_only_policy(),
        developer_instructions: Some("Answer in Portuguese.".to_string()),
        settings: ChatGptWebSettings::default(),
        codex_home: root.to_path_buf(),
        sessions_state_path: None,
        connector: None,
        compact_prompt: "Summarize.".to_string(),
    }
}

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

fn user_with_image(text: &str, data_url: &str) -> ResponseItem {
    ResponseItem::Message {
        id: None,
        role: "user".to_string(),
        content: vec![
            ContentItem::InputText {
                text: text.to_string(),
            },
            ContentItem::InputImage {
                image_url: data_url.to_string(),
                detail: None,
            },
        ],
        phase: None,
        internal_chat_message_metadata_passthrough: None,
    }
}

const PNG_1X1: &str = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mNkYPhfDwAChwGA60e6kgAAAABJRU5ErkJggg==";

#[test]
fn a_replay_carries_the_contract_environment_instructions_and_transcript() {
    let temp = tempfile::tempdir().expect("tempdir");
    let workspace = workspace(temp.path());
    let input = vec![user("hello")];
    let plan = plan_request(
        &input,
        &ConversationContinuity::default(),
        "chatgpt-web/thinking",
        None,
    );

    let rendered = render(RenderRequest {
        plan: &plan,
        workspace: &workspace,
        mode: PromptMode::None,
        is_pro: false,
        resume_after_interrupt: false,
        images: None,
    });

    assert!(rendered.is_replay);
    assert!(rendered.attachments.is_empty());
    let text = &rendered.text;
    assert!(text.starts_with(HEADER));
    assert!(text.contains(NO_BRIDGE_CONTRACT));
    assert!(!text.contains(PRO_CONTRACT));
    assert!(text.contains(CLOSING_CONTRACT));
    assert!(text.contains("Environment: Working directory: "));
    assert!(text.contains("Other readable roots: "));
    assert!(text.contains("Writable roots: "));
    assert!(text.contains("network access on the Codex side is restricted"));
    assert!(
        text.contains("<developer_instructions>\nAnswer in Portuguese.\n</developer_instructions>")
    );
    assert!(text.contains("<codex_transcript>\n<user>\nhello\n</user>\n</codex_transcript>"));
    assert!(text.ends_with(RESUME_TAIL));
}

#[test]
fn the_pro_line_and_the_connector_contract_replace_the_no_bridge_text() {
    let temp = tempfile::tempdir().expect("tempdir");
    let workspace = workspace(temp.path());
    let input = vec![user("hello")];
    let plan = plan_request(
        &input,
        &ConversationContinuity::default(),
        "chatgpt-web/pro",
        None,
    );

    let rendered = render(RenderRequest {
        plan: &plan,
        workspace: &workspace,
        mode: PromptMode::Connector(vec!["Connector line one.".to_string()]),
        is_pro: true,
        resume_after_interrupt: false,
        images: None,
    });

    assert!(rendered.text.contains("Connector line one."));
    assert!(!rendered.text.contains(NO_BRIDGE_CONTRACT));
    assert!(rendered.text.contains(PRO_CONTRACT));
}

#[test]
fn a_compaction_replay_uses_the_checkpoint_contract_and_tail() {
    let temp = tempfile::tempdir().expect("tempdir");
    let workspace = workspace(temp.path());
    let input = vec![user("hello"), user("Summarize.")];
    let plan = plan_request(
        &input,
        &ConversationContinuity::default(),
        "chatgpt-web/pro",
        Some("Summarize."),
    );
    assert!(plan.is_compaction);

    let rendered = render(RenderRequest {
        plan: &plan,
        workspace: &workspace,
        mode: PromptMode::Compaction,
        is_pro: true,
        resume_after_interrupt: false,
        images: None,
    });

    for line in COMPACTION_CONTRACT {
        assert!(rendered.text.contains(line));
    }
    assert!(!rendered.text.contains(NO_BRIDGE_CONTRACT));
    assert!(
        !rendered.text.contains(PRO_CONTRACT),
        "no delegation note on a checkpoint"
    );
    assert!(rendered.text.ends_with(COMPACTION_RESUME_TAIL));
}

#[test]
fn an_extension_sends_only_the_new_items_without_a_header() {
    let temp = tempfile::tempdir().expect("tempdir");
    let workspace = workspace(temp.path());
    let first = vec![user("hello")];
    let continuity = ConversationContinuity {
        conversation_id: Some("conv".to_string()),
        model_slug: Some("chatgpt-web/thinking".to_string()),
        delivered_items: 1,
        delivered_fingerprint: crate::claude_code::history::fingerprint(&first),
        echoed: Vec::new(),
        message_landed_unanswered: false,
    };
    let input = vec![user("hello"), user("more")];
    let plan = plan_request(&input, &continuity, "chatgpt-web/thinking", None);
    assert!(!plan.restart);

    let rendered = render(RenderRequest {
        plan: &plan,
        workspace: &workspace,
        mode: PromptMode::None,
        is_pro: false,
        resume_after_interrupt: false,
        images: None,
    });
    assert!(!rendered.is_replay);
    assert_eq!(rendered.text, "<user>\nmore\n</user>");

    // Nothing new: say so. After an interrupted turn: say that too.
    let plan = plan_request(&first, &continuity, "chatgpt-web/thinking", None);
    let rendered = render(RenderRequest {
        plan: &plan,
        workspace: &workspace,
        mode: PromptMode::None,
        is_pro: false,
        resume_after_interrupt: true,
        images: None,
    });
    assert_eq!(
        rendered.text,
        format!("{INTERRUPTED_PREFIX}\n\n{NO_NEW_INPUT}")
    );
}

#[test]
fn images_are_materialized_named_in_the_transcript_and_attached() {
    let temp = tempfile::tempdir().expect("tempdir");
    let workspace = workspace(temp.path());
    let store = ImageStore::new(temp.path());
    let input = vec![user_with_image(
        "what is this?",
        &format!("data:image/png;base64,{PNG_1X1}"),
    )];
    let plan = plan_request(
        &input,
        &ConversationContinuity::default(),
        "chatgpt-web/thinking",
        None,
    );

    let rendered = render(RenderRequest {
        plan: &plan,
        workspace: &workspace,
        mode: PromptMode::None,
        is_pro: false,
        resume_after_interrupt: false,
        images: Some(&store),
    });

    assert_eq!(rendered.attachments.len(), 1);
    let name = rendered.attachments[0]
        .file_name()
        .unwrap()
        .to_string_lossy()
        .to_string();
    assert!(name.starts_with("codex-img-") && name.ends_with(".png"));
    assert!(
        rendered
            .text
            .contains(&format!("[image_attachment: {name}]"))
    );
    assert!(rendered.text.contains(IMAGES_NOTE));
    assert!(rendered.attachments[0].exists());
}

#[test]
fn only_the_newest_ten_images_are_attached() {
    let temp = tempfile::tempdir().expect("tempdir");
    let workspace = workspace(temp.path());
    let store = ImageStore::new(temp.path());
    // Twelve distinct images: vary one byte of the payload via a text suffix
    // is not possible for PNG, so use distinct GIF-ish blobs (content only
    // has to differ; nothing decodes them here).
    let input: Vec<ResponseItem> = (0..12)
        .map(|index| {
            let bytes = format!("fake-image-{index}");
            let payload = base64::Engine::encode(&base64::engine::general_purpose::STANDARD, bytes);
            user_with_image(
                &format!("image {index}"),
                &format!("data:image/gif;base64,{payload}"),
            )
        })
        .collect();
    let plan = plan_request(
        &input,
        &ConversationContinuity::default(),
        "chatgpt-web/thinking",
        None,
    );

    let rendered = render(RenderRequest {
        plan: &plan,
        workspace: &workspace,
        mode: PromptMode::None,
        is_pro: false,
        resume_after_interrupt: false,
        images: Some(&store),
    });

    assert_eq!(rendered.attachments.len(), MAX_IMAGES_PER_MESSAGE);
    assert_eq!(rendered.text.matches("(not attached;").count(), 2);
}

#[test]
fn the_warning_names_the_level() {
    assert!(warning_text("Pro").starts_with("⚠️ ChatGPT Web Pro cannot access the local computer"));
}

#[test]
fn transcript_chars_counts_the_rendered_history() {
    let input = vec![user("abc"), user("de")];
    // "<user>\nabc\n</user>" = 18 chars; "<user>\nde\n</user>" = 17 chars.
    assert_eq!(transcript_chars(&input), 35);
}
