//! FORK: reader requests are deduplicated and accounted outside the main model.

use anyhow::Result;
use codex_core::TurnInputRequest;
use codex_core::config::ViewImageDelegation;
use codex_history::ImageReaderUsage;
use codex_history::RolloutItem;
use codex_protocol::config_types::CollaborationMode;
use codex_protocol::config_types::ModeKind;
use codex_protocol::config_types::Settings;
use codex_protocol::models::PermissionProfile;
use codex_protocol::openai_models::ReasoningEffort;
use codex_protocol::protocol::AskForApproval;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::ThreadSettingsOverrides;
use codex_protocol::protocol::TokenUsage;
use codex_protocol::protocol::TokenUsageRecord;
use codex_protocol::user_input::UserInput;
use core_test_support::TempDirExt;
use core_test_support::responses::ev_assistant_message;
use core_test_support::responses::ev_completed_with_tokens;
use core_test_support::responses::ev_response_created;
use core_test_support::responses::mount_sse_sequence;
use core_test_support::responses::sse;
use core_test_support::responses::start_mock_server;
use core_test_support::skip_if_no_network;
use core_test_support::test_codex::TestCodex;
use core_test_support::test_codex::local_selections;
use core_test_support::test_codex::test_codex;
use core_test_support::test_codex::turn_permission_fields;
use core_test_support::wait_for_event;
use pretty_assertions::assert_eq;
use std::path::Path;

const IMAGE_URL: &str = "data:image/png;base64,iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR4nGP4z8DwHwAFAAH/iZk9HQAAAABJRU5ErkJggg==";
const READER_DESCRIPTION: &str = "one repeated image description";

fn enable_delegation(config: &mut codex_core::config::Config) {
    config.tools_view_image = Some(ViewImageDelegation {
        model: "gpt-5.4".to_string(),
        provider_id: config.model_provider_id.clone(),
        provider: config.model_provider.clone(),
        reasoning_effort: ReasoningEffort::Low,
    });
}

async fn submit_image_turn(test: &TestCodex, input: Vec<UserInput>) -> Result<()> {
    let (sandbox_policy, permission_profile) =
        turn_permission_fields(PermissionProfile::Disabled, test.cwd.path());
    test.codex
        .start_or_steer_turn(TurnInputRequest::user_input(input).with_thread_settings(
            ThreadSettingsOverrides {
                environments: Some(local_selections(test.cwd.abs())),
                approval_policy: Some(AskForApproval::Never),
                sandbox_policy: Some(sandbox_policy),
                permission_profile,
                collaboration_mode: Some(CollaborationMode {
                    mode: ModeKind::Default,
                    settings: Settings {
                        model: test.session_configured.model.clone(),
                        reasoning_effort: None,
                        developer_instructions: None,
                    },
                }),
                ..Default::default()
            },
        ))
        .await?;
    wait_for_event(&test.codex, |event| {
        matches!(event, EventMsg::TurnComplete(_))
    })
    .await;
    Ok(())
}

fn rollout_items(path: &Path) -> Result<Vec<codex_rollout::RolloutLine>> {
    let text = std::fs::read_to_string(path)?;
    Ok(text
        .lines()
        .map(codex_rollout::parse_rollout_line)
        .collect::<serde_json::Result<Vec<_>>>()?)
}

fn reader_usages(path: &Path) -> Result<Vec<ImageReaderUsage>> {
    Ok(rollout_items(path)?
        .into_iter()
        .filter_map(|line| match line.item {
            RolloutItem::Extension(usage) => Some(usage),
            _ => None,
        })
        .collect())
}

