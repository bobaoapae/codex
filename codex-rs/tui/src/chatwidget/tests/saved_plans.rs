//! FORK: `/plans` — browsing and loading plans persisted by Plan mode.

use super::*;
use crate::app_event::SavedPlanAction;
use crate::chatwidget::saved_plans;
use codex_app_server_protocol::ApprovedPlanRef;
use codex_app_server_protocol::PlanApproveResponse;
use codex_app_server_protocol::PlanLifecycle;
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
        lifecycle: PlanLifecycle::Draft,
    }
}

fn plan_read(id: &str, title: &str, markdown: &str) -> PlanReadResponse {
    PlanReadResponse {
        plan: plan_summary(id, title, Some("/home/user/project"), 1_756_300_000),
        markdown: markdown.to_string(),
    }
}

fn approved_plan_read(id: &str, title: &str, markdown: &str) -> PlanReadResponse {
    let mut plan = plan_read(id, title, markdown);
    plan.plan.lifecycle = PlanLifecycle::Approved;
    plan
}

fn superseded_plan_read(id: &str, title: &str, markdown: &str) -> PlanReadResponse {
    let mut plan = approved_plan_read(id, title, markdown);
    plan.plan.lifecycle = PlanLifecycle::Superseded;
    plan
}

fn approved_plan_response(id: &str, title: &str) -> PlanApproveResponse {
    let mut plan = plan_summary(id, title, Some("/home/user/project"), 1_756_300_000);
    plan.lifecycle = PlanLifecycle::Approved;
    PlanApproveResponse {
        approved_plan: ApprovedPlanRef {
            id: id.to_string(),
            revision: plan.revision,
        },
        plan,
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
            AppEvent::OpenSavedPlanActions {
                id,
                title,
                revision,
                lifecycle,
            } => Some((id.clone(), title.clone(), *revision, *lifecycle)),
            _ => None,
        })
        .unwrap_or_else(|| panic!("expected OpenSavedPlanActions; events: {events:?}"));
    assert_eq!(
        opened,
        (
            "a.md".to_string(),
            "Newer plan".to_string(),
            2,
            PlanLifecycle::Draft
        )
    );
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
        chat.show_saved_plan_actions(
            "a.md".to_string(),
            "Newer plan".to_string(),
            2,
            PlanLifecycle::Draft,
        );
        for _ in 0..downs {
            chat.handle_key_event(KeyEvent::from(KeyCode::Down));
        }
        chat.handle_key_event(KeyEvent::from(KeyCode::Enter));

        let events = std::iter::from_fn(|| rx.try_recv().ok()).collect::<Vec<_>>();
        let action = events
            .iter()
            .find_map(|event| match event {
                AppEvent::LoadSavedPlan {
                    id,
                    revision,
                    lifecycle,
                    action,
                } if id == "a.md" => Some((*revision, *lifecycle, *action)),
                _ => None,
            })
            .unwrap_or_else(|| panic!("expected LoadSavedPlan; events: {events:?}"));
        assert_eq!(action, (2, PlanLifecycle::Draft, expected));
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
async fn implementing_an_approved_plan_submits_with_a_pinned_reference() {
    let (mut chat, mut rx, mut op_rx) = make_chatwidget_manual(Some("gpt-5.2")).await;
    chat.thread_id = Some(ThreadId::new());
    chat.set_feature_enabled(Feature::CollaborationModes, /*enabled*/ true);
    let _ = drain_insert_history(&mut rx);

    chat.apply_loaded_plan(
        approved_plan_read("a.md", "Newer plan", "# Newer plan\n- step\n"),
        2,
        SavedPlanAction::Implement,
    );

    let op = next_submit_op(&mut op_rx);
    let Op::UserTurn {
        items,
        collaboration_mode,
        approved_plan,
        ..
    } = op
    else {
        panic!("expected Op::UserTurn, got {op:?}");
    };
    assert_eq!(
        collaboration_mode.as_ref().map(|mode| mode.mode),
        Some(ModeKind::Default)
    );
    assert_eq!(
        approved_plan,
        Some(ApprovedPlanRef {
            id: "a.md".to_string(),
            revision: 2,
        })
    );
    let text = user_turn_text(&items);
    assert_eq!(
        text,
        plan_implementation::PLAN_IMPLEMENTATION_CODING_MESSAGE
    );
    let snapshot = format!("approvedPlan={approved_plan:?}; message={text}");
    assert_snapshot!(
        snapshot.as_str(),
        @r###"approvedPlan=Some(ApprovedPlanRef { id: "a.md", revision: 2 }); message=Implement the plan."###
    );
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
        approved_plan_read("a.md", "Newer plan", "# Newer plan\n- step\n"),
        2,
        SavedPlanAction::Revise,
    );

    let op = next_submit_op(&mut op_rx);
    let Op::UserTurn {
        items,
        collaboration_mode,
        approved_plan,
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
    assert_eq!(text, saved_plans::SAVED_PLAN_REVISE_MESSAGE);
    assert_eq!(
        approved_plan,
        Some(ApprovedPlanRef {
            id: "a.md".to_string(),
            revision: 2,
        })
    );
}

