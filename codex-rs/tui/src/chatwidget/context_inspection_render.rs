//! Bounded rendering for the `/context` PlainHistoryCell output.

use std::collections::BTreeMap;

use codex_app_server_protocol::CompactionSurvival;
use codex_app_server_protocol::ContextInspection;
use codex_app_server_protocol::ContextInspectionGroup;
use codex_app_server_protocol::ContextInspectionItem;
use codex_app_server_protocol::ContextLogicalOrigin;
use codex_app_server_protocol::ContextRuntimeBuildInfo;
use codex_app_server_protocol::ContextSnapshotKind;
use codex_app_server_protocol::ContextVisibility;
use ratatui::style::Stylize;
use ratatui::text::Line;

use crate::status::format_tokens_compact;

const MAX_COUNT_GROUPS: usize = 12;
const MAX_PREVIEW_ITEMS: usize = 8;
const MAX_PREVIEW_CHARS: usize = 240;
const MAX_LABEL_CHARS: usize = 64;

#[derive(Default)]
struct ContextAggregates {
    origins: BTreeMap<String, usize>,
    content_kinds: BTreeMap<String, usize>,
    visibilities: BTreeMap<String, usize>,
    compaction: BTreeMap<String, usize>,
    duplicates: BTreeMap<String, usize>,
    encrypted: usize,
}

pub(super) fn context_loading_lines(include_preview: bool) -> Vec<Line<'static>> {
    let mode = if include_preview {
        "Loading context preview…"
    } else {
        "Loading context summary…"
    };
    vec![
        "/context".magenta().into(),
        "Context inspection".bold().into(),
        mode.dim().into(),
    ]
}

pub(super) fn context_not_found_lines() -> Vec<Line<'static>> {
    vec![
        "/context".magenta().into(),
        "Context inspection".bold().into(),
        "No context snapshot is available for this thread."
            .cyan()
            .into(),
        "The thread may have been removed or is still starting."
            .dim()
            .into(),
    ]
}

pub(super) fn context_error_lines(error: &str) -> Vec<Line<'static>> {
    vec![
        "/context".magenta().into(),
        "Context inspection".bold().into(),
        "Context inspection failed".red().into(),
        sanitize_text(error, MAX_PREVIEW_CHARS).dim().into(),
    ]
}

pub(super) fn context_summary_lines(
    context: &ContextInspection,
    include_preview: bool,
) -> Vec<Line<'static>> {
    let mut lines = vec![
        if include_preview {
            "/context preview".magenta().into()
        } else {
            "/context".magenta().into()
        },
        "Context inspection".bold().into(),
        snapshot_line(context),
        labeled_line("Thread", short_id(&context.thread_id)),
        labeled_line("Items", context.item_count.to_string()),
        token_line(context),
        cache_line(context),
        build_line(context),
        config_line(context),
        window_line(context),
        checkpoint_line(context),
    ];

    let mut aggregates = ContextAggregates::default();
    aggregates.add_group(&context.base_instructions);
    aggregates.add_group(&context.tools);
    for item in &context.items {
        aggregates.add_item(item);
    }
    lines.push(labeled_line("Origins", format_counts(&aggregates.origins)));
    lines.push(labeled_line(
        "Content kinds",
        format_counts(&aggregates.content_kinds),
    ));
    lines.push(labeled_line(
        "Visibility",
        format_counts(&aggregates.visibilities),
    ));
    lines.push(labeled_line(
        "Compaction",
        format_counts(&aggregates.compaction),
    ));
    lines.push(labeled_line(
        "Duplicates",
        format_counts(&aggregates.duplicates),
    ));
    if aggregates.encrypted > 0 {
        lines.push(labeled_line(
            "Redaction",
            format!(
                "{} encrypted item{}; previews withheld",
                aggregates.encrypted,
                if aggregates.encrypted == 1 { "" } else { "s" }
            ),
        ));
    }
    append_preview_lines(&mut lines, context, include_preview);
    lines
}

impl ContextAggregates {
    fn add_group(&mut self, group: &ContextInspectionGroup) {
        let count = group.item_count;
        if count == 0 {
            return;
        }
        add_count(&mut self.origins, origin_label(group.logical_origin), count);
        add_count(
            &mut self.content_kinds,
            sanitize_text(&group.content_kind, MAX_LABEL_CHARS),
            count,
        );
        add_count(
            &mut self.visibilities,
            visibility_label(group.visibility),
            count,
        );
        add_count(
            &mut self.compaction,
            compaction_label(group.survives_compaction),
            count,
        );
        if group.encrypted {
            self.encrypted = self.encrypted.saturating_add(count);
        }
        add_duplicate(
            &mut self.duplicates,
            group.duplicate_group.as_deref(),
            group.duplicate_count,
        );
    }

