use crate::agent::control::SpawnAgentForkMode;
use crate::agent::role::apply_role_to_config;
use crate::config::Config;
use crate::config::DEFAULT_MULTI_AGENT_V2_DEFAULT_WAIT_TIMEOUT_MS;
use crate::config::DEFAULT_MULTI_AGENT_V2_MIN_WAIT_TIMEOUT_MS;
use crate::config::HARD_MAX_MULTI_AGENT_V2_TIMEOUT_MS;
use crate::function_tool::FunctionCallError;
use crate::session::session::Session;
use crate::session::turn_context::TurnContext;
use crate::tools::context::FunctionToolOutput;
use crate::tools::context::ToolOutput;
use crate::tools::context::ToolPayload;
use codex_model_provider_info::CHATGPT_WEB_PROVIDER_ID;
use codex_model_provider_info::CLAUDE_CODE_PROVIDER_ID;
use codex_model_provider_info::WireApi;
use codex_models_manager::manager::RefreshStrategy;
use codex_protocol::AgentPath;
use codex_protocol::ThreadId;
use codex_protocol::config_types::SERVICE_TIER_DEFAULT_REQUEST_VALUE;
use codex_protocol::error::CodexErr;
use codex_protocol::error::CodexErrorDetails;
use codex_protocol::models::BaseInstructions;
use codex_protocol::models::ResponseInputItem;
use codex_protocol::openai_models::ModelPreset;
use codex_protocol::openai_models::ReasoningEffort;
use codex_protocol::openai_models::ReasoningEffortPreset;
use codex_protocol::protocol::MultiAgentVersion;
use codex_protocol::protocol::SessionSource;
use codex_protocol::protocol::SubAgentSource;
use codex_protocol::user_input::UserInput;
use serde::Serialize;
use serde_json::Value as JsonValue;

/// Minimum wait timeout to prevent tight polling loops from burning CPU.
pub(crate) const MIN_WAIT_TIMEOUT_MS: i64 = DEFAULT_MULTI_AGENT_V2_MIN_WAIT_TIMEOUT_MS;
pub(crate) const DEFAULT_WAIT_TIMEOUT_MS: i64 = DEFAULT_MULTI_AGENT_V2_DEFAULT_WAIT_TIMEOUT_MS;
pub(crate) const MAX_WAIT_TIMEOUT_MS: i64 = HARD_MAX_MULTI_AGENT_V2_TIMEOUT_MS;
pub(crate) const MAX_SPAWN_AGENT_MODEL_OVERRIDES: usize = 5;

/// Native Claude children receive their task through the inter-agent brief and must not inherit
/// the parent's Codex conversation. Other child providers retain the requested fork semantics.
///
/// FORK: returns the note explaining an adjusted `fork_turns` instead of
/// silently ignoring it, so the caller can hand the model one honest sentence
/// rather than letting it believe the child inherited history it never saw.
///
/// `max_fork_turns` (`[claude_code] max_fork_turns` or
/// `[chatgpt_web] max_fork_turns`, default 0) is the ceiling on how much of the
/// parent conversation a locally served child may inherit. A full-history fork
/// is refused at any setting: that mode also inherits the parent's agent type,
/// which a locally served child cannot honor.
pub(crate) fn task_fork_mode_for_wire_api(
    wire_api: WireApi,
    requested_fork_mode: Option<SpawnAgentForkMode>,
    max_fork_turns: usize,
) -> (Option<SpawnAgentForkMode>, Option<String>) {
    // FORK: `chatgpt_web` children are briefed exactly like Claude children.
    let Some(config_table) = locally_served_config_table(wire_api) else {
        return (requested_fork_mode, None);
    };
    match requested_fork_mode {
        None => (None, None),
        Some(SpawnAgentForkMode::FullHistory) => (None, Some(LOCAL_FULL_FORK_NOTE.to_string())),
        Some(SpawnAgentForkMode::LastNTurns(_)) if max_fork_turns == 0 => {
            (None, Some(LOCAL_TASK_ONLY_FORK_NOTE.to_string()))
        }
        Some(SpawnAgentForkMode::LastNTurns(requested)) => {
            let allowed = requested.min(max_fork_turns);
            let note = (allowed != requested).then(|| {
                format!(
                    "`fork_turns` was reduced from {requested} to {allowed} by `[{config_table}] max_fork_turns`."
                )
            });
            (Some(SpawnAgentForkMode::LastNTurns(allowed)), note)
        }
    }
}

/// FORK: the `config.toml` table of a locally served wire API, or `None` for a
/// provider with a real backend.
fn locally_served_config_table(wire_api: WireApi) -> Option<&'static str> {
    match wire_api {
        WireApi::ClaudeCode => Some("claude_code"),
        WireApi::ChatGptWeb => Some("chatgpt_web"),
        WireApi::Responses => None,
    }
}

