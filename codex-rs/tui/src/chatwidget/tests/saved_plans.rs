//! FORK: `/plans` — browsing and loading plans persisted by Plan mode.

use super::*;
use crate::app_event::SavedPlanAction;
use crate::chatwidget::saved_plans;
use codex_app_server_protocol::PlanReadResponse;
use codex_app_server_protocol::PlanSummary;
use pretty_assertions::assert_eq;

fn plan_summary(id: &str, title: &str, cwd: Option<&str>, updated_at: i64) -> PlanSummary {
    PlanSummary {
        id: id.to_string(),
        title: title.to_string(),
        path: format!("/home/user/.codex/plans/{id}"),
        thread_id: Some("thread-1".to_string()),
        turn_id: Some("turn-1".to_string()),
        cwd: cwd.map(str::to_string),
        model: Some("gpt-5.2".to_string()),
        created_at: 1_756_000_000,
        updated_at,
        revision: 2,
    }
}

fn plan_read(id: &str, title: &str, markdown: &str) -> PlanReadResponse {
    PlanReadResponse {
        plan: plan_summary(id, title, Some("/home/user/project"), 1_756_300_000),
        markdown: markdown.to_string(),
    }
}

#[tokio::test]
async fn plans_slash_command_opens_the_picker() {
    let (mut chat, mut rx, _op_rx) = make_chatwidget_manual(/*model_override*/ None).await;

    chat.dispatch_command(SlashCommand::Plans);

    let events = std::iter::from_fn(|| rx.try_recv().ok()).collect::<Vec<_>>();
    assert!(
        events
            .iter()
            .any(|event| matches!(event, AppEvent::OpenPlansPicker)),
        "expected OpenPlansPicker; events: {events:?}"
    );
}

#[tokio::test]
async fn plans_slash_command_is_blocked_during_a_task_and_for_owned_input() {
    let (mut chat, mut rx, _op_rx) = make_chatwidget_manual(/*model_override*/ None).await;
    chat.on_task_started();
    chat.dispatch_command(SlashCommand::Plans);
    let events = std::iter::from_fn(|| rx.try_recv().ok()).collect::<Vec<_>>();
    assert!(
        !events
            .iter()
            .any(|event| matches!(event, AppEvent::OpenPlansPicker)),
        "the picker must not open while a turn is running; events: {events:?}"
    );

    let (mut chat, mut rx, _op_rx) = make_chatwidget_manual(/*model_override*/ None).await;
    chat.blocks_direct_input = true;
    chat.dispatch_command(SlashCommand::Plans);
    let events = std::iter::from_fn(|| rx.try_recv().ok()).collect::<Vec<_>>();
    assert!(
        !events
            .iter()
            .any(|event| matches!(event, AppEvent::OpenPlansPicker)),
        "the picker must not open for parent-owned input; events: {events:?}"
    );
}

#[test]
fn slash_command_names_stay_distinct() {
    use std::str::FromStr;

    assert_eq!(
        SlashCommand::from_str("plan").expect("plan command"),
        SlashCommand::Plan
    );
    assert_eq!(
        SlashCommand::from_str("plans").expect("plans command"),
        SlashCommand::Plans
    );
    assert!(!SlashCommand::Plans.available_during_task());
}

#[test]
fn picker_rows_are_searchable_and_describe_the_project() {
    let plans = vec![
        plan_summary(
            "a.md",
            "Newer plan",
            Some("/home/user/project"),
            1_756_300_000,
        ),
        plan_summary(
            "b.md",
            "Older plan",
            Some("/home/user/other"),
            1_756_200_000,
        ),
    ];

    let params = saved_plans::picker_params(&plans, "/home/user/project");

    assert!(params.is_searchable);
    assert_eq!(params.items.len(), 2);
    assert!(
        params.items.iter().all(|item| item.search_value.is_some()),
        "every row must be reachable by search"
    );
    assert!(
        params.items[0]
            .description
            .as_deref()
            .expect("description")
            .contains("this project"),
    );
    assert!(
        params.items[1]
            .description
            .as_deref()
            .expect("description")
            .contains("other"),
    );
    assert!(
        params.items[0]
            .description
            .as_deref()
            .expect("description")
            .ends_with("rev 2")
    );
}

#[tokio::test]
async fn picker_selection_opens_the_actions_popup() {
    let (mut chat, mut rx, _op_rx) = make_chatwidget_manual(/*model_override*/ None).await;
    chat.show_plans_picker(vec![plan_summary(
        "a.md",
        "Newer plan",
        Some("/home/user/project"),
        1_756_300_000,
    )]);

    chat.handle_key_event(KeyEvent::from(KeyCode::Enter));

    let events = std::iter::from_fn(|| rx.try_recv().ok()).collect::<Vec<_>>();
    let opened = events
        .iter()
        .find_map(|event| match event {
            AppEvent::OpenSavedPlanActions { id, title } => Some((id.clone(), title.clone())),
            _ => None,
        })
        .unwrap_or_else(|| panic!("expected OpenSavedPlanActions; events: {events:?}"));
    assert_eq!(opened, ("a.md".to_string(), "Newer plan".to_string()));
}

