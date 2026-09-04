//! Read-only inspection of the model context assembled by core.
//!
//! This module intentionally observes the same `ContextManager::for_prompt` and
//! `session::turn::build_prompt` path used by sampling. It never refreshes context contributors,
//! advances a context window, records rollout items, or attaches cache accounting to an item.

#[path = "context_inspection_items.rs"]
mod items;
#[path = "context_inspection_preview.rs"]
mod preview;
#[path = "context_inspection_provenance.rs"]
mod provenance;
#[path = "context_inspection_types.rs"]
mod types;

pub use types::CompactionSurvival;
pub use types::ContextCacheMetrics;
pub use types::ContextInspection;
pub use types::ContextInspectionGroup;
pub use types::ContextInspectionItem;
pub use types::ContextInspectionMode;
pub use types::ContextInspectionOptions;
pub use types::ContextLogicalOrigin;
pub use types::ContextSnapshotKind;
pub use types::ContextVisibility;

use crate::client_common::Prompt;
use crate::codex_thread::CodexThread;
use crate::context_manager::ContextManager;
use crate::session::rollout_reconstruction::reconstruct_history_from_rollout_items_with_policy;
use crate::session::session::Session;
use crate::session::turn::build_prompt;
use codex_features::Feature;
use codex_models_manager::ModelsManagerConfig;
use codex_models_manager::manager::ModelsManager;
use codex_protocol::ThreadId;
use codex_protocol::error::CodexErr;
use codex_protocol::error::Result as CodexResult;
use codex_protocol::models::BaseInstructions;
use codex_protocol::models::ResponseItem;
use codex_protocol::openai_models::ModelInfo;
use codex_protocol::openai_models::default_input_modalities;
use codex_protocol::protocol::RuntimeBuildInfo;
use codex_protocol::protocol::TokenUsage;
use codex_thread_store::LoadThreadHistoryParams;
use codex_thread_store::StoredModelContext;
use codex_utils_output_truncation::TruncationPolicy;
use items::PreviewBudget;
use items::active_tokens;
use items::active_tokens_from_usage;
use items::cache_metrics_from_usage;
use items::inspect_base_instructions;
use items::inspect_items;
use items::inspect_tools;
use provenance::CheckpointMetadata;
use provenance::PersistedMetadata;
use provenance::latest_checkpoint;
use provenance::latest_replacement_items;
use provenance::latest_token_usage;
use provenance::latest_turn_id;
use provenance::load_persisted_items;
use provenance::persisted_metadata;
use provenance::prompt_item_count_through_rollout_boundary;
use provenance::provenance_is_stale;
use std::sync::Arc;

pub(crate) use provenance::persisted_parent_thread_id;

const MAX_PREVIEW_CHARS: usize = 512;
const MAX_PREVIEW_TOKENS: usize = 10_000;
const MAX_PREVIEW_SOURCE_CHARS: usize = MAX_PREVIEW_CHARS.saturating_mul(8);
const REDACTED: &str = "[REDACTED]";
const SECRET_KEYS: &[&str] = &[
    "access-token",
    "api_key",
    "api-key",
    "apikey",
    "access_token",
    "auth_token",
    "auth-token",
    "authorization",
    "bearer",
    "client_secret",
    "client-secret",
    "cookie",
    "credential",
    "password",
    "private_key",
    "private-key",
    "secret",
    "session_token",
    "session-token",
    "token",
];

/// Inspect the loaded or cold context for a thread.
pub async fn inspect_thread(
    thread: &CodexThread,
    options: ContextInspectionOptions,
) -> CodexResult<ContextInspection> {
    match options.mode {
        ContextInspectionMode::Loaded => inspect_loaded_thread(thread, options).await,
        ContextInspectionMode::Cold => inspect_cold_thread(thread, options).await,
    }
}

/// Inspect the current in-memory history without refreshing dynamic contributors.
pub async fn inspect_loaded_thread(
    thread: &CodexThread,
    options: ContextInspectionOptions,
) -> CodexResult<ContextInspection> {
    inspect_loaded_session(&thread.session, &options).await
}