#[tokio::test]
async fn implementing_a_draft_approves_before_starting_the_turn() {
    let (mut chat, mut rx, mut op_rx) = make_chatwidget_manual(Some("gpt-5.2")).await;
    chat.thread_id = Some(ThreadId::new());
    chat.set_feature_enabled(Feature::CollaborationModes, /*enabled*/ true);
    let _ = drain_insert_history(&mut rx);

    chat.apply_loaded_plan(
        plan_read("a.md", "Newer plan", "# Newer plan\n- step\n"),
        2,
        SavedPlanAction::Implement,
    );
    let events = std::iter::from_fn(|| rx.try_recv().ok()).collect::<Vec<_>>();
    assert!(events.iter().any(|event| {
        matches!(
            event,
            AppEvent::ApproveSavedPlan {
                id,
                expected_revision: 2,
                action: SavedPlanAction::Implement,
            } if id == "a.md"
        )
    }));
    assert_no_submit_op(&mut op_rx);

    // Simulate the app-server's successful CAS response. The subsequent turn still carries the
    // exact immutable reference, never the draft body.
    let request_id = chat.begin_saved_plan_approval();
    assert!(chat.finish_saved_plan_approval(request_id));
    let response = approved_plan_response("a.md", "Newer plan");
    let approved_plan = response.approved_plan.clone();
    chat.apply_approved_plan(
        response.plan,
        approved_plan.clone(),
        SavedPlanAction::Implement,
    );

    let Op::UserTurn {
        items,
        approved_plan: submitted_plan,
        ..
    } = next_submit_op(&mut op_rx)
    else {
        panic!("expected Op::UserTurn after approval");
    };
    assert_eq!(submitted_plan, Some(approved_plan.clone()));
    assert_eq!(
        user_turn_text(&items),
        plan_implementation::PLAN_IMPLEMENTATION_CODING_MESSAGE
    );
    let snapshot = format!(
        "approvedPlan={approved_plan:?}; message={}",
        user_turn_text(&items)
    );
    assert_snapshot!(
        snapshot.as_str(),
        @r###"approvedPlan=ApprovedPlanRef { id: "a.md", revision: 2 }; message=Implement the plan."###
    );
}

