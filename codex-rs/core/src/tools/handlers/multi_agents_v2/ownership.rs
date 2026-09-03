use super::*;
use crate::ownership::AuthorizedWorkspaceRoots;
use crate::ownership::MutationOperation;
use crate::ownership::OwnershipEnvironment;
use crate::ownership::OwnershipError;
use crate::ownership::OwnershipGrantRequest;
use crate::ownership::OwnershipReceiptSink;
use crate::ownership::OwnershipReleaseRequest;
use crate::tools::context::ToolOutput;
use crate::tools::handlers::multi_agents_v2::ownership_spec::create_grant_agent_ownership_tool;
use crate::tools::handlers::multi_agents_v2::ownership_spec::create_override_agent_ownership_tool;
use crate::tools::handlers::multi_agents_v2::ownership_spec::create_release_agent_ownership_tool;
use crate::tools::registry::CoreToolRuntime;
use crate::tools::registry::ToolExecutor;
use codex_protocol::models::ResponseInputItem;
use codex_state::WorkflowLeaseMode;
use codex_state::WorkflowLeaseOverrideUse;
use codex_state::WorkflowPathLease;
use codex_tools::ToolName;
use codex_tools::ToolSpec;
use serde::Deserialize;
use serde::Serialize;
use serde_json::Value as JsonValue;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

pub(crate) struct GrantAgentOwnershipHandler;
pub(crate) struct ReleaseAgentOwnershipHandler;
pub(crate) struct OverrideAgentOwnershipHandler;

impl ToolExecutor<ToolInvocation> for GrantAgentOwnershipHandler {
    fn tool_name(&self) -> ToolName {
        ToolName::plain("grant_agent_ownership")
    }

    fn spec(&self) -> ToolSpec {
        create_grant_agent_ownership_tool()
    }

    fn handle<'a>(&'a self, invocation: ToolInvocation) -> codex_tools::ToolExecutorFuture<'a>
    where
        ToolInvocation: 'a,
    {
        Box::pin(async move { handle_grant(invocation).await.map(boxed_tool_output) })
    }
}

impl CoreToolRuntime for GrantAgentOwnershipHandler {
    fn matches_kind(&self, payload: &ToolPayload) -> bool {
        matches!(payload, ToolPayload::Function { .. })
    }
}

impl ToolExecutor<ToolInvocation> for ReleaseAgentOwnershipHandler {
    fn tool_name(&self) -> ToolName {
        ToolName::plain("release_agent_ownership")
    }

    fn spec(&self) -> ToolSpec {
        create_release_agent_ownership_tool()
    }

    fn handle<'a>(&'a self, invocation: ToolInvocation) -> codex_tools::ToolExecutorFuture<'a>
    where
        ToolInvocation: 'a,
    {
        Box::pin(async move { handle_release(invocation).await.map(boxed_tool_output) })
    }
}

impl CoreToolRuntime for ReleaseAgentOwnershipHandler {
    fn matches_kind(&self, payload: &ToolPayload) -> bool {
        matches!(payload, ToolPayload::Function { .. })
    }
}

impl ToolExecutor<ToolInvocation> for OverrideAgentOwnershipHandler {
    fn tool_name(&self) -> ToolName {
        ToolName::plain("override_agent_ownership")
    }

    fn spec(&self) -> ToolSpec {
        create_override_agent_ownership_tool()
    }

    fn handle<'a>(&'a self, invocation: ToolInvocation) -> codex_tools::ToolExecutorFuture<'a>
    where
        ToolInvocation: 'a,
    {
        Box::pin(async move { handle_override(invocation).await.map(boxed_tool_output) })
    }
}