/// Rebuild the latest persisted model context without mutating the loaded session.
pub async fn inspect_cold_thread(
    thread: &CodexThread,
    options: ContextInspectionOptions,
) -> CodexResult<ContextInspection> {
    inspect_cold_session(&thread.session, &options).await
}

/// Rebuild a stored thread's latest model context without constructing a session.
///
/// This is the detached counterpart to [`inspect_cold_thread`]. The caller supplies the model
/// manager/config snapshot and any durable fork-boundary hint it has available. Model metadata is
/// resolved from the manager's in-memory catalog only; this function never refreshes providers,
/// dynamic contributors, rollout state, or session locks.
pub(crate) async fn inspect_stored_context(
    thread_id: ThreadId,
    stored: StoredModelContext,
    model: Option<String>,
    models_manager: &dyn ModelsManager,
    models_manager_config: &ModelsManagerConfig,
    runtime_build_info: Option<RuntimeBuildInfo>,
    config_layer_revision: Option<String>,
    runtime_feature_revision: Option<String>,
    inherited_rollout_count: Option<usize>,
    options: ContextInspectionOptions,
) -> CodexResult<ContextInspection> {
    if stored.thread_id != thread_id {
        return Err(CodexErr::Fatal(format!(
            "stored model context belongs to {}, not {thread_id}",
            stored.thread_id
        )));
    }

    let persisted = persisted_metadata(&stored.items, thread_id);
    let checkpoint = latest_checkpoint(&stored.items);
    let replacement_items = latest_replacement_items(&stored.items);
    let inherited_item_count = inherited_rollout_count
        .map(|count| prompt_item_count_through_rollout_boundary(&stored.items, count));

    let model_info = if let Some(model) = model.as_deref() {
        Some(
            models_manager
                .get_model_info(model, models_manager_config)
                .await,
        )
    } else {
        None
    };
    let truncation_policy = model_info
        .as_ref()
        .map(|model| model.truncation_policy.into())
        .unwrap_or(TruncationPolicy::Bytes(usize::MAX));
    let reconstruction = reconstruct_history_from_rollout_items_with_policy(
        truncation_policy,
        &stored.items,
        // FORK: detached inspection has no session to read the feature off, so
        // it takes the feature's own default.
        Feature::GuardianThreadContext.default_enabled(),
    );
    let mut history = ContextManager::new();
    history.replace_annotated(reconstruction.history);
    let default_modalities;
    let input_modalities = if let Some(model) = model_info.as_ref() {
        model.input_modalities.as_slice()
    } else {
        default_modalities = default_input_modalities();
        default_modalities.as_slice()
    };
    let input = history.for_prompt(input_modalities);
    let base_instructions = persisted
        .base_instructions
        .clone()
        .or_else(|| {
            model_info.as_ref().map(|model| BaseInstructions {
                text: model.get_model_instructions(models_manager_config.personality),
                provenance: None,
            })
        })
        .unwrap_or_default();
    let prompt = Prompt {
        input,
        base_instructions,
        ..Prompt::default()
    };
    let turn_id = options
        .turn_id
        .clone()
        .or_else(|| latest_turn_id(&stored.items));
    let usage = latest_token_usage(&stored.items);
    let window_number = reconstruction.window_number;
    let window_id = reconstruction
        .window_id
        .map(|id| id.to_string())
        .or_else(|| Some(format!("{thread_id}:{window_number}")));

    Ok(build_inspection_with_metadata(
        thread_id,
        &prompt,
        &InspectionAssembly {
            snapshot_kind: ContextSnapshotKind::Cold,
            turn_id,
            dynamic_context_available: false,
            source_available: persisted.available,
            inherited_item_count,
            replacement_items,
            active_tokens: usage.as_ref().map(active_tokens_from_usage),
            usage,
            context_window_tokens: model_info
                .as_ref()
                .and_then(ModelInfo::usable_context_window),
            window_id,
            window_number: Some(window_number),
            first_window_id: reconstruction.first_window_id.map(|id| id.to_string()),
            previous_window_id: reconstruction.previous_window_id.map(|id| id.to_string()),
            context_window_id: reconstruction.window_id.map(|id| id.to_string()),
            checkpoint,
            persisted,
        },
        runtime_build_info,
        config_layer_revision,
        runtime_feature_revision,
        &options,
    ))
}

