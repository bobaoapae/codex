//! FORK: images are read once by a cheap model and reach the main model as text.

use codex_core::TurnInputRequest;
use codex_core::config::ViewImageDelegation;
use codex_protocol::config_types::CollaborationMode;
use codex_protocol::config_types::ModeKind;
use codex_protocol::config_types::Settings;
use codex_protocol::models::PermissionProfile;
use codex_protocol::openai_models::ReasoningEffort;
use codex_protocol::protocol::AskForApproval;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::Op;
use codex_protocol::protocol::ThreadSettingsOverrides;
use codex_protocol::user_input::UserInput;
use core_test_support::TempDirExt;
use core_test_support::responses;
use core_test_support::responses::ev_assistant_message;
use core_test_support::responses::ev_completed;
use core_test_support::responses::ev_function_call;
use core_test_support::responses::ev_response_created;
use core_test_support::responses::sse;
use core_test_support::responses::start_mock_server;
use core_test_support::skip_if_no_network;
use core_test_support::test_codex::TestCodex;
use core_test_support::test_codex::local_selections;
use core_test_support::test_codex::test_codex;
use core_test_support::test_codex::turn_permission_fields;
use core_test_support::wait_for_event;
use image::ImageBuffer;
use image::Rgba;
use pretty_assertions::assert_eq;
use serde_json::Value;
use std::path::Path;

const READER_DESCRIPTION: &str = "a solid magenta square, two pixels wide";

fn write_test_png(path: &Path) -> anyhow::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    ImageBuffer::from_pixel(2, 2, Rgba([255u8, 0, 255, 255])).save(path)?;
    Ok(())
}

/// Counts `input_image` content items anywhere in a captured request body.
fn image_count(body: &Value) -> usize {
    fn walk(value: &Value, found: &mut usize) {
        match value {
            Value::Object(fields) => {
                if fields.get("type").and_then(Value::as_str) == Some("input_image") {
                    *found += 1;
                }
                for nested in fields.values() {
                    walk(nested, found);
                }
            }
            Value::Array(items) => {
                for nested in items {
                    walk(nested, found);
                }
            }
            _ => {}
        }
    }
    let mut found = 0;
    walk(body, &mut found);
    found
}

fn body_contains(body: &Value, needle: &str) -> bool {
    body.to_string().contains(needle)
}

/// Points the reader at the same mock server and model as the session, which is
/// the only image-capable model the test catalog has.
fn enable_delegation(config: &mut codex_core::config::Config) {
    config.tools_view_image = Some(ViewImageDelegation {
        model: config
            .model
            .clone()
            .expect("test config always selects a model"),
        provider_id: config.model_provider_id.clone(),
        provider: config.model_provider.clone(),
        reasoning_effort: ReasoningEffort::Low,
    });
}