fn token_usage_records(path: &Path) -> Result<Vec<TokenUsageRecord>> {
    Ok(rollout_items(path)?
        .into_iter()
        .filter_map(|line| match line.item {
            RolloutItem::TokenUsageRecord(record) => Some(record),
            _ => None,
        })
        .collect())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn equivalent_images_share_reader_usage_without_main_model_accounting() -> Result<()> {
    skip_if_no_network!(Ok(()));

    let server = start_mock_server().await;
    let mut builder = test_codex().with_config(enable_delegation);
    let test = builder.build_with_auto_env(&server).await?;
    let reader = test
        .config
        .tools_view_image
        .as_ref()
        .expect("reader delegation is enabled");
    let expected_reader_model = reader.model.clone();
    let expected_reader_provider = reader.provider_id.clone();

    let mock = mount_sse_sequence(
        &server,
        vec![
            // The two equivalent images in the first turn share this one call.
            sse(vec![
                ev_response_created("reader-response"),
                ev_assistant_message("reader-message", READER_DESCRIPTION),
                ev_completed_with_tokens("reader-response", /*total_tokens*/ 17),
            ]),
            sse(vec![
                ev_response_created("main-response-1"),
                ev_assistant_message("main-message-1", "first turn complete"),
                ev_completed_with_tokens("main-response-1", /*total_tokens*/ 23),
            ]),
            // The same image on the next turn is a completed-cache hit.
            sse(vec![
                ev_response_created("main-response-2"),
                ev_assistant_message("main-message-2", "second turn complete"),
                ev_completed_with_tokens("main-response-2", /*total_tokens*/ 29),
            ]),
        ],
    )
    .await;

    submit_image_turn(
        &test,
        vec![
            UserInput::Image {
                image_url: IMAGE_URL.to_string(),
                detail: None,
            },
            UserInput::Image {
                image_url: IMAGE_URL.to_string(),
                detail: None,
            },
            UserInput::Text {
                text: "describe the repeated images".to_string(),
                text_elements: Vec::new(),
            },
        ],
    )
    .await?;
    submit_image_turn(
        &test,
        vec![
            UserInput::Image {
                image_url: IMAGE_URL.to_string(),
                detail: None,
            },
            UserInput::Text {
                text: "describe it again".to_string(),
                text_elements: Vec::new(),
            },
        ],
    )
    .await?;

    test.codex.shutdown_and_wait().await?;

    let requests = mock.requests();
    assert_eq!(requests.len(), 3);
    let reader_requests = requests
        .iter()
        .filter(|request| !request.message_input_image_urls("user").is_empty())
        .collect::<Vec<_>>();
    assert_eq!(
        reader_requests.len(),
        1,
        "equivalent reads must share one request"
    );
    let reader_request = reader_requests[0];
    assert_eq!(reader_request.message_input_image_urls("user").len(), 1);
    assert_eq!(
        reader_request.body_json()["model"].as_str(),
        Some(expected_reader_model.as_str())
    );
    assert!(reader_request.body_contains_text("reading an image on behalf"));

    for request in requests
        .iter()
        .filter(|request| request.message_input_image_urls("user").is_empty())
    {
        assert!(request.body_contains_text(READER_DESCRIPTION));
    }

    let rollout_path = test.codex.rollout_path().expect("rollout path");
    assert_eq!(
        reader_usages(&rollout_path)?,
        vec![ImageReaderUsage::new(
            expected_reader_model,
            expected_reader_provider,
            "reader-response",
            Some(TokenUsage {
                input_tokens: 17,
                cached_input_tokens: 0,
                cache_write_input_tokens: 0,
                output_tokens: 0,
                reasoning_output_tokens: 0,
                total_tokens: 17,
                codex_rollout_budget_units: None,
            }),
        )]
    );

    let records = token_usage_records(&rollout_path)?;
    assert_eq!(
        records
            .iter()
            .map(|record| {
                (
                    record.response_id.as_str(),
                    record.usage.total_tokens,
                    record.turn_token_usage.total_tokens,
                    record.thread_token_usage.total_tokens,
                )
            })
            .collect::<Vec<_>>(),
        vec![
            ("main-response-1", 23, 23, 23),
            ("main-response-2", 29, 29, 52),
        ]
    );

    Ok(())
}
