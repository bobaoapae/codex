//! Server collaboration-mode discovery and TUI-visible selection. Missing discovery adds no presets.

use codex_app_server_client::AppServerRequestHandle;
use codex_app_server_protocol::ClientRequest;
use codex_app_server_protocol::CollaborationModeListParams;
use codex_app_server_protocol::CollaborationModeListResponse;
use codex_app_server_protocol::RequestId;
use codex_models_manager::collaboration_mode_presets::plan_mode_instructions;
use codex_protocol::config_types::CollaborationModeMask;
use codex_protocol::config_types::ModeKind;
use std::path::Path;
use std::time::Duration;

use crate::model_catalog::ModelCatalog;

/// FORK: replace the built-in Plan-mode developer instructions with
/// `$CODEX_HOME/plan_mode.md` when that override file exists.
///
/// Applied where the active mask is installed rather than where the presets are built, so the
/// preset list itself stays a pure function of what discovery returned.
pub(crate) fn apply_plan_instructions_override(
    mask: &mut CollaborationModeMask,
    codex_home: &Path,
) {
    if mask.mode != Some(ModeKind::Plan) {
        return;
    }
    // Only rewrite instructions the mask already carries: a sparse mask (`None`) means "leave the
    // current instructions alone", and a cleared one (`Some(None)`) means "no instructions".
    if !matches!(mask.developer_instructions, Some(Some(_))) {
        return;
    }
    let instructions = plan_mode_instructions(Some(codex_home));
    if !instructions.is_empty() {
        mask.developer_instructions = Some(Some(instructions));
    }
}

fn filtered_presets(model_catalog: &ModelCatalog) -> Vec<CollaborationModeMask> {
    model_catalog
        .collaboration_modes
        .iter()
        .filter(|mask| mask.mode.is_some_and(ModeKind::is_tui_visible))
        .cloned()
        .collect()
}

pub(crate) fn default_mask(model_catalog: &ModelCatalog) -> Option<CollaborationModeMask> {
    let presets = filtered_presets(model_catalog);
    presets
        .iter()
        .find(|mask| mask.mode == Some(ModeKind::Default))
        .cloned()
        .or_else(|| presets.into_iter().next())
}

pub(crate) fn mask_for_kind(
    model_catalog: &ModelCatalog,
    kind: ModeKind,
) -> Option<CollaborationModeMask> {
    if !kind.is_tui_visible() {
        return None;
    }
    filtered_presets(model_catalog)
        .into_iter()
        .find(|mask| mask.mode == Some(kind))
}

/// Cycle to the next collaboration mode preset in list order.
pub(crate) fn next_mask(
    model_catalog: &ModelCatalog,
    current: Option<&CollaborationModeMask>,
) -> Option<CollaborationModeMask> {
    let presets = filtered_presets(model_catalog);
    if presets.is_empty() {
        return None;
    }
    let current_kind = current.and_then(|mask| mask.mode);
    let next_index = presets
        .iter()
        .position(|mask| mask.mode == current_kind)
        .map_or(0, |idx| (idx + 1) % presets.len());
    presets.get(next_index).cloned()
}

pub(crate) fn default_mode_mask(model_catalog: &ModelCatalog) -> Option<CollaborationModeMask> {
    mask_for_kind(model_catalog, ModeKind::Default)
}

pub(crate) fn plan_mask(model_catalog: &ModelCatalog) -> Option<CollaborationModeMask> {
    mask_for_kind(model_catalog, ModeKind::Plan)
}

/// Discovery is optional even on servers that accept the rest of TUI bootstrap.
pub(crate) async fn list(request_handle: AppServerRequestHandle) -> Vec<CollaborationModeMask> {
    let response = tokio::time::timeout(
        Duration::from_secs(/*secs*/ 2),
        request_handle.request_typed::<CollaborationModeListResponse>(
            ClientRequest::CollaborationModeList {
                request_id: RequestId::String(format!(
                    "collaboration-mode-list-{}",
                    uuid::Uuid::new_v4()
                )),
                params: CollaborationModeListParams::default(),
            },
        ),
    )
    .await;
    match response {
        Ok(Ok(response)) => response
            .data
            .into_iter()
            .map(|mask| CollaborationModeMask {
                name: mask.name,
                mode: mask.mode,
                model: mask.model,
                reasoning_effort: mask.reasoning_effort,
                // The RPC intentionally omits prompts. Clear restored overrides so the server
                // supplies its current instructions when this mode is selected.
                developer_instructions: Some(None),
            })
            .collect(),
        _ => {
            tracing::warn!("optional collaborationMode/list discovery unavailable");
            Vec::new()
        }
    }
}