#[tokio::test]
async fn actions_popup_rows_map_to_load_actions() {
    let (mut chat, mut rx, _op_rx) = make_chatwidget_manual(/*model_override*/ None).await;
    chat.set_feature_enabled(Feature::CollaborationModes, /*enabled*/ true);

    for (downs, expected) in [
        (0, SavedPlanAction::Implement),
        (1, SavedPlanAction::AttachToNextMessage),
        (2, SavedPlanAction::Revise),
    ] {
        chat.show_saved_plan_actions("a.md".to_string(), "Newer plan".to_string());
        for _ in 0..downs {
            chat.handle_key_event(KeyEvent::from(KeyCode::Down));
        }
        chat.handle_key_event(KeyEvent::from(KeyCode::Enter));

        let events = std::iter::from_fn(|| rx.try_recv().ok()).collect::<Vec<_>>();
        let action = events
            .iter()
            .find_map(|event| match event {
                AppEvent::LoadSavedPlan { id, action } if id == "a.md" => Some(*action),
                _ => None,
            })
            .unwrap_or_else(|| panic!("expected LoadSavedPlan; events: {events:?}"));
        assert_eq!(action, expected);
    }
}

#[tokio::test]
async fn revise_is_disabled_without_plan_mode() {
    let params =
        saved_plans::load_plan_params("a.md", "Newer plan", /*plan_mode_available*/ false);
    assert_eq!(
        params.items[2].disabled_reason.as_deref(),
        Some(saved_plans::SAVED_PLAN_PLAN_MODE_UNAVAILABLE)
    );
    assert!(params.items[2].actions.is_empty());

    let params =
        saved_plans::load_plan_params("a.md", "Newer plan", /*plan_mode_available*/ true);
    assert_eq!(params.items[2].disabled_reason, None);
    assert!(!params.items[2].actions.is_empty());
}

#[tokio::test]
async fn implementing_a_saved_plan_submits_in_default_mode_with_the_plan_as_context() {
    let (mut chat, mut rx, mut op_rx) = make_chatwidget_manual(Some("gpt-5.2")).await;
    chat.thread_id = Some(ThreadId::new());
    chat.set_feature_enabled(Feature::CollaborationModes, /*enabled*/ true);
    let _ = drain_insert_history(&mut rx);

    chat.apply_loaded_plan(
        plan_read("a.md", "Newer plan", "# Newer plan\n- step\n"),
        SavedPlanAction::Implement,
    );

    let op = next_submit_op(&mut op_rx);
    let Op::UserTurn {
        items,
        collaboration_mode,
        ..
    } = op
    else {
        panic!("expected Op::UserTurn, got {op:?}");
    };
    assert_eq!(
        collaboration_mode.as_ref().map(|mode| mode.mode),
        Some(ModeKind::Default)
    );
    let text = user_turn_text(&items);
    assert!(text.starts_with("# Saved plan: Newer plan (/home/user/.codex/plans/a.md)"));
    assert!(text.contains("# Newer plan\n- step"));
    assert!(text.ends_with(&format!(
        "## My request for Codex:\n{}",
        plan_implementation::PLAN_IMPLEMENTATION_CODING_MESSAGE
    )));
    assert_eq!(text.matches("## My request for Codex:").count(), 1);
    // The context is consumed by the submission.
    assert!(chat.pending_saved_plan_title().is_none());
}

#[tokio::test]
async fn revising_a_saved_plan_submits_in_plan_mode() {
    let (mut chat, mut rx, mut op_rx) = make_chatwidget_manual(Some("gpt-5.2")).await;
    chat.thread_id = Some(ThreadId::new());
    chat.set_feature_enabled(Feature::CollaborationModes, /*enabled*/ true);
    let _ = drain_insert_history(&mut rx);

    chat.apply_loaded_plan(
        plan_read("a.md", "Newer plan", "# Newer plan\n- step\n"),
        SavedPlanAction::Revise,
    );

    let op = next_submit_op(&mut op_rx);
    let Op::UserTurn {
        items,
        collaboration_mode,
        ..
    } = op
    else {
        panic!("expected Op::UserTurn, got {op:?}");
    };
    assert_eq!(
        collaboration_mode.as_ref().map(|mode| mode.mode),
        Some(ModeKind::Plan)
    );
    let text = user_turn_text(&items);
    assert!(text.ends_with(saved_plans::SAVED_PLAN_REVISE_MESSAGE));
}

