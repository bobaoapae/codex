use super::*;

use crate::app_event::AppEvent;
use crate::chatwidget::tests::make_chatwidget_manual_with_sender;
use crate::slash_command::SlashCommand;
use codex_app_server_protocol::CompactionSurvival;
use codex_app_server_protocol::ContextInspection;
use codex_app_server_protocol::ContextInspectionGroup;
use codex_app_server_protocol::ContextInspectionItem;
use codex_app_server_protocol::ContextLogicalOrigin;
use codex_app_server_protocol::ContextRuntimeBuildInfo;
use codex_app_server_protocol::ContextSnapshotKind;
use codex_app_server_protocol::ContextVisibility;
use insta::assert_snapshot;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::text::Text;
use ratatui::widgets::Paragraph;
use ratatui::widgets::Widget;
use ratatui::widgets::Wrap;

fn group(
    index: usize,
    item_count: usize,
    content_kind: &str,
    origin: ContextLogicalOrigin,
    visibility: ContextVisibility,
    encrypted: bool,
    duplicate_group: Option<&str>,
    duplicate_count: usize,
    preview: Option<&str>,
) -> ContextInspectionGroup {
    ContextInspectionGroup {
        index,
        item_count,
        role: "developer".to_string(),
        content_kind: content_kind.to_string(),
        logical_origin: origin,
        visibility,
        estimated_tokens: 20,
        serialized_bytes: 80,
        survives_compaction: CompactionSurvival::True,
        encrypted,
        duplicate_group: duplicate_group.map(str::to_string),
        duplicate_count,
        preview: preview.map(str::to_string),
    }
}

fn item(
    index: usize,
    content_kind: &str,
    origin: ContextLogicalOrigin,
    visibility: ContextVisibility,
    encrypted: bool,
    duplicate_group: Option<&str>,
    duplicate_count: usize,
    preview: Option<&str>,
) -> ContextInspectionItem {
    ContextInspectionItem {
        index,
        role: "user".to_string(),
        content_kind: content_kind.to_string(),
        logical_origin: origin,
        visibility,
        estimated_tokens: 40,
        serialized_bytes: 160,
        survives_compaction: CompactionSurvival::False,
        encrypted,
        duplicate_group: duplicate_group.map(str::to_string),
        duplicate_count,
        preview: preview.map(str::to_string),
    }
}

fn inspection() -> ContextInspection {
    ContextInspection {
        thread_id: "thread-normal".to_string(),
        turn_id: Some("turn-7".to_string()),
        snapshot_kind: ContextSnapshotKind::Live,
        partial: false,
        item_count: 2,
        estimated_prompt_tokens: Some(12_345),
        estimated_active_tokens: Some(8_765),
        estimated_context_window_tokens: Some(128_000),
        cached_input_tokens: Some(4_000),
        uncached_input_tokens: Some(3_000),
        cache_write_input_tokens: Some(100),
        runtime_build_info: Some(ContextRuntimeBuildInfo {
            version: "1.2.3".to_string(),
            build_commit: "abcdef1234567890".to_string(),
            target: "x86_64".to_string(),
        }),
        config_layer_revision: Some("config-7".to_string()),
        runtime_feature_revision: Some("features-3".to_string()),
        persisted_runtime_build_info: Some(ContextRuntimeBuildInfo {
            version: "1.2.3".to_string(),
            build_commit: "abcdef1234567890".to_string(),
            target: "x86_64".to_string(),
        }),
        persisted_config_layer_revision: Some("config-7".to_string()),
        persisted_runtime_feature_revision: Some("features-3".to_string()),
        stale: false,
        window_id: Some("window-1".to_string()),
        context_window_id: Some("context-window-1".to_string()),
        window_number: Some(2),
        first_window_id: Some("window-0".to_string()),
        previous_window_id: Some("window-0".to_string()),
        checkpoint_id: Some("checkpoint-1".to_string()),
        checkpoint_revision: Some(4),
        base_instructions: group(
            0,
            1,
            "baseInstructions",
            ContextLogicalOrigin::BaseInstructions,
            ContextVisibility::Model,
            false,
            None,
            1,
            Some("base instructions"),
        ),
        tools: group(
            1,
            1,
            "tools",
            ContextLogicalOrigin::ThreadContext,
            ContextVisibility::Internal,
            false,
            None,
            1,
            Some("tool definitions"),
        ),
        items: vec![item(
            2,
            "user.text",
            ContextLogicalOrigin::TurnContext,
            ContextVisibility::User,
            false,
            None,
            1,
            Some("hello from the user"),
        )],
    }
}