    fn add_item(&mut self, item: &ContextInspectionItem) {
        add_count(&mut self.origins, origin_label(item.logical_origin), 1);
        add_count(
            &mut self.content_kinds,
            sanitize_text(&item.content_kind, MAX_LABEL_CHARS),
            1,
        );
        add_count(&mut self.visibilities, visibility_label(item.visibility), 1);
        add_count(
            &mut self.compaction,
            compaction_label(item.survives_compaction),
            1,
        );
        if item.encrypted {
            self.encrypted = self.encrypted.saturating_add(1);
        }
        add_duplicate(
            &mut self.duplicates,
            item.duplicate_group.as_deref(),
            item.duplicate_count,
        );
    }
}

fn add_count(counts: &mut BTreeMap<String, usize>, key: String, amount: usize) {
    let entry = counts.entry(key).or_default();
    *entry = entry.saturating_add(amount);
}

fn add_duplicate(counts: &mut BTreeMap<String, usize>, group: Option<&str>, count: usize) {
    let Some(group) = group else {
        return;
    };
    if count <= 1 {
        return;
    }
    let key = sanitize_text(group, MAX_LABEL_CHARS);
    let current = counts.get(&key).copied().unwrap_or_default();
    counts.insert(key, current.max(count));
}

fn format_counts(counts: &BTreeMap<String, usize>) -> String {
    if counts.is_empty() {
        return "none".to_string();
    }
    let mut parts = counts
        .iter()
        .take(MAX_COUNT_GROUPS)
        .map(|(key, count)| format!("{key}×{count}"))
        .collect::<Vec<_>>();
    if counts.len() > MAX_COUNT_GROUPS {
        parts.push(format!("+{} more", counts.len() - MAX_COUNT_GROUPS));
    }
    parts.join(" · ")
}

fn snapshot_line(context: &ContextInspection) -> Line<'static> {
    let mut value = snapshot_label(context.snapshot_kind).to_string();
    if context.partial {
        value.push_str(" · partial");
    }
    if context.stale {
        value.push_str(" · stale");
    }
    labeled_line("Snapshot", value)
}

fn token_line(context: &ContextInspection) -> Line<'static> {
    labeled_line(
        "Tokens",
        format!(
            "prompt {} · active {} · window {}",
            optional_tokens(context.estimated_prompt_tokens),
            optional_tokens(context.estimated_active_tokens),
            optional_tokens(context.estimated_context_window_tokens),
        ),
    )
}

fn cache_line(context: &ContextInspection) -> Line<'static> {
    labeled_line(
        "Cache",
        format!(
            "cached {} · uncached {} · write {}",
            optional_tokens(context.cached_input_tokens),
            optional_tokens(context.uncached_input_tokens),
            optional_tokens(context.cache_write_input_tokens),
        ),
    )
}

fn build_line(context: &ContextInspection) -> Line<'static> {
    let current = build_label(context.runtime_build_info.as_ref());
    let persisted = build_label(context.persisted_runtime_build_info.as_ref());
    let mut value = format!("current {current} · persisted {persisted}");
    if context.stale {
        value.push_str(" · stale");
    }
    labeled_line("Build", value)
}

fn config_line(context: &ContextInspection) -> Line<'static> {
    let mut value = format!(
        "layer {} · features {} · persisted layer {} · persisted features {}",
        optional_text(context.config_layer_revision.as_deref()),
        optional_text(context.runtime_feature_revision.as_deref()),
        optional_text(context.persisted_config_layer_revision.as_deref()),
        optional_text(context.persisted_runtime_feature_revision.as_deref()),
    );
    if context.stale {
        value.push_str(" · stale");
    }
    labeled_line("Config", value)
}

fn window_line(context: &ContextInspection) -> Line<'static> {
    labeled_line(
        "Window",
        format!(
            "id {} · context {} · number {} · first {} · previous {}",
            optional_text(context.window_id.as_deref()),
            optional_text(context.context_window_id.as_deref()),
            context
                .window_number
                .map_or_else(|| "unknown".to_string(), |value| value.to_string()),
            optional_text(context.first_window_id.as_deref()),
            optional_text(context.previous_window_id.as_deref()),
        ),
    )
}

fn checkpoint_line(context: &ContextInspection) -> Line<'static> {
    labeled_line(
        "Checkpoint",
        format!(
            "id {} · revision {}",
            optional_text(context.checkpoint_id.as_deref()),
            context
                .checkpoint_revision
                .map_or_else(|| "unknown".to_string(), |value| value.to_string()),
        ),
    )
}