#[tokio::test]
async fn attaching_a_saved_plan_uses_the_typed_reference_only_once() {
    let (mut chat, mut rx, mut op_rx) = make_chatwidget_manual(Some("gpt-5.2")).await;
    chat.thread_id = Some(ThreadId::new());
    chat.set_feature_enabled(Feature::CollaborationModes, /*enabled*/ true);

    chat.apply_loaded_plan(
        approved_plan_read("a.md", "Newer plan", "# Newer plan\n- step\n"),
        2,
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
    let first_op = next_submit_op(&mut op_rx);
    let Op::UserTurn {
        items: first_items,
        approved_plan: first_plan,
        ..
    } = first_op
    else {
        panic!("expected first Op::UserTurn");
    };
    assert_eq!(user_turn_text(&first_items), "do it");
    assert_eq!(
        first_plan,
        Some(ApprovedPlanRef {
            id: "a.md".to_string(),
            revision: 2,
        })
    );

    chat.on_task_complete(
        /*last_agent_message*/ None, /*duration_ms*/ None, /*from_replay*/ false,
    );
    chat.bottom_pane
        .set_composer_text("and again".to_string(), Vec::new(), Vec::new());
    chat.handle_key_event(KeyEvent::from(KeyCode::Enter));
    let second_op = next_submit_op(&mut op_rx);
    let Op::UserTurn {
        items: second_items,
        approved_plan: second_plan,
        ..
    } = second_op
    else {
        panic!("expected second Op::UserTurn");
    };
    assert_eq!(user_turn_text(&second_items), "and again");
    assert_eq!(second_plan, None);
}

#[tokio::test]
async fn superseded_saved_plan_never_starts_a_turn() {
    let (mut chat, mut rx, mut op_rx) = make_chatwidget_manual(Some("gpt-5.2")).await;
    chat.thread_id = Some(ThreadId::new());
    let _ = drain_insert_history(&mut rx);

    chat.apply_loaded_plan(
        superseded_plan_read("a.md", "Newer plan", "# Newer plan\n- step\n"),
        2,
        SavedPlanAction::Implement,
    );
    assert_no_submit_op(&mut op_rx);
    let history = drain_insert_history(&mut rx)
        .iter()
        .map(|lines| lines_to_single_string(lines))
        .collect::<Vec<_>>()
        .join("\n");
    assert!(history.contains(saved_plans::SAVED_PLAN_SUPERSEDED));
    assert_snapshot!("superseded_saved_plan_rejection", history);
}

#[tokio::test]
async fn stale_saved_plan_read_never_executes_a_newer_revision() {
    let (mut chat, mut rx, mut op_rx) = make_chatwidget_manual(Some("gpt-5.2")).await;
    chat.thread_id = Some(ThreadId::new());
    let _ = drain_insert_history(&mut rx);
    let mut stale = approved_plan_read("a.md", "Newer plan", "# Newer plan\n- step\n");
    stale.plan.revision = 3;

    chat.apply_loaded_plan(stale, 2, SavedPlanAction::Implement);
    assert_no_submit_op(&mut op_rx);
    let history = drain_insert_history(&mut rx)
        .iter()
        .map(|lines| lines_to_single_string(lines))
        .collect::<Vec<_>>()
        .join("\n");
    assert!(history.contains(saved_plans::SAVED_PLAN_SUPERSEDED));
    assert_snapshot!("stale_saved_plan_read_rejection", history);
}

#[tokio::test]
async fn plan_approval_conflict_explains_refresh_action() {
    let (mut chat, mut rx, _op_rx) = make_chatwidget_manual(/*model_override*/ None).await;
    let _ = drain_insert_history(&mut rx);

    chat.show_saved_plan_approval_conflict(
        "plan revision changed; expected revision 2".to_string(),
    );
    let history = drain_insert_history(&mut rx)
        .iter()
        .map(|lines| lines_to_single_string(lines))
        .collect::<Vec<_>>()
        .join("\n");
    assert!(history.contains("Refresh /plans"));
    assert_snapshot!("saved_plan_approval_conflict", history);
}

#[tokio::test]
async fn goal_conflict_explains_why_the_pinned_plan_did_not_start() {
    let (mut chat, mut rx, _op_rx) = make_chatwidget_manual(/*model_override*/ None).await;
    let _ = drain_insert_history(&mut rx);
    chat.input_queue.user_turn_pending_start = true;

    assert!(chat.handle_turn_start_rejection(
        "Failed to start turn: goalConflict: thread already has an unfinished goal".to_string()
    ));
    let history = drain_insert_history(&mut rx)
        .iter()
        .map(|lines| lines_to_single_string(lines))
        .collect::<Vec<_>>()
        .join("\n");
    assert!(history.contains("unfinished goal"));
    assert!(history.contains("Resolve that goal"));
    assert_snapshot!("saved_plan_goal_conflict", history);
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
async fn plans_picker_lifecycle_snapshot() {
    let (mut chat, _rx, _op_rx) = make_chatwidget_manual(/*model_override*/ None).await;
    let mut approved = plan_summary(
        "approved.md",
        "Approved plan",
        Some("/home/user/project"),
        1_756_300_000,
    );
    approved.lifecycle = PlanLifecycle::Approved;
    let mut superseded = plan_summary(
        "superseded.md",
        "Superseded plan",
        Some("/home/user/other"),
        1_756_200_000,
    );
    superseded.lifecycle = PlanLifecycle::Superseded;
    chat.show_plans_picker(vec![approved, superseded]);

    let popup = mask_timestamps(normalize_snapshot_paths(render_bottom_popup(
        &chat, /*width*/ 80,
    )));
    assert_chatwidget_snapshot!("plans_picker_lifecycle", popup);
}

#[tokio::test]
async fn saved_plan_actions_popup_snapshot() {
    let (mut chat, _rx, _op_rx) = make_chatwidget_manual(/*model_override*/ None).await;
    chat.set_feature_enabled(Feature::CollaborationModes, /*enabled*/ true);
    chat.show_saved_plan_actions(
        "a.md".to_string(),
        "Newer plan".to_string(),
        2,
        PlanLifecycle::Approved,
    );

    let popup = normalize_snapshot_paths(render_bottom_popup(&chat, /*width*/ 80));
    assert_chatwidget_snapshot!("saved_plan_actions_popup", popup);
}

#[tokio::test]
async fn superseded_plan_actions_popup_shows_lifecycle_and_blocks_actions() {
    let (mut chat, _rx, _op_rx) = make_chatwidget_manual(/*model_override*/ None).await;
    chat.set_feature_enabled(Feature::CollaborationModes, /*enabled*/ true);
    chat.show_saved_plan_actions(
        "a.md".to_string(),
        "Newer plan".to_string(),
        1,
        PlanLifecycle::Superseded,
    );

    let popup = normalize_snapshot_paths(render_bottom_popup(&chat, /*width*/ 80));
    assert_chatwidget_snapshot!("superseded_plan_actions_popup", popup);
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