async fn run_text_turn(codex: &TestCodex, text: &str) -> anyhow::Result<()> {
    let (sandbox_policy, permission_profile) =
        turn_permission_fields(PermissionProfile::Disabled, codex.cwd.path());
    codex
        .codex
        .start_or_steer_turn(
            TurnInputRequest::user_input(vec![UserInput::Text {
                text: text.to_string(),
                text_elements: Vec::new(),
            }])
            .with_thread_settings(ThreadSettingsOverrides {
                environments: Some(local_selections(codex.cwd.abs())),
                approval_policy: Some(AskForApproval::Never),
                sandbox_policy: Some(sandbox_policy),
                permission_profile,
                collaboration_mode: Some(CollaborationMode {
                    mode: ModeKind::Default,
                    settings: Settings {
                        model: codex.session_configured.model.clone(),
                        reasoning_effort: None,
                        developer_instructions: None,
                    },
                }),
                ..Default::default()
            }),
        )
        .await?;
    wait_for_event(&codex.codex, |event| {
        matches!(event, EventMsg::TurnComplete(_))
    })
    .await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn pasted_image_reaches_the_model_as_the_reader_description() -> anyhow::Result<()> {
    skip_if_no_network!(Ok(()));

    let server = start_mock_server().await;
    let test = test_codex()
        .with_config(enable_delegation)
        .build(&server)
        .await?;
    let TestCodex {
        codex,
        cwd,
        session_configured,
        home: _home,
        ..
    } = &test;

    let image_path = cwd.path().join("images/pasted.png");
    write_test_png(&image_path)?;

    let mock = responses::mount_sse_sequence(
        &server,
        vec![
            // 1: the reader describing the pasted image.
            sse(vec![
                ev_response_created("resp-reader"),
                ev_assistant_message("msg-reader", READER_DESCRIPTION),
                ev_completed("resp-reader"),
            ]),
            // 2: the first main-model request.
            sse(vec![
                ev_response_created("resp-1"),
                ev_assistant_message("msg-1", "done"),
                ev_completed("resp-1"),
            ]),
            // 3: a later turn, which must still not carry the pixels.
            sse(vec![
                ev_response_created("resp-2"),
                ev_assistant_message("msg-2", "done again"),
                ev_completed("resp-2"),
            ]),
        ],
    )
    .await;

    let (sandbox_policy, permission_profile) =
        turn_permission_fields(PermissionProfile::Disabled, cwd.path());
    codex
        .start_or_steer_turn(
            TurnInputRequest::user_input(vec![
                UserInput::LocalImage {
                    path: image_path.clone(),
                    detail: None,
                },
                UserInput::Text {
                    text: "what is this".to_string(),
                    text_elements: Vec::new(),
                },
            ])
            .with_thread_settings(ThreadSettingsOverrides {
                environments: Some(local_selections(cwd.abs())),
                approval_policy: Some(AskForApproval::Never),
                sandbox_policy: Some(sandbox_policy),
                permission_profile,
                collaboration_mode: Some(CollaborationMode {
                    mode: ModeKind::Default,
                    settings: Settings {
                        model: session_configured.model.clone(),
                        reasoning_effort: None,
                        developer_instructions: None,
                    },
                }),
                ..Default::default()
            }),
        )
        .await?;
    wait_for_event(codex, |event| matches!(event, EventMsg::TurnComplete(_))).await;

    run_text_turn(&test, "and now").await?;

    codex.submit(Op::Shutdown).await?;
    wait_for_event(codex, |event| matches!(event, EventMsg::ShutdownComplete)).await;

    let requests = mock.requests();
    assert_eq!(requests.len(), 3, "reader request plus two model turns");

    // The reader is the only request that ever sees the pixels.
    let reader_body = requests[0].body_json();
    assert_eq!(image_count(&reader_body), 1);
    assert!(body_contains(&reader_body, "reading an image on behalf"));

    for (index, request) in requests.iter().enumerate().skip(1) {
        let body = request.body_json();
        assert_eq!(
            image_count(&body),
            0,
            "request {index} still carries the pixels"
        );
        assert!(
            body_contains(&body, READER_DESCRIPTION),
            "request {index} lost the reader's description"
        );
    }

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn view_image_raw_keeps_the_pixels_in_later_requests() -> anyhow::Result<()> {
    skip_if_no_network!(Ok(()));

    let server = start_mock_server().await;
    let test = test_codex()
        .with_config(enable_delegation)
        .build(&server)
        .await?;
    let TestCodex { codex, cwd, .. } = &test;

    let image_path = cwd.path().join("raw.png");
    write_test_png(&image_path)?;

    let mock = responses::mount_sse_sequence(
        &server,
        vec![
            // 1: the model asks for the pixels themselves.
            sse(vec![
                ev_response_created("resp-1"),
                ev_function_call(
                    "call-raw",
                    "view_image",
                    &serde_json::json!({ "path": "raw.png", "raw": true }).to_string(),
                ),
                ev_completed("resp-1"),
            ]),
            // 2: the follow-up request, which must still carry them.
            sse(vec![
                ev_response_created("resp-2"),
                ev_assistant_message("msg-2", "done"),
                ev_completed("resp-2"),
            ]),
        ],
    )
    .await;

    run_text_turn(&test, "look at raw.png with raw pixels").await?;

    codex.submit(Op::Shutdown).await?;
    wait_for_event(codex, |event| matches!(event, EventMsg::ShutdownComplete)).await;

    let requests = mock.requests();
    assert_eq!(requests.len(), 2, "no reader call for a raw-pinned image");
    assert_eq!(image_count(&requests[0].body_json()), 0);
    assert_eq!(
        image_count(&requests[1].body_json()),
        1,
        "raw=true must keep the pixels in the model's context"
    );

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn view_image_output_reaches_the_model_as_the_reader_description() -> anyhow::Result<()> {
    skip_if_no_network!(Ok(()));

    let server = start_mock_server().await;
    let test = test_codex()
        .with_config(enable_delegation)
        .build(&server)
        .await?;
    let TestCodex { codex, cwd, .. } = &test;

    write_test_png(&cwd.path().join("shot.png"))?;

    let mock = responses::mount_sse_sequence(
        &server,
        vec![
            // 1: the model calls view_image without asking for the pixels.
            sse(vec![
                ev_response_created("resp-1"),
                ev_function_call(
                    "call-view",
                    "view_image",
                    &serde_json::json!({ "path": "shot.png" }).to_string(),
                ),
                ev_completed("resp-1"),
            ]),
            // 2: the reader describing the tool output's image.
            sse(vec![
                ev_response_created("resp-reader"),
                ev_assistant_message("msg-reader", READER_DESCRIPTION),
                ev_completed("resp-reader"),
            ]),
            // 3: the follow-up request, which gets the text and not the pixels.
            sse(vec![
                ev_response_created("resp-2"),
                ev_assistant_message("msg-2", "done"),
                ev_completed("resp-2"),
            ]),
        ],
    )
    .await;

    run_text_turn(&test, "look at shot.png").await?;

    codex.submit(Op::Shutdown).await?;
    wait_for_event(codex, |event| matches!(event, EventMsg::ShutdownComplete)).await;

    let requests = mock.requests();
    assert_eq!(requests.len(), 3);
    assert_eq!(image_count(&requests[0].body_json()), 0);
    // Only the reader sees the pixels view_image loaded.
    assert_eq!(image_count(&requests[1].body_json()), 1);

    let follow_up = requests[2].body_json();
    assert_eq!(
        image_count(&follow_up),
        0,
        "the view_image output still carries its pixels"
    );
    assert!(body_contains(&follow_up, READER_DESCRIPTION));
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn view_image_question_returns_the_reader_answer_as_text() -> anyhow::Result<()> {
    skip_if_no_network!(Ok(()));

    let server = start_mock_server().await;
    let test = test_codex()
        .with_config(enable_delegation)
        .build(&server)
        .await?;
    let TestCodex { codex, cwd, .. } = &test;

    write_test_png(&cwd.path().join("shot.png"))?;

    let mock = responses::mount_sse_sequence(
        &server,
        vec![
            // 1: the model asks the reader a question about the image.
            sse(vec![
                ev_response_created("resp-1"),
                ev_function_call(
                    "call-ask",
                    "view_image",
                    &serde_json::json!({
                        "path": "shot.png",
                        "question": "what color is it",
                    })
                    .to_string(),
                ),
                ev_completed("resp-1"),
            ]),
            // 2: the reader answering.
            sse(vec![
                ev_response_created("resp-reader"),
                ev_assistant_message("msg-reader", "magenta"),
                ev_completed("resp-reader"),
            ]),
            // 3: the follow-up request, carrying the answer as tool output.
            sse(vec![
                ev_response_created("resp-2"),
                ev_assistant_message("msg-2", "done"),
                ev_completed("resp-2"),
            ]),
        ],
    )
    .await;

    run_text_turn(&test, "what color is shot.png").await?;

    codex.submit(Op::Shutdown).await?;
    wait_for_event(codex, |event| matches!(event, EventMsg::ShutdownComplete)).await;

    let requests = mock.requests();
    assert_eq!(requests.len(), 3);
    let reader_body = requests[1].body_json();
    assert_eq!(image_count(&reader_body), 1);
    assert!(body_contains(&reader_body, "what color is it"));

    let follow_up = requests[2].body_json();
    assert_eq!(image_count(&follow_up), 0);
    assert_eq!(
        requests[2].function_call_output_text("call-ask").as_deref(),
        Some("magenta")
    );
    Ok(())
}

/// FORK: a resumed thread replays the persisted description; re-reading every
/// image on resume would be the expensive regression this whole feature avoids.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn resume_replays_the_description_without_calling_the_reader_again() -> anyhow::Result<()> {
    skip_if_no_network!(Ok(()));

    let server = start_mock_server().await;
    let mut builder = test_codex().with_config(enable_delegation);
    let test = builder.build(&server).await?;

    let image_path = test.cwd.path().join("resumed.png");
    write_test_png(&image_path)?;

    let first = responses::mount_sse_sequence(
        &server,
        vec![
            sse(vec![
                ev_response_created("resp-reader"),
                ev_assistant_message("msg-reader", READER_DESCRIPTION),
                ev_completed("resp-reader"),
            ]),
            sse(vec![
                ev_response_created("resp-1"),
                ev_assistant_message("msg-1", "done"),
                ev_completed("resp-1"),
            ]),
        ],
    )
    .await;

    let (sandbox_policy, permission_profile) =
        turn_permission_fields(PermissionProfile::Disabled, test.cwd.path());
    test.codex
        .start_or_steer_turn(
            TurnInputRequest::user_input(vec![UserInput::LocalImage {
                path: image_path.clone(),
                detail: None,
            }])
            .with_thread_settings(ThreadSettingsOverrides {
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
            }),
        )
        .await?;
    wait_for_event(&test.codex, |event| {
        matches!(event, EventMsg::TurnComplete(_))
    })
    .await;
    assert_eq!(first.requests().len(), 2);

    server.reset().await;
    let resumed = builder.restart(&server, &test).await?;
    let after_resume = responses::mount_sse_sequence(
        &server,
        vec![sse(vec![
            ev_response_created("resp-2"),
            ev_assistant_message("msg-2", "done again"),
            ev_completed("resp-2"),
        ])],
    )
    .await;

    run_text_turn(&resumed, "still there?").await?;
    resumed.codex.submit(Op::Shutdown).await?;
    wait_for_event(&resumed.codex, |event| {
        matches!(event, EventMsg::ShutdownComplete)
    })
    .await;

    let requests = after_resume.requests();
    assert_eq!(requests.len(), 1, "resume must not call the reader again");
    let body = requests[0].body_json();
    assert_eq!(image_count(&body), 0);
    assert!(
        body_contains(&body, READER_DESCRIPTION),
        "the persisted description did not survive resume"
    );
    Ok(())
}