fn append_preview_lines(
    lines: &mut Vec<Line<'static>>,
    context: &ContextInspection,
    include_preview: bool,
) {
    if !include_preview {
        return;
    }

    lines.push("Preview (explicitly requested)".bold().into());
    let mut previews = Vec::new();
    if let Some(preview) = safe_preview(
        context.base_instructions.encrypted,
        context.base_instructions.logical_origin,
        &context.base_instructions.content_kind,
        context.base_instructions.preview.as_deref(),
    ) {
        previews.push((context.base_instructions.index, preview));
    }
    if let Some(preview) = safe_preview(
        context.tools.encrypted,
        context.tools.logical_origin,
        &context.tools.content_kind,
        context.tools.preview.as_deref(),
    ) {
        previews.push((context.tools.index, preview));
    }
    previews.extend(context.items.iter().filter_map(|item| {
        safe_preview(
            item.encrypted,
            item.logical_origin,
            &item.content_kind,
            item.preview.as_deref(),
        )
        .map(|preview| (item.index, preview))
    }));

    for (index, preview) in previews.iter().take(MAX_PREVIEW_ITEMS) {
        lines.push(format!("  #{index}: {preview}").into());
    }
    if previews.is_empty() {
        lines.push("  no plaintext preview available".dim().into());
    } else if previews.len() > MAX_PREVIEW_ITEMS {
        lines.push(
            format!(
                "  +{} more previews omitted",
                previews.len() - MAX_PREVIEW_ITEMS
            )
            .dim()
            .into(),
        );
    }
}

fn safe_preview(
    encrypted: bool,
    origin: ContextLogicalOrigin,
    content_kind: &str,
    preview: Option<&str>,
) -> Option<String> {
    if encrypted || origin == ContextLogicalOrigin::ToolOutput || !safe_content_kind(content_kind) {
        return None;
    }
    let preview = preview?;
    let preview = sanitize_text(preview, MAX_PREVIEW_CHARS);
    (!preview.is_empty()).then_some(preview)
}

fn safe_content_kind(content_kind: &str) -> bool {
    let content_kind = content_kind.to_ascii_lowercase();
    ![
        "cipher", "encrypt", "tool", "output", "media", "image", "audio", "video",
    ]
    .iter()
    .any(|blocked| content_kind.contains(blocked))
}

fn labeled_line(label: &str, value: String) -> Line<'static> {
    vec![format!("{label}: ").dim(), value.into()].into()
}

fn optional_tokens(value: Option<i64>) -> String {
    value.map_or_else(|| "unknown".to_string(), format_tokens_compact)
}

fn optional_text(value: Option<&str>) -> String {
    value.map_or_else(|| "unknown".to_string(), short_id)
}

fn build_label(build: Option<&ContextRuntimeBuildInfo>) -> String {
    build.map_or_else(
        || "unknown".to_string(),
        |build| {
            format!(
                "{} {} ({})",
                sanitize_text(&build.version, MAX_LABEL_CHARS),
                short_id(&build.build_commit),
                sanitize_text(&build.target, MAX_LABEL_CHARS),
            )
        },
    )
}

fn short_id(value: &str) -> String {
    let value = sanitize_text(value, MAX_LABEL_CHARS);
    let mut chars = value.chars();
    let shortened = chars.by_ref().take(24).collect::<String>();
    if chars.next().is_some() {
        format!("{shortened}…")
    } else {
        shortened
    }
}

fn sanitize_text(value: &str, max_chars: usize) -> String {
    value
        .chars()
        .filter(|character| !character.is_control())
        .take(max_chars)
        .collect()
}

fn snapshot_label(value: ContextSnapshotKind) -> &'static str {
    match value {
        ContextSnapshotKind::Live => "live",
        ContextSnapshotKind::Speculative => "speculative",
        ContextSnapshotKind::Cold => "cold",
    }
}

fn origin_label(value: ContextLogicalOrigin) -> String {
    match value {
        ContextLogicalOrigin::BaseInstructions => "baseInstructions",
        ContextLogicalOrigin::ThreadContext => "threadContext",
        ContextLogicalOrigin::TurnContext => "turnContext",
        ContextLogicalOrigin::WorldState => "worldState",
        ContextLogicalOrigin::InheritedHistory => "inheritedHistory",
        ContextLogicalOrigin::NewOutput => "newOutput",
        ContextLogicalOrigin::ToolOutput => "toolOutput",
        ContextLogicalOrigin::CompactionReplacement => "compactionReplacement",
        ContextLogicalOrigin::Derived => "derived",
        ContextLogicalOrigin::Unknown => "unknown",
    }
    .to_string()
}

fn visibility_label(value: ContextVisibility) -> String {
    match value {
        ContextVisibility::Model => "model",
        ContextVisibility::User => "user",
        ContextVisibility::Internal => "internal",
        ContextVisibility::Unknown => "unknown",
    }
    .to_string()
}

fn compaction_label(value: CompactionSurvival) -> String {
    match value {
        CompactionSurvival::True => "survives",
        CompactionSurvival::False => "doesNotSurvive",
        CompactionSurvival::Unknown => "unknown",
    }
    .to_string()
}