async fn inspect_loaded_session(
    session: &Arc<Session>,
    options: &ContextInspectionOptions,
) -> CodexResult<ContextInspection> {
    let step_context = session.last_known_step_context().await;
    let (prompt, snapshot_kind, turn_id, dynamic_context_available, context_window_tokens) =
        if let Some(step_context) = step_context.as_ref() {
            let input = session
                .clone_history()
                .await
                .for_prompt(&step_context.settings.model_info.input_modalities);
            let base_instructions = session.get_base_instructions().await;
            let prompt = build_prompt(input, step_context, base_instructions);
            (
                prompt,
                ContextSnapshotKind::Live,
                Some(step_context.turn.sub_id.clone()),
                true,
                step_context.settings.model_info.usable_context_window(),
            )
        } else {
            // An idle session has no request-scoped dynamic snapshot. Use the configured model's
            // modalities to preserve history normalization, but do not call capture_step_context:
            // doing so would refresh contributors and make a read API observable.
            let model_info = session.configured_model_info().await;
            let input = session
                .clone_history()
                .await
                .for_prompt(&model_info.input_modalities);
            let base_instructions = session.get_base_instructions().await;
            let prompt = Prompt {
                input,
                base_instructions,
                ..Prompt::default()
            };
            (
                prompt,
                ContextSnapshotKind::Speculative,
                None,
                false,
                model_info.usable_context_window(),
            )
        };

    let persisted_items = load_persisted_items(session).await;
    let persisted = persisted_items
        .as_deref()
        .map(|items| persisted_metadata(items, session.thread_id()))
        .unwrap_or_default();
    let checkpoint = persisted_items
        .as_deref()
        .map(latest_checkpoint)
        .unwrap_or_default();
    let replacement_items = persisted_items
        .as_deref()
        .map(latest_replacement_items)
        .unwrap_or_default();
    let inherited_rollout_count = if let Some(count) = session.inherited_history_item_count() {
        Some(count)
    } else if let Some(parent_thread_id) = persisted_items
        .as_deref()
        .and_then(persisted_parent_thread_id)
    {
        session
            .services
            .agent_control
            .fork_context_inherited_count(parent_thread_id, session.thread_id())
            .await
    } else {
        None
    };
    let inherited_item_count = inherited_rollout_count.map(|count| {
        persisted_items.as_deref().map_or(count, |items| {
            prompt_item_count_through_rollout_boundary(items, count)
        })
    });
    let usage = session.token_usage_info().await;
    let (window_id, window_number, first_window_id, context_window_id, previous_window_id) =
        session.context_window_snapshot().await;
    let source_available = persisted.available;

    build_inspection(
        session,
        prompt,
        InspectionAssembly {
            snapshot_kind,
            turn_id: turn_id.or_else(|| options.turn_id.clone()),
            dynamic_context_available,
            source_available,
            inherited_item_count,
            replacement_items,
            active_tokens: usage.as_ref().map(active_tokens),
            usage: usage.as_ref().map(|info| info.total_token_usage.clone()),
            context_window_tokens,
            window_id: Some(window_id),
            window_number: Some(window_number),
            first_window_id: Some(first_window_id.to_string()),
            previous_window_id: previous_window_id.map(|id| id.to_string()),
            context_window_id: Some(context_window_id.to_string()),
            checkpoint,
            persisted,
        },
        options,
    )
    .await
}