/// FORK: the `max_fork_turns` ceiling that applies to a child on `wire_api`.
pub(crate) fn max_fork_turns_for_wire_api(config: &Config, wire_api: WireApi) -> usize {
    match wire_api {
        WireApi::ClaudeCode => config.claude_code_max_fork_turns,
        WireApi::ChatGptWeb => config.chatgpt_web.max_fork_turns,
        WireApi::Responses => 0,
    }
}

/// FORK: whether a child on this provider is served by a local process (the
/// Claude Code CLI or the ChatGPT web app) rather than a model endpoint.
pub(crate) fn is_locally_served(config: &Config) -> bool {
    matches!(
        config.model_provider.wire_api,
        WireApi::ClaudeCode | WireApi::ChatGptWeb
    ) || config.model_provider_id == CLAUDE_CODE_PROVIDER_ID
        || config.model_provider_id == CHATGPT_WEB_PROVIDER_ID
}

/// Said when a locally served child was asked to inherit turns it is not
/// allowed to.
const LOCAL_TASK_ONLY_FORK_NOTE: &str = "`fork_turns` was ignored: this locally served agent starts from task-only context, so the brief must be self-contained.";

/// Said when a locally served child was asked for a full-history fork, which
/// also inherits the parent agent type and so can never apply.
const LOCAL_FULL_FORK_NOTE: &str = "`fork_turns: \"all\"` is not available for a locally served agent; it started from task-only context. Send a self-contained brief.";

pub(crate) fn model_supports_multi_agent_backend(
    model: &ModelPreset,
    multi_agent_version: MultiAgentVersion,
) -> bool {
    multi_agent_version != MultiAgentVersion::V2
        || model.multi_agent_version != Some(MultiAgentVersion::Disabled)
}

pub(crate) fn function_arguments(payload: ToolPayload) -> Result<String, FunctionCallError> {
    match payload {
        ToolPayload::Function { arguments } => Ok(arguments),
        _ => Err(FunctionCallError::RespondToModel(
            "collab handler received unsupported payload".to_string(),
        )),
    }
}

pub(crate) fn tool_output_json_text<T>(value: &T, tool_name: &str) -> String
where
    T: Serialize,
{
    serde_json::to_string(value).unwrap_or_else(|err| {
        JsonValue::String(format!("failed to serialize {tool_name} result: {err}")).to_string()
    })
}

pub(crate) fn tool_output_response_item<T>(
    call_id: &str,
    payload: &ToolPayload,
    value: &T,
    success: Option<bool>,
    tool_name: &str,
) -> ResponseInputItem
where
    T: Serialize,
{
    FunctionToolOutput::from_text(tool_output_json_text(value, tool_name), success)
        .to_response_item(call_id, payload)
}

pub(crate) fn tool_output_code_mode_result<T>(value: &T, tool_name: &str) -> JsonValue
where
    T: Serialize,
{
    serde_json::to_value(value).unwrap_or_else(|err| {
        JsonValue::String(format!("failed to serialize {tool_name} result: {err}"))
    })
}

pub(crate) fn collab_spawn_error(err: CodexErr) -> FunctionCallError {
    match err.details() {
        CodexErrorDetails::UnsupportedOperation(message) if message == "thread manager dropped" => {
            FunctionCallError::RespondToModel("collab manager unavailable".to_string())
        }
        CodexErrorDetails::UnsupportedOperation(message) => {
            FunctionCallError::RespondToModel(message.clone())
        }
        _ => FunctionCallError::RespondToModel(format!("collab spawn failed: {err}")),
    }
}

pub(crate) fn collab_agent_error(agent_id: ThreadId, err: CodexErr) -> FunctionCallError {
    match err.details() {
        CodexErrorDetails::ThreadNotFound(id) => {
            FunctionCallError::RespondToModel(format!("agent with id {id} not found"))
        }
        CodexErrorDetails::InternalAgentDied => {
            FunctionCallError::RespondToModel(format!("agent with id {agent_id} is closed"))
        }
        CodexErrorDetails::UnsupportedOperation(_) => {
            FunctionCallError::RespondToModel("collab manager unavailable".to_string())
        }
        _ => FunctionCallError::RespondToModel(format!("collab tool failed: {err}")),
    }
}

pub(crate) fn thread_spawn_source(
    parent_thread_id: ThreadId,
    parent_session_source: &SessionSource,
    depth: i32,
    agent_role: Option<&str>,
    task_name: Option<String>,
) -> Result<SessionSource, FunctionCallError> {
    let agent_path = task_name
        .as_deref()
        .map(|task_name| {
            parent_session_source
                .get_agent_path()
                .unwrap_or_else(AgentPath::root)
                .join(task_name)
                .map_err(FunctionCallError::RespondToModel)
        })
        .transpose()?;
    Ok(SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
        parent_thread_id,
        depth,
        agent_path,
        agent_nickname: None,
        agent_role: agent_role.map(str::to_string),
    }))
}