fn render(context: &ContextInspection, include_preview: bool) -> String {
    context_summary_lines(context, include_preview)
        .iter()
        .map(|line| {
            line.spans
                .iter()
                .map(|span| span.content.as_ref())
                .collect::<String>()
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[test]
fn normal_context_summary_snapshot() {
    assert_snapshot!(render(&inspection(), false), @r###"
/context
Context inspection
Snapshot: live
Thread: thread-normal
Items: 2
Tokens: prompt 12.3K · active 8.77K · window 128K
Cache: cached 4K · uncached 3K · write 100
Build: current 1.2.3 abcdef1234567890 (x86_64) · persisted 1.2.3 abcdef1234567890 (x86_64)
Config: layer config-7 · features features-3 · persisted layer config-7 · persisted features features-3
Window: id window-1 · context context-window-1 · number 2 · first window-0 · previous window-0
Checkpoint: id checkpoint-1 · revision 4
Origins: baseInstructions×1 · threadContext×1 · turnContext×1
Content kinds: baseInstructions×1 · tools×1 · user.text×1
Visibility: internal×1 · model×1 · user×1
Compaction: doesNotSurvive×1 · survives×2
Duplicates: none
"###);
}

#[test]
fn cold_partial_context_summary_snapshot() {
    let mut context = inspection();
    context.snapshot_kind = ContextSnapshotKind::Cold;
    context.partial = true;
    context.item_count = 0;
    context.runtime_build_info = None;
    context.persisted_runtime_build_info = None;
    context.base_instructions = group(
        0,
        0,
        "baseInstructions",
        ContextLogicalOrigin::BaseInstructions,
        ContextVisibility::Model,
        false,
        None,
        1,
        None,
    );
    context.tools = context.base_instructions.clone();
    context.items.clear();
    assert_snapshot!(render(&context, false), @r###"
/context
Context inspection
Snapshot: cold · partial
Thread: thread-normal
Items: 0
Tokens: prompt 12.3K · active 8.77K · window 128K
Cache: cached 4K · uncached 3K · write 100
Build: current unknown · persisted unknown
Config: layer config-7 · features features-3 · persisted layer config-7 · persisted features features-3
Window: id window-1 · context context-window-1 · number 2 · first window-0 · previous window-0
Checkpoint: id checkpoint-1 · revision 4
Origins: none
Content kinds: none
Visibility: none
Compaction: none
Duplicates: none
"###);
}

#[test]
fn stale_build_summary_snapshot() {
    let mut context = inspection();
    context.stale = true;
    context.persisted_runtime_build_info = Some(ContextRuntimeBuildInfo {
        version: "1.1.0".to_string(),
        build_commit: "old-commit".to_string(),
        target: "x86_64".to_string(),
    });
    context.persisted_config_layer_revision = Some("config-2".to_string());
    context.persisted_runtime_feature_revision = Some("features-1".to_string());
    assert_snapshot!(render(&context, false), @r###"
/context
Context inspection
Snapshot: live · stale
Thread: thread-normal
Items: 2
Tokens: prompt 12.3K · active 8.77K · window 128K
Cache: cached 4K · uncached 3K · write 100
Build: current 1.2.3 abcdef1234567890 (x86_64) · persisted 1.1.0 old-commit (x86_64) · stale
Config: layer config-7 · features features-3 · persisted layer config-2 · persisted features features-1 · stale
Window: id window-1 · context context-window-1 · number 2 · first window-0 · previous window-0
Checkpoint: id checkpoint-1 · revision 4
Origins: baseInstructions×1 · threadContext×1 · turnContext×1
Content kinds: baseInstructions×1 · tools×1 · user.text×1
Visibility: internal×1 · model×1 · user×1
Compaction: doesNotSurvive×1 · survives×2
Duplicates: none
"###);
}

#[test]
fn duplicate_summary_snapshot() {
    let mut context = inspection();
    context.base_instructions = group(
        0,
        2,
        "baseInstructions",
        ContextLogicalOrigin::BaseInstructions,
        ContextVisibility::Model,
        false,
        Some("same-prefix"),
        2,
        None,
    );
    context.tools = group(
        2,
        0,
        "tools",
        ContextLogicalOrigin::ThreadContext,
        ContextVisibility::Internal,
        false,
        None,
        1,
        None,
    );
    context.items = vec![item(
        3,
        "user.text",
        ContextLogicalOrigin::TurnContext,
        ContextVisibility::User,
        false,
        Some("same-prefix"),
        2,
        None,
    )];
    assert_snapshot!(render(&context, false), @r###"
/context
Context inspection
Snapshot: live
Thread: thread-normal
Items: 2
Tokens: prompt 12.3K · active 8.77K · window 128K
Cache: cached 4K · uncached 3K · write 100
Build: current 1.2.3 abcdef1234567890 (x86_64) · persisted 1.2.3 abcdef1234567890 (x86_64)
Config: layer config-7 · features features-3 · persisted layer config-7 · persisted features features-3
Window: id window-1 · context context-window-1 · number 2 · first window-0 · previous window-0
Checkpoint: id checkpoint-1 · revision 4
Origins: baseInstructions×2 · turnContext×1
Content kinds: baseInstructions×2 · user.text×1
Visibility: model×2 · user×1
Compaction: doesNotSurvive×1 · survives×2
Duplicates: same-prefix×2
"###);
}

#[test]
fn encrypted_preview_is_not_rendered_snapshot() {
    let mut context = inspection();
    context.base_instructions = group(
        0,
        1,
        "ciphertext",
        ContextLogicalOrigin::BaseInstructions,
        ContextVisibility::Model,
        true,
        None,
        1,
        Some("secret ciphertext"),
    );
    context.tools = group(
        1,
        1,
        "tool.output",
        ContextLogicalOrigin::ToolOutput,
        ContextVisibility::Internal,
        false,
        None,
        1,
        Some("secret tool output"),
    );
    context.items = vec![item(
        2,
        "media.image",
        ContextLogicalOrigin::NewOutput,
        ContextVisibility::Model,
        false,
        None,
        1,
        Some("secret image data"),
    )];
    let rendered = render(&context, true);
    assert!(!rendered.contains("secret"));
    assert_snapshot!(rendered, @r###"
/context preview
Context inspection
Snapshot: live
Thread: thread-normal
Items: 2
Tokens: prompt 12.3K · active 8.77K · window 128K
Cache: cached 4K · uncached 3K · write 100
Build: current 1.2.3 abcdef1234567890 (x86_64) · persisted 1.2.3 abcdef1234567890 (x86_64)
Config: layer config-7 · features features-3 · persisted layer config-7 · persisted features features-3
Window: id window-1 · context context-window-1 · number 2 · first window-0 · previous window-0
Checkpoint: id checkpoint-1 · revision 4
Origins: baseInstructions×1 · newOutput×1 · toolOutput×1
Content kinds: ciphertext×1 · media.image×1 · tool.output×1
Visibility: internal×1 · model×2
Compaction: doesNotSurvive×1 · survives×2
Duplicates: none
Redaction: 1 encrypted item; previews withheld
Preview (explicitly requested)
  no plaintext preview available
"###);
}

#[test]
fn narrow_width_wraps_context_lines_snapshot() {
    let mut context = inspection();
    context.thread_id = "thread-with-a-deliberately-long-id".to_string();
    context.items[0].content_kind = "a-very-long-content-kind".to_string();
    let lines = context_summary_lines(&context, false);
    let mut buffer = Buffer::empty(Rect::new(0, 0, 32, 40));
    Paragraph::new(Text::from(lines))
        .wrap(Wrap { trim: false })
        .render(Rect::new(0, 0, 32, 40), &mut buffer);
    let rendered = (0..40)
        .map(|row| {
            (0..32)
                .map(|column| buffer[(column, row)].symbol())
                .collect::<String>()
                .trim_end()
                .to_string()
        })
        .filter(|line| !line.is_empty())
        .collect::<Vec<_>>()
        .join("\n");
    assert_snapshot!(rendered, @r###"
/context
Context inspection
Snapshot: live
Thread:
thread-with-a-deliberate…
Items: 2
Tokens: prompt 12.3K · active
8.77K · window 128K
Cache: cached 4K · uncached 3K ·
write 100
Build: current 1.2.3
abcdef1234567890 (x86_64) ·
persisted 1.2.3 abcdef1234567890
(x86_64)
Config: layer config-7 ·
features features-3 · persisted
layer config-7 · persisted
features features-3
Window: id window-1 · context
context-window-1 · number 2 ·
first window-0 · previous
window-0
Checkpoint: id checkpoint-1 ·
revision 4
Origins: baseInstructions×1 ·
threadContext×1 · turnContext×1
Content kinds:
a-very-long-content-kind×1 ·
baseInstructions×1 · tools×1
Visibility: internal×1 · model×1
· user×1
Compaction: doesNotSurvive×1 ·
survives×2
Duplicates: none
"###);
}

#[test]
fn loading_and_error_states_snapshot() {
    let loading = context_loading_lines(false)
        .iter()
        .map(|line| line.to_string())
        .collect::<Vec<_>>()
        .join("\n");
    let error = context_error_lines("server unavailable")
        .iter()
        .map(|line| line.to_string())
        .collect::<Vec<_>>()
        .join("\n");
    let not_found = context_not_found_lines()
        .iter()
        .map(|line| line.to_string())
        .collect::<Vec<_>>()
        .join("\n");
    assert_snapshot!(loading, @r###"
/context
Context inspection
Loading context summary…
"###);
    assert_snapshot!(error, @r###"
/context
Context inspection
Context inspection failed
server unavailable
"###);
    assert_snapshot!(not_found, @r###"
/context
Context inspection
No context snapshot is available for this thread.
The thread may have been removed or is still starting.
"###);
}

#[test]
fn stale_request_token_is_ignored() {
    let mut state = ContextInspectionState::default();
    let first = state.begin(ThreadId::new());
    let second = state.begin(ThreadId::new());
    assert!(!state.finish(first, ThreadId::new()));
    assert!(state.finish(second, state.thread_id.expect("active thread")));
}

#[tokio::test]
async fn context_command_is_available_during_tasks_and_parent_owned_views() {
    let (mut chat, _event_sender, mut event_rx, _op_rx) =
        make_chatwidget_manual_with_sender().await;
    let thread_id = ThreadId::new();
    chat.thread_id = Some(thread_id);
    chat.on_task_started();
    chat.dispatch_command(SlashCommand::Context);
    assert!(matches!(
        event_rx.try_recv(),
        Ok(AppEvent::OpenContextInspection {
            include_preview: false
        })
    ));

    chat.blocks_direct_input = true;
    chat.dispatch_command(SlashCommand::Context);
    assert!(matches!(
        event_rx.try_recv(),
        Ok(AppEvent::OpenContextInspection {
            include_preview: false
        })
    ));
}

#[tokio::test]
async fn context_preview_is_explicit_and_opt_in() {
    let (mut chat, _event_sender, mut event_rx, _op_rx) =
        make_chatwidget_manual_with_sender().await;
    chat.thread_id = Some(ThreadId::new());
    chat.dispatch_command_with_args(SlashCommand::Context, "preview".to_string(), Vec::new());
    assert!(matches!(
        event_rx.try_recv(),
        Ok(AppEvent::OpenContextInspection {
            include_preview: true
        })
    ));
}