async fn inspect_cold_session(
    session: &Arc<Session>,
    options: &ContextInspectionOptions,
) -> CodexResult<ContextInspection> {
    let thread_id = session.thread_id();
    let stored = session
        .services
        .thread_store
        .load_latest_model_context(LoadThreadHistoryParams {
            thread_id,
            include_archived: true,
        })
        .await
        .map_err(|error| {
            CodexErr::Fatal(format!(
                "failed to load model context for inspection {thread_id}: {error}"
            ))
        })?;
    let persisted = persisted_metadata(&stored.items, thread_id);
    let checkpoint = latest_checkpoint(&stored.items);
    let replacement_items = latest_replacement_items(&stored.items);
    let inherited_rollout_count =
        if let Some(parent_thread_id) = persisted_parent_thread_id(&stored.items) {
            session
                .services
                .agent_control
                .fork_context_inherited_count(parent_thread_id, thread_id)
                .await
        } else {
            None
        };
    let inherited_item_count = inherited_rollout_count
        .map(|count| prompt_item_count_through_rollout_boundary(&stored.items, count));

    // The existing reconstruction path applies replacement histories, rollback, world-state
    // replay, and legacy compaction handling. Resolve only model metadata here; no turn,
    // contributor refresh, or persistence is created for a read-only cold inspection.
    let model_info = session.configured_model_info().await;
    let reconstruction = session.reconstruct_history_from_rollout_with_policy(
        model_info.truncation_policy.into(),
        &stored.items,
    );
    let mut history = ContextManager::new();
    history.replace_annotated(reconstruction.history);
    let input = history.for_prompt(&model_info.input_modalities);
    let base_instructions = match persisted.base_instructions.clone() {
        Some(base_instructions) => base_instructions,
        None => session.get_base_instructions().await,
    };
    let prompt = Prompt {
        input,
        base_instructions,
        ..Prompt::default()
    };
    let turn_id = options
        .turn_id
        .clone()
        .or_else(|| latest_turn_id(&stored.items));
    let usage = latest_token_usage(&stored.items);
    let window_number = reconstruction.window_number;
    let window_id = reconstruction
        .window_id
        .map(|id| id.to_string())
        .or_else(|| Some(format!("{thread_id}:{window_number}")));

    build_inspection(
        session,
        prompt,
        InspectionAssembly {
            snapshot_kind: ContextSnapshotKind::Cold,
            turn_id,
            dynamic_context_available: false,
            source_available: persisted.available,
            inherited_item_count,
            replacement_items,
            active_tokens: usage.as_ref().map(active_tokens_from_usage),
            usage,
            context_window_tokens: model_info.usable_context_window(),
            window_id,
            window_number: Some(window_number),
            first_window_id: reconstruction.first_window_id.map(|id| id.to_string()),
            previous_window_id: reconstruction.previous_window_id.map(|id| id.to_string()),
            context_window_id: reconstruction.window_id.map(|id| id.to_string()),
            checkpoint,
            persisted,
        },
        options,
    )
    .await
}

/// Internal prompt projection used by the loaded/cold entry points and focused core tests.
/// Keeping this helper on the real `Prompt` type ensures tools and base instructions are counted
/// from the same object passed to the model client.
#[cfg(test)]
pub(crate) fn inspect_prompt_for_tests(
    thread_id: codex_protocol::ThreadId,
    prompt: &Prompt,
    options: &ContextInspectionOptions,
) -> ContextInspection {
    build_inspection_without_session(thread_id, prompt, options)
}

struct InspectionAssembly {
    snapshot_kind: ContextSnapshotKind,
    turn_id: Option<String>,
    dynamic_context_available: bool,
    source_available: bool,
    inherited_item_count: Option<usize>,
    replacement_items: Vec<ResponseItem>,
    active_tokens: Option<i64>,
    usage: Option<TokenUsage>,
    context_window_tokens: Option<i64>,
    window_id: Option<String>,
    window_number: Option<u64>,
    first_window_id: Option<String>,
    previous_window_id: Option<String>,
    context_window_id: Option<String>,
    checkpoint: CheckpointMetadata,
    persisted: PersistedMetadata,
}

async fn build_inspection(
    session: &Session,
    prompt: Prompt,
    assembly: InspectionAssembly,
    options: &ContextInspectionOptions,
) -> CodexResult<ContextInspection> {
    let config = session.get_config().await;
    let current_build = Some(RuntimeBuildInfo::current());
    let current_config_revision = Some(config.config_layer_stack.revision());
    let current_runtime_feature_revision =
        Some(config.config_layer_stack.runtime_feature_revision());
    let inspection = build_inspection_with_metadata(
        session.thread_id(),
        &prompt,
        &assembly,
        current_build,
        current_config_revision,
        current_runtime_feature_revision,
        options,
    );
    Ok(inspection)
}