pub(crate) fn parse_collab_input(
    message: Option<String>,
    items: Option<Vec<UserInput>>,
) -> Result<Vec<UserInput>, FunctionCallError> {
    match (message, items) {
        (Some(_), Some(_)) => Err(FunctionCallError::RespondToModel(
            "Provide either message or items, but not both".to_string(),
        )),
        (None, None) => Err(FunctionCallError::RespondToModel(
            "Provide one of: message or items".to_string(),
        )),
        (Some(message), None) => {
            if message.trim().is_empty() {
                return Err(FunctionCallError::RespondToModel(
                    "Empty message can't be sent to an agent".to_string(),
                ));
            }
            Ok(vec![UserInput::Text {
                text: message,
                text_elements: Vec::new(),
            }])
        }
        (None, Some(items)) => {
            if items.is_empty() {
                return Err(FunctionCallError::RespondToModel(
                    "Items can't be empty".to_string(),
                ));
            }
            Ok(items)
        }
    }
}

/// Builds the base config snapshot for a newly spawned sub-agent.
///
/// The returned config starts from the parent's effective config and then refreshes the
/// runtime-owned fields carried by the turn, including model selection, reasoning settings,
/// approval policy, sandbox, and cwd. Role-specific overrides are layered
/// after this step; skipping this helper and cloning stale config state directly can send the child
/// agent out with the wrong provider or runtime policy.
pub(crate) fn build_agent_spawn_config(
    base_instructions: &BaseInstructions,
    turn: &TurnContext,
) -> Result<Config, FunctionCallError> {
    let mut config = build_agent_shared_config(turn)?;
    config.base_instructions = Some(base_instructions.text.clone());
    config.base_instructions_provenance = base_instructions.provenance.clone();
    Ok(config)
}

pub(crate) fn build_agent_resume_config(turn: &TurnContext) -> Result<Config, FunctionCallError> {
    let mut config = build_agent_shared_config(turn)?;
    // For resume, keep base instructions sourced from rollout/session metadata.
    config.base_instructions = None;
    config.base_instructions_provenance = None;
    Ok(config)
}

fn build_agent_shared_config(turn: &TurnContext) -> Result<Config, FunctionCallError> {
    let base_config = turn.config.clone();
    let mut config = (*base_config).clone();
    config.model = Some(turn.model_info().slug.clone());
    config.model_provider = turn.provider.info().clone();
    config.model_reasoning_effort = turn
        .reasoning_effort()
        .or(turn.model_info().default_reasoning_level.as_ref())
        .cloned();
    config.model_reasoning_summary = Some(turn.reasoning_summary());
    config.developer_instructions = turn.developer_instructions.clone();
    if turn.multi_agent_version == MultiAgentVersion::V2
        && let Some(developer_instructions) = turn
            .config
            .multi_agent_v2
            .subagent_developer_instructions
            .clone()
    {
        config.developer_instructions = Some(developer_instructions);
    }
    apply_spawn_agent_runtime_overrides(&mut config, turn)?;

    Ok(config)
}

pub(crate) fn reject_full_fork_agent_type_override(
    agent_type: Option<&str>,
) -> Result<(), FunctionCallError> {
    if agent_type.is_some() {
        return Err(FunctionCallError::RespondToModel(
            "Full-history forked agents inherit the parent agent type; omit agent_type, or spawn without a full-history fork.".to_string(),
        ));
    }
    Ok(())
}

/// Copies runtime-only turn state onto a child config before it is handed to `AgentControl`.
///
/// These values are chosen by the live turn rather than persisted config, so leaving them stale can
/// make a child agent disagree with its parent about approval policy, cwd, or sandboxing.
pub(crate) fn apply_spawn_agent_runtime_overrides(
    config: &mut Config,
    turn: &TurnContext,
) -> Result<(), FunctionCallError> {
    config
        .permissions
        .approval_policy
        .set(turn.approval_policy())
        .map_err(|err| {
            FunctionCallError::RespondToModel(format!("approval_policy is invalid: {err}"))
        })?;
    config.approvals_reviewer = turn.config.approvals_reviewer;
    #[allow(deprecated)]
    let turn_cwd = turn.cwd.clone();
    config.cwd = turn_cwd;
    config
        .permissions
        .set_permission_profile_from_session_snapshot(
            turn.config
                .permissions
                .permission_profile_state()
                .snapshot(),
        )
        .map_err(|err| {
            FunctionCallError::RespondToModel(format!("permission_profile is invalid: {err}"))
        })?;
    align_provider_with_locally_served_model(config)?;
    Ok(())
}