#[tokio::test]
async fn attaching_a_saved_plan_prefixes_only_the_next_message() {
    let (mut chat, mut rx, mut op_rx) = make_chatwidget_manual(Some("gpt-5.2")).await;
    chat.thread_id = Some(ThreadId::new());
    chat.set_feature_enabled(Feature::CollaborationModes, /*enabled*/ true);

    chat.apply_loaded_plan(
        plan_read("a.md", "Newer plan", "# Newer plan\n- step\n"),
        SavedPlanAction::AttachToNextMessage,
    );
    assert_eq!(
        chat.pending_saved_plan_title().as_deref(),
        Some("Newer plan")
    );
    let _ = drain_insert_history(&mut rx);

    chat.bottom_pane
        .set_composer_text("do it".to_string(), Vec::new(), Vec::new());
    chat.handle_key_event(KeyEvent::from(KeyCode::Enter));
    let first = user_turn_text(&submit_items(next_submit_op(&mut op_rx)));
    assert!(first.starts_with("# Saved plan: Newer plan"));
    assert!(first.ends_with("## My request for Codex:\ndo it"));

    chat.on_task_complete(
        /*last_agent_message*/ None, /*duration_ms*/ None, /*from_replay*/ false,
    );
    chat.bottom_pane
        .set_composer_text("and again".to_string(), Vec::new(), Vec::new());
    chat.handle_key_event(KeyEvent::from(KeyCode::Enter));
    let second = user_turn_text(&submit_items(next_submit_op(&mut op_rx)));
    assert_eq!(second, "and again");
}

#[tokio::test]
async fn plan_saved_hint_shows_for_live_turns_only() {
    let (mut chat, mut rx, _op_rx) = make_chatwidget_manual(Some("gpt-5.2")).await;
    chat.set_feature_enabled(Feature::CollaborationModes, /*enabled*/ true);
    let plan_mask = collaboration_modes::plan_mask(chat.model_catalog.as_ref())
        .expect("expected plan collaboration mode");
    chat.set_collaboration_mask(plan_mask);
    let _ = drain_insert_history(&mut rx);

    chat.handle_thread_item(
        ThreadItem::Plan {
            id: "turn-1-plan".to_string(),
            text: "# Final plan\n".to_string(),
        },
        "turn-1".to_string(),
        ThreadItemRenderSource::Live,
    );
    let live = drain_insert_history(&mut rx)
        .iter()
        .map(|lines| lines_to_single_string(lines))
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        live.contains(saved_plans::PLAN_SAVED_HINT),
        "expected the saved-plan hint, got: {live:?}"
    );

    chat.handle_thread_item(
        ThreadItem::Plan {
            id: "turn-2-plan".to_string(),
            text: "# Final plan\n".to_string(),
        },
        "turn-2".to_string(),
        ThreadItemRenderSource::Replay(ReplayKind::ResumeInitialMessages),
    );
    let replayed = drain_insert_history(&mut rx)
        .iter()
        .map(|lines| lines_to_single_string(lines))
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        !replayed.contains(saved_plans::PLAN_SAVED_HINT),
        "replayed plans must not repeat the hint, got: {replayed:?}"
    );
}

fn submit_items(op: Op) -> Vec<UserInput> {
    match op {
        Op::UserTurn { items, .. } => items,
        other => panic!("expected Op::UserTurn, got {other:?}"),
    }
}

fn user_turn_text(items: &[UserInput]) -> String {
    items
        .iter()
        .find_map(|item| match item {
            UserInput::Text { text, .. } => Some(text.clone()),
            _ => None,
        })
        .expect("user turn should carry text")
}

#[tokio::test]
async fn plans_picker_loaded_snapshot() {
    let (mut chat, _rx, _op_rx) = make_chatwidget_manual(/*model_override*/ None).await;
    chat.show_plans_picker(vec![
        plan_summary(
            "a.md",
            "Newer plan",
            Some("/home/user/project"),
            1_756_300_000,
        ),
        plan_summary(
            "b.md",
            "Older plan",
            Some("/home/user/other"),
            1_756_200_000,
        ),
    ]);

    // The row description renders `updated_at` in local time, so mask it to keep the snapshot
    // stable across time zones.
    let popup = mask_timestamps(normalize_snapshot_paths(render_bottom_popup(
        &chat, /*width*/ 80,
    )));
    assert_chatwidget_snapshot!("plans_picker_loaded", popup);
}

#[tokio::test]
async fn saved_plan_actions_popup_snapshot() {
    let (mut chat, _rx, _op_rx) = make_chatwidget_manual(/*model_override*/ None).await;
    chat.set_feature_enabled(Feature::CollaborationModes, /*enabled*/ true);
    chat.show_saved_plan_actions("a.md".to_string(), "Newer plan".to_string());

    let popup = normalize_snapshot_paths(render_bottom_popup(&chat, /*width*/ 80));
    assert_chatwidget_snapshot!("saved_plan_actions_popup", popup);
}

/// Replace `YYYY-MM-DD HH:MM` runs so local-time rendering stays snapshot-stable.
fn mask_timestamps(text: String) -> String {
    let mut out = String::with_capacity(text.len());
    let chars: Vec<char> = text.chars().collect();
    let mut index = 0;
    while index < chars.len() {
        let is_timestamp = index + 16 <= chars.len()
            && chars[index..index + 16]
                .iter()
                .enumerate()
                .all(|(offset, ch)| match offset {
                    4 | 7 => *ch == '-',
                    10 => *ch == ' ',
                    13 => *ch == ':',
                    _ => ch.is_ascii_digit(),
                });
        if is_timestamp {
            out.push_str("<updated_at>");
            index += 16;
        } else {
            out.push(chars[index]);
            index += 1;
        }
    }
    out
}