impl CoreToolRuntime for OverrideAgentOwnershipHandler {
    fn matches_kind(&self, payload: &ToolPayload) -> bool {
        matches!(payload, ToolPayload::Function { .. })
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct GrantAgentOwnershipArgs {
    #[serde(alias = "target")]
    agent: String,
    paths: Vec<String>,
    mode: String,
    #[serde(default, rename = "ttlMs", alias = "ttl_ms")]
    ttl_ms: Option<u64>,
    #[serde(default, alias = "environmentId", alias = "environment_id")]
    environment: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ReleaseAgentOwnershipArgs {
    #[serde(rename = "leaseId", alias = "lease_id")]
    lease_id: String,
    token: String,
    generation: i64,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct OverrideAgentOwnershipArgs {
    #[serde(rename = "operationDigest", alias = "operation_digest")]
    operation_digest: String,
    paths: Vec<String>,
    reason: String,
}

#[derive(Debug, Serialize)]
pub(crate) struct OwnershipLeaseResult {
    #[serde(rename = "leaseId")]
    lease_id: String,
    token: String,
    #[serde(rename = "rootRunId")]
    root_run_id: String,
    #[serde(rename = "ownerRunId")]
    owner_run_id: String,
    #[serde(rename = "displayPath")]
    display_path: String,
    #[serde(rename = "comparisonKey")]
    comparison_key: String,
    mode: &'static str,
    generation: i64,
    #[serde(rename = "expiresAtMs")]
    expires_at_ms: Option<i64>,
}

#[derive(Debug, Serialize)]
pub(crate) struct OwnershipGrantResult {
    leases: Vec<OwnershipLeaseResult>,
    /// FORK: set when some requested path needed no lease at all.
    #[serde(skip_serializing_if = "Option::is_none")]
    note: Option<String>,
}

impl From<WorkflowPathLease> for OwnershipLeaseResult {
    fn from(lease: WorkflowPathLease) -> Self {
        Self {
            lease_id: lease.lease_id,
            token: lease.token,
            root_run_id: lease.root_run_id,
            owner_run_id: lease.owner_run_id,
            display_path: lease.path.display,
            comparison_key: lease.path.comparison_key,
            mode: lease.mode.as_str(),
            generation: lease.generation,
            expires_at_ms: lease.expires_at_ms,
        }
    }
}

#[derive(Debug, Serialize)]
pub(crate) struct PreparedOwnershipOverrideResult {
    #[serde(rename = "overrideId")]
    override_id: String,
    token: String,
    generation: i64,
    #[serde(rename = "operationDigest")]
    operation_digest: String,
    paths: Vec<WorkflowLeasePathResult>,
    #[serde(rename = "conflictOwnerRunIds")]
    conflict_owner_run_ids: Vec<String>,
}

#[derive(Debug, Serialize)]
struct WorkflowLeasePathResult {
    display: String,
    #[serde(rename = "comparisonKey")]
    comparison_key: String,
}

impl From<WorkflowLeaseOverrideUse> for PreparedOwnershipOverrideResult {
    fn from(proof: WorkflowLeaseOverrideUse) -> Self {
        Self {
            override_id: proof.override_id,
            token: proof.token,
            generation: proof.generation,
            operation_digest: proof.operation_digest,
            paths: proof
                .paths
                .into_iter()
                .map(|path| WorkflowLeasePathResult {
                    display: path.display,
                    comparison_key: path.comparison_key,
                })
                .collect(),
            conflict_owner_run_ids: proof.conflict_owner_run_ids,
        }
    }
}

impl ToolOutput for OwnershipLeaseResult {
    fn log_output(&self) -> String {
        tool_output_json_text(self, "ownership")
    }

    fn success_for_logging(&self) -> bool {
        true
    }

    fn to_response_item(&self, call_id: &str, payload: &ToolPayload) -> ResponseInputItem {
        tool_output_response_item(call_id, payload, self, Some(true), "ownership")
    }

    fn code_mode_result(&self, _payload: &ToolPayload) -> JsonValue {
        tool_output_code_mode_result(self, "ownership")
    }
}

impl ToolOutput for OwnershipGrantResult {
    fn log_output(&self) -> String {
        tool_output_json_text(self, "grant_agent_ownership")
    }

    fn success_for_logging(&self) -> bool {
        true
    }

    fn to_response_item(&self, call_id: &str, payload: &ToolPayload) -> ResponseInputItem {
        tool_output_response_item(call_id, payload, self, Some(true), "grant_agent_ownership")
    }

    fn code_mode_result(&self, _payload: &ToolPayload) -> JsonValue {
        tool_output_code_mode_result(self, "grant_agent_ownership")
    }
}

impl ToolOutput for PreparedOwnershipOverrideResult {
    fn log_output(&self) -> String {
        tool_output_json_text(self, "override_agent_ownership")
    }

    fn success_for_logging(&self) -> bool {
        true
    }

    fn to_response_item(&self, call_id: &str, payload: &ToolPayload) -> ResponseInputItem {
        tool_output_response_item(
            call_id,
            payload,
            self,
            Some(true),
            "override_agent_ownership",
        )
    }

    fn code_mode_result(&self, _payload: &ToolPayload) -> JsonValue {
        tool_output_code_mode_result(self, "override_agent_ownership")
    }
}

async fn handle_grant(
    invocation: ToolInvocation,
) -> Result<OwnershipGrantResult, FunctionCallError> {
    let ToolInvocation {
        session,
        turn,
        payload,
        ..
    } = invocation;
    require_root_invocation(&turn)?;
    let args: GrantAgentOwnershipArgs = parse_arguments(&function_arguments(payload)?)?;
    let target_id = resolve_agent_target(&session, &turn, &args.agent).await?;
    let target = session
        .services
        .agent_control
        .ownership_actor(target_id)
        .map_err(|error| ownership_tool_error(error.to_string()))?;
    let requester = session
        .services
        .agent_control
        .ownership_actor(session.thread_id)
        .map_err(|error| ownership_tool_error(error.to_string()))?;
    let mode = parse_lease_mode(&args.mode)?;
    let lease_duration = Duration::from_millis(args.ttl_ms.unwrap_or(15 * 60 * 1_000));
    let environment = args
        .environment
        .map_or(OwnershipEnvironment::Default, OwnershipEnvironment::Named);
    let service = session
        .ownership_service()
        .await
        .map_err(ownership_error_to_tool)?;
    let paths: Vec<PathBuf> = args.paths.into_iter().map(PathBuf::from).collect();
    let exempt = exempt_path_count(service.authorized_roots(), &paths);
    let leases = session
        .services
        .agent_control
        .grant_agent_ownership(
            service.authorized_roots().clone(),
            OwnershipGrantRequest {
                requester,
                target,
                paths,
                mode,
                lease_duration,
                environment,
            },
        )
        .await
        .map_err(ownership_error_to_tool)?;
    // FORK: scratch space under `<codex_home>/visualizations` is private to the
    // thread and admission never consults a lease over it. A grant that names
    // only such paths is a no-op, not a failure.
    let note = if exempt > 0 {
        let scratch = session.get_config().await.visualizations_dir();
        Some(scratch_note(exempt, scratch.as_path()))
    } else {
        None
    };
    if leases.is_empty() && note.is_none() {
        return Err(ownership_tool_error(
            "ownership grant returned no lease".to_string(),
        ));
    }
    Ok(OwnershipGrantResult {
        leases: leases.into_iter().map(Into::into).collect(),
        note,
    })
}

/// FORK: how many of `paths` lie in scratch space that needs no lease.
///
/// A path that does not normalize at all is not counted: the grant itself will
/// report that failure.
fn exempt_path_count(roots: &AuthorizedWorkspaceRoots, paths: &[PathBuf]) -> usize {
    paths
        .iter()
        .filter(|path| {
            roots
                .normalize(path)
                .is_ok_and(|normalized| roots.is_lease_exempt(&normalized))
        })
        .count()
}

fn scratch_note(exempt: usize, scratch: &std::path::Path) -> String {
    format!(
        "{exempt} path(s) under {} are private scratch space and need no lease",
        scratch.display()
    )
}

async fn handle_release(
    invocation: ToolInvocation,
) -> Result<OwnershipLeaseResult, FunctionCallError> {
    let ToolInvocation {
        session,
        turn,
        payload,
        ..
    } = invocation;
    require_root_invocation(&turn)?;
    let args: ReleaseAgentOwnershipArgs = parse_arguments(&function_arguments(payload)?)?;
    let requester = session
        .services
        .agent_control
        .ownership_actor(session.thread_id)
        .map_err(|error| ownership_tool_error(error.to_string()))?;
    let service = session
        .ownership_service()
        .await
        .map_err(ownership_error_to_tool)?;
    let lease = session
        .services
        .agent_control
        .release_agent_ownership(
            service.authorized_roots().clone(),
            OwnershipReleaseRequest {
                requester,
                lease_id: args.lease_id,
                token: args.token,
                generation: args.generation,
            },
        )
        .await
        .map_err(ownership_error_to_tool)?;
    Ok(lease.into())
}

async fn handle_override(
    invocation: ToolInvocation,
) -> Result<PreparedOwnershipOverrideResult, FunctionCallError> {
    let ToolInvocation {
        session,
        turn,
        payload,
        ..
    } = invocation;
    require_root_invocation(&turn)?;
    let args: OverrideAgentOwnershipArgs = parse_arguments(&function_arguments(payload)?)?;
    let requester = session
        .services
        .agent_control
        .ownership_actor(session.thread_id)
        .map_err(|error| ownership_tool_error(error.to_string()))?;
    let service = session
        .ownership_service()
        .await
        .map_err(ownership_error_to_tool)?;
    let receipt_sink: Arc<dyn OwnershipReceiptSink> = session.clone();
    let proof = service
        .prepare_override(
            requester,
            args.paths.into_iter().map(PathBuf::from).collect(),
            MutationOperation {
                digest: args.operation_digest,
            },
            args.reason,
            receipt_sink,
        )
        .await
        .map_err(ownership_error_to_tool)?;
    Ok(proof.into())
}

fn require_root_invocation(
    turn: &crate::session::turn_context::TurnContext,
) -> Result<(), FunctionCallError> {
    if turn.session_source.is_non_root_agent() {
        return Err(ownership_tool_error(
            "ownership tools are available only to the root agent".to_string(),
        ));
    }
    Ok(())
}

fn parse_lease_mode(mode: &str) -> Result<WorkflowLeaseMode, FunctionCallError> {
    match mode {
        "read" => Ok(WorkflowLeaseMode::Read),
        "write" => Ok(WorkflowLeaseMode::Write),
        _ => Err(ownership_tool_error(
            "mode must be `read` or `write`".to_string(),
        )),
    }
}

fn ownership_error_to_tool(error: OwnershipError) -> FunctionCallError {
    match error {
        OwnershipError::Conflict {
            conflicts,
            operation_digest,
            paths,
        } => {
            let path_summary = bounded_join(
                paths.iter().take(16).map(|path| path.display.as_str()),
                2_048,
            );
            let conflict_owner_ids = bounded_join(
                conflicts
                    .iter()
                    .map(|conflict| conflict.owner_run_id.as_str()),
                2_048,
            );
            ownership_tool_error(format!(
                "ownership conflict; operationDigest={operation_digest}; paths=[{path_summary}]; conflictOwnerRunIds=[{conflict_owner_ids}]"
            ))
        }
        error => ownership_tool_error(error.to_string()),
    }
}

fn ownership_tool_error(message: String) -> FunctionCallError {
    FunctionCallError::RespondToModel(message)
}

fn bounded_join<'a>(values: impl Iterator<Item = &'a str>, limit: usize) -> String {
    let mut output = String::new();
    for value in values {
        let separator = if output.is_empty() { "" } else { "," };
        if output.len() + separator.len() + value.len() > limit {
            output.push_str("...");
            break;
        }
        output.push_str(separator);
        output.push_str(value);
    }
    output
}

#[cfg(test)]
#[path = "ownership_tests.rs"]
mod tests;