/// FORK: pins a Claude-backed child to one configured account.
///
/// The pin is tried first and still fails over, so naming a spent account costs
/// one fast failure rather than stranding the agent. Naming an account for a
/// child that is not Claude-backed is an error: silently ignoring it would let
/// the parent believe it had spread a fan-out across accounts.
pub(crate) fn apply_spawn_agent_claude_account(
    config: &mut Config,
    requested_account: Option<&str>,
) -> Result<(), FunctionCallError> {
    let Some(requested_account) = requested_account else {
        return Ok(());
    };
    if config.model_provider_id != CLAUDE_CODE_PROVIDER_ID {
        return Err(FunctionCallError::RespondToModel(format!(
            "`account` only applies to a Claude-backed agent; this one runs on `{}`. Spawn it with a Claude agent type, or omit `account`.",
            config.model_provider_id
        )));
    }
    match crate::claude_code::resolve_account_alias(
        &config.claude_code_account_dirs,
        requested_account,
    )
    .map_err(FunctionCallError::RespondToModel)?
    {
        crate::claude_code::AccountAlias::Auto => config.claude_code_account_override = None,
        crate::claude_code::AccountAlias::Dir(dir) => {
            config.claude_code_account_override = Some(dir);
        }
    }
    Ok(())
}

/// FORK: a locally served model (the Claude Code CLI, the ChatGPT web app) is
/// only reachable through the provider that serves it.
///
/// A role names the provider itself, but `spawn_agent(model = "claude-opus-5")`
/// does not: the slug is accepted by the catalog and the child would inherit the
/// parent's provider, which answers such a request with a flat rejection. Align
/// the two here, after every other override has been applied.
fn align_provider_with_locally_served_model(config: &mut Config) -> Result<(), FunctionCallError> {
    let Some(model) = config.model.as_deref() else {
        return Ok(());
    };
    let Some(provider_id) =
        codex_models_manager::local_models::provider_for_locally_served_model(model)
    else {
        return Ok(());
    };
    if config.model_provider_id == provider_id {
        return Ok(());
    }
    let provider = config
        .model_providers
        .get(provider_id)
        .cloned()
        .ok_or_else(|| {
            FunctionCallError::RespondToModel(format!(
                "model `{model}` is served by the local `{provider_id}` provider, which is not configured"
            ))
        })?;
    config.model_provider = provider;
    config.model_provider_id = provider_id.to_string();
    Ok(())
}

pub(crate) async fn apply_requested_spawn_agent_model_overrides(
    session: &Session,
    turn: &TurnContext,
    config: &mut Config,
    requested_model: Option<&str>,
    requested_reasoning_effort: Option<ReasoningEffort>,
    // FORK: notes accumulate the arguments we adjusted instead of rejecting.
    notes: &mut Vec<String>,
) -> Result<(), FunctionCallError> {
    let requested_model = requested_model.or(turn.config.agent_default_subagent_model.as_deref());
    let requested_reasoning_effort = requested_reasoning_effort
        .or_else(|| turn.config.agent_default_subagent_reasoning_effort.clone());
    if requested_model.is_none() && requested_reasoning_effort.is_none() {
        return Ok(());
    }

    if let Some(requested_model) = requested_model {
        let available_models = session
            .services
            .models_manager
            .list_models(RefreshStrategy::Offline, config.http_client_factory())
            .await;
        let selected_model_name = find_spawn_agent_model_name(
            &available_models,
            requested_model,
            turn.multi_agent_version,
        )?;
        let selected_model_info = session
            .services
            .models_manager
            .get_model_info(&selected_model_name, &config.to_models_manager_config())
            .await;

        config.model = Some(selected_model_name.clone());
        if let Some(reasoning_effort) = requested_reasoning_effort {
            let (effort, note) = clamp_spawn_agent_reasoning_effort(
                &selected_model_name,
                &selected_model_info.supported_reasoning_levels,
                &reasoning_effort,
            );
            notes.extend(note);
            config.model_reasoning_effort = effort.or(selected_model_info.default_reasoning_level);
        } else {
            config.model_reasoning_effort = selected_model_info.default_reasoning_level;
        }

        return Ok(());
    }

    if let Some(reasoning_effort) = requested_reasoning_effort {
        let (effort, note) = clamp_spawn_agent_reasoning_effort(
            &turn.model_info().slug,
            &turn.model_info().supported_reasoning_levels,
            &reasoning_effort,
        );
        notes.extend(note);
        if let Some(effort) = effort {
            config.model_reasoning_effort = Some(effort);
        }
    }

    Ok(())
}