#[cfg(test)]
fn build_inspection_without_session(
    thread_id: codex_protocol::ThreadId,
    prompt: &Prompt,
    options: &ContextInspectionOptions,
) -> ContextInspection {
    let assembly = InspectionAssembly {
        snapshot_kind: ContextSnapshotKind::Speculative,
        turn_id: options.turn_id.clone(),
        dynamic_context_available: !prompt.tools.is_empty(),
        source_available: true,
        inherited_item_count: None,
        replacement_items: Vec::new(),
        active_tokens: None,
        usage: None,
        context_window_tokens: None,
        window_id: None,
        window_number: None,
        first_window_id: None,
        previous_window_id: None,
        context_window_id: None,
        checkpoint: CheckpointMetadata::default(),
        persisted: PersistedMetadata::default(),
    };
    build_inspection_with_metadata(thread_id, prompt, &assembly, None, None, None, options)
}

fn build_inspection_with_metadata(
    thread_id: codex_protocol::ThreadId,
    prompt: &Prompt,
    assembly: &InspectionAssembly,
    runtime_build_info: Option<RuntimeBuildInfo>,
    config_layer_revision: Option<String>,
    runtime_feature_revision: Option<String>,
    options: &ContextInspectionOptions,
) -> ContextInspection {
    let mut preview_budget = PreviewBudget::new(options.include_preview);
    let base_instructions =
        inspect_base_instructions(&prompt.base_instructions, &mut preview_budget);
    let tools = inspect_tools(prompt, &mut preview_budget);
    let items = inspect_items(
        &prompt.input,
        assembly.inherited_item_count,
        &assembly.replacement_items,
        assembly.turn_id.as_deref(),
        &mut preview_budget,
    );
    let estimated_prompt_tokens = Some(
        base_instructions
            .estimated_tokens
            .saturating_add(tools.estimated_tokens)
            .saturating_add(
                items
                    .iter()
                    .map(|item| item.estimated_tokens)
                    .fold(0_i64, i64::saturating_add),
            ),
    );
    let usage = assembly
        .usage
        .as_ref()
        .map(cache_metrics_from_usage)
        .unwrap_or_default();
    let persisted = &assembly.persisted;
    let stale = provenance_is_stale(
        runtime_build_info.as_ref(),
        config_layer_revision.as_deref(),
        runtime_feature_revision.as_deref(),
        persisted,
    );
    let partial = !assembly.source_available || !assembly.dynamic_context_available;

    ContextInspection {
        thread_id,
        turn_id: assembly.turn_id.clone(),
        snapshot_kind: assembly.snapshot_kind,
        partial,
        item_count: items.len(),
        estimated_prompt_tokens,
        estimated_active_tokens: assembly.active_tokens.or(estimated_prompt_tokens),
        estimated_context_window_tokens: assembly.context_window_tokens,
        cached_input_tokens: usage.cached_input_tokens,
        uncached_input_tokens: usage.uncached_input_tokens,
        cache_write_input_tokens: usage.cache_write_input_tokens,
        runtime_build_info,
        config_layer_revision,
        runtime_feature_revision,
        persisted_runtime_build_info: persisted.runtime_build_info.clone(),
        persisted_config_layer_revision: persisted.config_layer_revision.clone(),
        persisted_runtime_feature_revision: persisted.runtime_feature_revision.clone(),
        stale,
        window_id: assembly.window_id.clone(),
        context_window_id: assembly.context_window_id.clone(),
        window_number: assembly.window_number,
        first_window_id: assembly.first_window_id.clone(),
        previous_window_id: assembly.previous_window_id.clone(),
        checkpoint_id: assembly.checkpoint.id.clone(),
        checkpoint_revision: assembly.checkpoint.revision,
        base_instructions,
        tools,
        items,
    }
}

#[cfg(test)]
#[path = "context_inspection_tests.rs"]
mod tests;