pub(crate) async fn apply_spawn_agent_service_tier(
    session: &Session,
    config: &mut Config,
) -> Result<(), FunctionCallError> {
    // FORK: a locally served child has no backend service tiers at all, and the
    // root tier is now inherited unconditionally (including the `default`
    // early-return below, which skips the model capability check), so strip it
    // before any of that can push a tier onto a local provider request.
    if is_locally_served(config) {
        config.service_tier = None;
        return Ok(());
    }

    let Some(service_tier) = session.services.agent_control.root_service_tier() else {
        config.service_tier = None;
        return Ok(());
    };
    if service_tier == SERVICE_TIER_DEFAULT_REQUEST_VALUE {
        config.service_tier = Some(service_tier);
        return Ok(());
    }

    let model = config.model.clone().ok_or_else(|| {
        FunctionCallError::RespondToModel(
            "spawn_agent could not resolve the child model for service tier validation".to_string(),
        )
    })?;
    let model_info = session
        .services
        .models_manager
        .get_model_info(model.as_str(), &config.to_models_manager_config())
        .await;

    config.service_tier = model_info
        .supports_service_tier(service_tier.as_str())
        .then_some(service_tier);
    Ok(())
}

pub(crate) async fn apply_spawn_agent_role(
    session: &Session,
    config: &mut Config,
    role_name: Option<&str>,
    // FORK: notes accumulate the arguments we adjusted instead of rejecting.
    notes: &mut Vec<String>,
) -> Result<(), FunctionCallError> {
    let previous_model = config.model.clone();
    let previous_reasoning_effort = config.model_reasoning_effort.clone();
    apply_role_to_config(config, role_name)
        .await
        .map_err(FunctionCallError::RespondToModel)?;
    if config.model == previous_model && config.model_reasoning_effort == previous_reasoning_effort
    {
        return Ok(());
    }

    let Some(reasoning_effort) = config.model_reasoning_effort.clone() else {
        return Ok(());
    };
    let model = config.model.clone().ok_or_else(|| {
        FunctionCallError::RespondToModel(
            "spawn_agent could not resolve the child model for reasoning effort validation"
                .to_string(),
        )
    })?;
    let model_info = session
        .services
        .models_manager
        .get_model_info(&model, &config.to_models_manager_config())
        .await;
    // Fallback metadata does not describe the real model, so clamping against it
    // would silently downgrade a role that is in fact supported.
    if model_info.used_fallback_model_metadata {
        return Ok(());
    }

    let (effort, note) = clamp_spawn_agent_reasoning_effort(
        &model,
        &model_info.supported_reasoning_levels,
        &reasoning_effort,
    );
    notes.extend(note);
    config.model_reasoning_effort = effort.or(model_info.default_reasoning_level);
    Ok(())
}

fn find_spawn_agent_model_name(
    available_models: &[ModelPreset],
    requested_model: &str,
    multi_agent_version: MultiAgentVersion,
) -> Result<String, FunctionCallError> {
    available_models
        .iter()
        .find(|model| {
            model.model == requested_model
                && model_supports_multi_agent_backend(model, multi_agent_version)
        })
        .map(|model| model.model.clone())
        .ok_or_else(|| {
            let available = available_models
                .iter()
                .filter(|model| model.show_in_picker)
                .filter(|model| model_supports_multi_agent_backend(model, multi_agent_version))
                .take(MAX_SPAWN_AGENT_MODEL_OVERRIDES)
                .map(|model| model.model.as_str())
                .collect::<Vec<_>>()
                .join(", ");
            FunctionCallError::RespondToModel(format!(
                "Unknown model `{requested_model}` for spawn_agent. Available models: {available}"
            ))
        })
}

/// Rank used to clamp an unsupported reasoning effort down to the closest
/// supported level. `Custom` has no position on the scale, so it maps to the
/// model default instead of to a neighbour.
fn reasoning_effort_rank(effort: &ReasoningEffort) -> Option<u8> {
    match effort {
        ReasoningEffort::None => Some(0),
        ReasoningEffort::Minimal => Some(1),
        ReasoningEffort::Low => Some(2),
        ReasoningEffort::Medium => Some(3),
        ReasoningEffort::High => Some(4),
        ReasoningEffort::XHigh => Some(5),
        ReasoningEffort::Max => Some(6),
        ReasoningEffort::Ultra => Some(7),
        // FORK: `Persistent` is a proactivity mode, not a depth; like `Custom`
        // it has no position on the scale.
        ReasoningEffort::Custom(_) | ReasoningEffort::Persistent => None,
    }
}

/// FORK: an effort the child model does not have is a bad *spawn argument*, not
/// a reason to refuse the whole spawn. Requesting `ultra` for a Claude child
/// used to fail the call outright; now it lands on the highest level the child
/// actually supports and the caller gets a note saying so.
///
/// Clamping is one-directional: never above what was asked for. An empty
/// support list (or an unrankable `Custom`) leaves the model default alone.
fn clamp_spawn_agent_reasoning_effort(
    model: &str,
    supported_reasoning_levels: &[ReasoningEffortPreset],
    requested_reasoning_effort: &ReasoningEffort,
) -> (Option<ReasoningEffort>, Option<String>) {
    if supported_reasoning_levels
        .iter()
        .any(|preset| &preset.effort == requested_reasoning_effort)
    {
        return (Some(requested_reasoning_effort.clone()), None);
    }
    if supported_reasoning_levels.is_empty() {
        return (
            None,
            Some(format!(
                "Reasoning effort `{requested_reasoning_effort}` was dropped: model `{model}` declares no reasoning levels."
            )),
        );
    }

    let clamped = reasoning_effort_rank(requested_reasoning_effort).and_then(|requested_rank| {
        supported_reasoning_levels
            .iter()
            .filter_map(|preset| {
                reasoning_effort_rank(&preset.effort)
                    .filter(|rank| *rank <= requested_rank)
                    .map(|rank| (rank, preset.effort.clone()))
            })
            .max_by_key(|(rank, _)| *rank)
            .map(|(_, effort)| effort)
    });

    let supported = supported_reasoning_levels
        .iter()
        .map(|preset| preset.effort.to_string())
        .collect::<Vec<_>>()
        .join(", ");
    match clamped {
        Some(effort) => (
            Some(effort.clone()),
            Some(format!(
                "Reasoning effort `{requested_reasoning_effort}` is not supported for model `{model}`; used `{effort}` instead. Supported: {supported}."
            )),
        ),
        None => (
            None,
            Some(format!(
                "Reasoning effort `{requested_reasoning_effort}` is not supported for model `{model}`; used the model default. Supported: {supported}."
            )),
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use codex_features::Feature;
    use codex_login::CodexAuth;
    use codex_protocol::config_types::MultiAgentMode;
    use std::path::PathBuf;

    #[test]
    fn fork_invariant_claude_children_use_task_only_fork_rules() {
        assert_eq!(
            task_fork_mode_for_wire_api(
                WireApi::ClaudeCode,
                Some(SpawnAgentForkMode::LastNTurns(10)),
                /*max_fork_turns*/ 0,
            )
            .0,
            None
        );
        // A full-history fork is refused even when turns are allowed: it also
        // inherits the parent agent type, which a Claude child cannot be.
        assert_eq!(
            task_fork_mode_for_wire_api(
                WireApi::ClaudeCode,
                Some(SpawnAgentForkMode::FullHistory),
                /*max_fork_turns*/ 8,
            )
            .0,
            None
        );
    }

    /// FORK: `[claude_code] max_fork_turns` is a ceiling, and a request above it
    /// is clamped rather than refused.
    #[test]
    fn claude_fork_turns_are_capped_by_config() {
        assert_eq!(
            task_fork_mode_for_wire_api(
                WireApi::ClaudeCode,
                Some(SpawnAgentForkMode::LastNTurns(10)),
                /*max_fork_turns*/ 3,
            ),
            (
                Some(SpawnAgentForkMode::LastNTurns(3)),
                Some(
                    "`fork_turns` was reduced from 10 to 3 by `[claude_code] max_fork_turns`."
                        .to_string()
                )
            )
        );
        // Under the cap nothing is adjusted, so nothing is said.
        assert_eq!(
            task_fork_mode_for_wire_api(
                WireApi::ClaudeCode,
                Some(SpawnAgentForkMode::LastNTurns(2)),
                /*max_fork_turns*/ 3,
            ),
            (Some(SpawnAgentForkMode::LastNTurns(2)), None)
        );
    }

    #[test]
    fn non_claude_fork_keeps_requested_history_mode() {
        assert_eq!(
            task_fork_mode_for_wire_api(
                WireApi::Responses,
                Some(SpawnAgentForkMode::LastNTurns(10)),
                /*max_fork_turns*/ 0,
            ),
            (Some(SpawnAgentForkMode::LastNTurns(10)), None)
        );
    }

    /// FORK: a dropped `fork_turns` has to be said out loud, or the parent keeps
    /// writing briefs that assume the child saw the conversation.
    #[test]
    fn spawning_a_claude_agent_with_fork_turns_returns_a_note() {
        let (fork_mode, note) = task_fork_mode_for_wire_api(
            WireApi::ClaudeCode,
            Some(SpawnAgentForkMode::LastNTurns(3)),
            /*max_fork_turns*/ 0,
        );
        assert_eq!(fork_mode, None);
        let note = note.expect("dropping fork_turns must be reported");
        assert!(note.contains("fork_turns"), "{note}");
        assert!(note.contains("self-contained"), "{note}");

        // Nothing was requested, so there is nothing to explain.
        assert_eq!(
            task_fork_mode_for_wire_api(WireApi::ClaudeCode, None, 0),
            (None, None)
        );
    }

    fn effort_presets(efforts: &[ReasoningEffort]) -> Vec<ReasoningEffortPreset> {
        efforts
            .iter()
            .map(|effort| ReasoningEffortPreset {
                effort: effort.clone(),
                description: String::new(),
            })
            .collect()
    }

    /// FORK: `ultra` is this user's configured default; a child that cannot go
    /// that high should run at its ceiling, not fail to spawn.
    #[test]
    fn an_unsupported_reasoning_effort_clamps_down_not_up() {
        let supported = effort_presets(&[
            ReasoningEffort::Low,
            ReasoningEffort::Medium,
            ReasoningEffort::High,
        ]);

        let (effort, note) = clamp_spawn_agent_reasoning_effort(
            "claude-opus-5",
            &supported,
            &ReasoningEffort::Ultra,
        );
        assert_eq!(effort, Some(ReasoningEffort::High));
        let note = note.expect("a clamped effort must be reported");
        assert!(note.contains("`ultra`"), "{note}");
        assert!(note.contains("`high`"), "{note}");

        // Never upgrade: asking for less than the floor keeps the model default.
        let (effort, note) = clamp_spawn_agent_reasoning_effort(
            "claude-opus-5",
            &supported,
            &ReasoningEffort::Minimal,
        );
        assert_eq!(effort, None);
        assert!(note.is_some());

        // A supported level passes through untouched and says nothing.
        assert_eq!(
            clamp_spawn_agent_reasoning_effort(
                "claude-opus-5",
                &supported,
                &ReasoningEffort::Medium
            ),
            (Some(ReasoningEffort::Medium), None)
        );
    }

    /// A model that declares no levels at all cannot be clamped into one.
    #[test]
    fn an_effort_is_dropped_when_the_model_declares_no_levels() {
        let (effort, note) =
            clamp_spawn_agent_reasoning_effort("local-model", &[], &ReasoningEffort::High);
        assert_eq!(effort, None);
        assert!(
            note.expect("dropping the effort must be reported")
                .contains("no reasoning levels")
        );
    }

    /// FORK: pinning an account is only meaningful for a Claude-backed child.
    #[tokio::test]
    async fn claude_account_pin_requires_a_claude_backed_child() {
        let (_home, mut config) = claude_test_config(vec![PathBuf::from("/accounts/a")]).await;

        let err = apply_spawn_agent_claude_account(&mut config, Some("1"))
            .expect_err("a non-Claude child cannot take an account");

        let FunctionCallError::RespondToModel(message) = err else {
            panic!("expected a model-facing error");
        };
        assert!(message.contains("Claude-backed"), "{message}");
        assert_eq!(config.claude_code_account_override, None);
    }

    /// FORK: an index, a path, or part of the email all name the same account.
    #[tokio::test]
    async fn claude_account_pin_accepts_an_index_and_clears_on_auto() {
        let dirs = vec![PathBuf::from("/accounts/a"), PathBuf::from("/accounts/b")];
        let (_home, mut config) = claude_test_config(dirs.clone()).await;
        config.model_provider_id = CLAUDE_CODE_PROVIDER_ID.to_string();

        apply_spawn_agent_claude_account(&mut config, Some("2")).expect("index should resolve");
        assert_eq!(config.claude_code_account_override, Some(dirs[1].clone()));

        apply_spawn_agent_claude_account(&mut config, Some("auto")).expect("auto should clear");
        assert_eq!(config.claude_code_account_override, None);

        let err = apply_spawn_agent_claude_account(&mut config, Some("nope"))
            .expect_err("an unknown account is an error, not a silent default");
        let FunctionCallError::RespondToModel(message) = err else {
            panic!("expected a model-facing error");
        };
        assert!(message.contains("unknown Claude account"), "{message}");
    }

    /// FORK: naming a locally served model without a role must still route to
    /// the provider that can serve it.
    #[tokio::test]
    async fn fork_invariant_claude_model_routes_to_claude_provider() {
        let (_home, mut config) = claude_test_config(Vec::new()).await;
        config.model = Some("claude-opus-5".to_string());
        assert_ne!(config.model_provider_id, CLAUDE_CODE_PROVIDER_ID);

        align_provider_with_locally_served_model(&mut config).expect("provider should align");

        assert_eq!(config.model_provider_id, CLAUDE_CODE_PROVIDER_ID);
        assert_eq!(config.model_provider.wire_api, WireApi::ClaudeCode);
    }

    /// FORK: the same routing for the ChatGPT Web bundle, which must land on
    /// its own provider and not on `claude_code`.
    #[tokio::test]
    async fn fork_invariant_chatgpt_web_model_routes_to_chatgpt_web_provider() {
        let (_home, mut config) = claude_test_config(Vec::new()).await;
        config.model = Some("chatgpt-web/thinking".to_string());
        assert_ne!(config.model_provider_id, CHATGPT_WEB_PROVIDER_ID);

        align_provider_with_locally_served_model(&mut config).expect("provider should align");

        assert_eq!(config.model_provider_id, CHATGPT_WEB_PROVIDER_ID);
        assert_eq!(config.model_provider.wire_api, WireApi::ChatGptWeb);
    }

    /// FORK: a ChatGPT Web child is briefed like a Claude child: task-only by
    /// default, capped by its own config table otherwise.
    #[test]
    fn fork_invariant_chatgpt_web_children_use_task_only_fork_rules() {
        assert_eq!(
            task_fork_mode_for_wire_api(
                WireApi::ChatGptWeb,
                Some(SpawnAgentForkMode::LastNTurns(4)),
                /*max_fork_turns*/ 0,
            ),
            (None, Some(LOCAL_TASK_ONLY_FORK_NOTE.to_string()))
        );
        assert_eq!(
            task_fork_mode_for_wire_api(
                WireApi::ChatGptWeb,
                Some(SpawnAgentForkMode::LastNTurns(4)),
                /*max_fork_turns*/ 2,
            ),
            (
                Some(SpawnAgentForkMode::LastNTurns(2)),
                Some(
                    "`fork_turns` was reduced from 4 to 2 by `[chatgpt_web] max_fork_turns`."
                        .to_string()
                )
            )
        );
        assert_eq!(
            task_fork_mode_for_wire_api(
                WireApi::ChatGptWeb,
                Some(SpawnAgentForkMode::FullHistory),
                /*max_fork_turns*/ 8,
            )
            .0,
            None
        );
    }

    #[tokio::test]
    async fn fork_invariant_effective_multi_agent_mode_is_ultra_only() {
        let (_session, regular_turn, _rx) =
            crate::session::tests::make_session_and_context_with_auth_and_config_and_rx(
                CodexAuth::from_api_key("Test API Key"),
                Vec::new(),
                |config| {
                    let _ = config.features.enable(Feature::MultiAgentV2);
                    config.model_reasoning_effort = Some(ReasoningEffort::High);
                },
            )
            .await;
        assert_eq!(regular_turn.multi_agent_version, MultiAgentVersion::V2);
        assert_eq!(
            crate::session::multi_agents::effective_multi_agent_mode(&regular_turn),
            Some(MultiAgentMode::ExplicitRequestOnly)
        );

        let (_session, ultra_turn, _rx) =
            crate::session::tests::make_session_and_context_with_auth_and_config_and_rx(
                CodexAuth::from_api_key("Test API Key"),
                Vec::new(),
                |config| {
                    let _ = config.features.enable(Feature::MultiAgentV2);
                    config.model_reasoning_effort = Some(ReasoningEffort::Ultra);
                },
            )
            .await;
        assert_eq!(ultra_turn.multi_agent_version, MultiAgentVersion::V2);
        assert_eq!(
            crate::session::multi_agents::effective_multi_agent_mode(&ultra_turn),
            Some(MultiAgentMode::Proactive)
        );
    }

    /// FORK: each locally served provider reads its own `max_fork_turns`.
    #[tokio::test]
    async fn max_fork_turns_comes_from_the_matching_config_table() {
        let (_home, mut config) = claude_test_config(Vec::new()).await;
        config.claude_code_max_fork_turns = 3;
        config.chatgpt_web.max_fork_turns = 5;
        assert_eq!(max_fork_turns_for_wire_api(&config, WireApi::ClaudeCode), 3);
        assert_eq!(max_fork_turns_for_wire_api(&config, WireApi::ChatGptWeb), 5);
        assert_eq!(max_fork_turns_for_wire_api(&config, WireApi::Responses), 0);
    }

    async fn claude_test_config(
        account_dirs: Vec<PathBuf>,
    ) -> (tempfile::TempDir, crate::config::Config) {
        let home = tempfile::TempDir::new().expect("create temp dir");
        let home_path = home.path().to_path_buf();
        let mut config = crate::config::ConfigBuilder::default()
            .codex_home(home_path.clone())
            .fallback_cwd(Some(home_path))
            .build()
            .await
            .expect("load test config");
        config.claude_code_account_dirs = account_dirs;
        (home, config)
    }
}
