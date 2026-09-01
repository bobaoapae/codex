use super::*;
use crate::agent::AgentChangeKind;
use crate::session::InputQueueActivity;
use crate::tools::handlers::multi_agents_spec::WaitAgentTimeoutOptions;
use crate::tools::handlers::multi_agents_spec::create_wait_agent_tool_v2;
use codex_protocol::AgentPath;
use codex_protocol::ThreadId;
use codex_tools::ToolSpec;
use serde::Deserialize;
use std::collections::HashMap;
use std::collections::HashSet;
use std::time::Duration;
use tokio::sync::watch;
use tokio::time::Instant;
use tokio::time::timeout_at;

use super::wait_state::ResolvedTarget;
pub(crate) use super::wait_state::WaitAgentResult;
use super::wait_state::WaitAgentSnapshot;
use super::wait_state::WaitAgentTargetStatus;
pub(crate) use super::wait_state::WaitAgentWakeReason;
use super::wait_state::WaitOutcome;
use super::wait_state::is_final;
use super::wait_state::target_snapshots;

#[derive(Default)]
pub(crate) struct Handler {
    options: WaitAgentTimeoutOptions,
}

impl Handler {
    pub(crate) fn new(options: WaitAgentTimeoutOptions) -> Self {
        Self { options }
    }
}

impl ToolExecutor<ToolInvocation> for Handler {
    fn tool_name(&self) -> ToolName {
        ToolName::plain("wait_agent")
    }

    fn spec(&self) -> ToolSpec {
        create_wait_agent_tool_v2(self.options)
    }

    fn handle<'a>(&'a self, invocation: ToolInvocation) -> codex_tools::ToolExecutorFuture<'a>
    where
        ToolInvocation: 'a,
    {
        Box::pin(self.handle_call(invocation))
    }
}

impl Handler {
    async fn handle_call(
        &self,
        invocation: ToolInvocation,
    ) -> Result<Box<dyn crate::tools::context::ToolOutput>, FunctionCallError> {
        let ToolInvocation {
            session,
            turn,
            payload,
            call_id,
            ..
        } = invocation;
        let arguments = function_arguments(payload)?;
        let args: WaitArgs = parse_arguments(&arguments)?;
        let min_timeout_ms = turn.config.multi_agent_v2.min_wait_timeout_ms;
        let max_timeout_ms = turn.config.multi_agent_v2.max_wait_timeout_ms;
        let default_timeout_ms = turn.config.multi_agent_v2.default_wait_timeout_ms;
        let requested_timeout_ms = args.timeout_ms;
        let timeout_ms = match requested_timeout_ms {
            Some(ms) if ms > max_timeout_ms => {
                return Err(FunctionCallError::RespondToModel(format!(
                    "timeout_ms must be at most {max_timeout_ms}"
                )));
            }
            Some(ms) => ms.max(min_timeout_ms),
            None => default_timeout_ms,
        };

        let control = &session.services.agent_control;
        control.register_session_root(session.thread_id, turn.parent_thread_id);
        let target_references = args.targets.unwrap_or_default();
        let targets = resolve_targets(
            control,
            session.thread_id,
            &turn.session_source,
            &target_references,
        )
        .await?;
        let after_revision = args.after_revision;
        let current_revision = control.current_revision();
        let baseline = after_revision.unwrap_or(current_revision);
        let turn_state = session
            .input_queue
            .turn_state_for_sub_id(&session.active_turn, &turn.sub_id)
            .await;
        let (mut activity_rx, pending_activity) = session
            .input_queue
            .subscribe_activity(turn_state.as_deref())
            .await;
        let mut revision_rx = control.subscribe_revision();

        session
            .emit_turn_item_started(
                &turn,
                &TurnItem::CollabAgentToolCall(CollabAgentToolCallItem {
                    id: call_id.clone(),
                    tool: CollabAgentTool::Wait,
                    status: CollabAgentToolCallStatus::InProgress,
                    sender_thread_id: session.thread_id,
                    receiver_thread_ids: Vec::new(),
                    receiver_agents: Vec::new(),
                    prompt: None,
                    model: None,
                    reasoning_effort: None,
                    agents_states: Default::default(),
                }),
            )
            .await;

        let deadline = Instant::now() + Duration::from_millis(timeout_ms as u64);
        let outcome = wait_for_outcome(
            WaitParameters {
                session: &session,
                targets: &targets,
                baseline,
                include_current_terminal: after_revision.is_none(),
                accept_existing_mailbox: after_revision.is_none(),
                timeout_duration: Duration::from_millis(timeout_ms as u64),
            },
            pending_activity,
            &mut activity_rx,
            &mut revision_rx,
            deadline,
        )
        .await;
        let target_snapshots = target_snapshots(control, &targets).await;
        let agents = live_agent_snapshots(&session, &turn).await;
        let result = WaitAgentResult::from_outcome(
            outcome,
            requested_timeout_ms,
            timeout_ms,
            target_snapshots,
            agents,
        );

        session
            .emit_turn_item_completed(
                &turn,
                TurnItem::CollabAgentToolCall(CollabAgentToolCallItem {
                    id: call_id,
                    tool: CollabAgentTool::Wait,
                    status: CollabAgentToolCallStatus::Completed,
                    sender_thread_id: session.thread_id,
                    receiver_thread_ids: Vec::new(),
                    receiver_agents: Vec::new(),
                    prompt: None,
                    model: None,
                    reasoning_effort: None,
                    agents_states: HashMap::new(),
                }),
            )
            .await;

        Ok(boxed_tool_output(result))
    }
}

impl CoreToolRuntime for Handler {
    fn matches_kind(&self, payload: &ToolPayload) -> bool {
        matches!(payload, ToolPayload::Function { .. })
    }
}

struct WaitParameters<'a> {
    session: &'a crate::session::session::Session,
    targets: &'a [ResolvedTarget],
    baseline: u64,
    include_current_terminal: bool,
    accept_existing_mailbox: bool,
    timeout_duration: Duration,
}

async fn resolve_targets(
    control: &crate::agent::AgentControl,
    current_thread_id: ThreadId,
    session_source: &codex_protocol::protocol::SessionSource,
    references: &[String],
) -> Result<Vec<ResolvedTarget>, FunctionCallError> {
    let mut resolved = Vec::with_capacity(references.len());
    let mut seen = HashSet::new();
    for reference in references {
        let path = control
            .resolve_agent_path(current_thread_id, session_source, reference)
            .map_err(agent_reference_error)?;
        let thread_id = control
            .resolve_agent_reference(current_thread_id, session_source, reference)
            .await
            .map_err(agent_reference_error)?;
        if seen.insert(path.to_string()) {
            resolved.push(ResolvedTarget { thread_id, path });
        }
    }
    Ok(resolved)
}

fn agent_reference_error(error: codex_protocol::error::CodexErr) -> FunctionCallError {
    FunctionCallError::RespondToModel(error.to_string())
}

async fn wait_for_outcome(
    parameters: WaitParameters<'_>,
    pending_activity: Option<InputQueueActivity>,
    activity_rx: &mut watch::Receiver<InputQueueActivity>,
    revision_rx: &mut watch::Receiver<u64>,
    deadline: Instant,
) -> WaitOutcome {
    let WaitParameters {
        session,
        targets,
        baseline,
        include_current_terminal,
        accept_existing_mailbox,
        timeout_duration,
    } = parameters;
    if matches!(pending_activity, Some(InputQueueActivity::Steer)) {
        return WaitOutcome::Steered { revision: baseline };
    }
    if matches!(pending_activity, Some(InputQueueActivity::Mailbox))
        && (targets.is_empty() || mailbox_matches_targets(session, targets).await)
        && (accept_existing_mailbox || matching_message_is_new(session, targets, baseline).await)
    {
        return WaitOutcome::Progress {
            revision: matching_message_revision(session, targets, baseline).await,
            reason: WaitAgentWakeReason::Message,
        };
    }
    if let Some((revision, kind)) =
        latest_matching_change(session, targets, baseline, include_current_terminal).await
    {
        return WaitOutcome::Progress {
            revision,
            reason: wake_reason(kind),
        };
    }

    loop {
        tokio::select! {
            activity = timeout_at(deadline, activity_rx.changed()) => {
                match activity {
                    Ok(Ok(())) => {
                        let activity = *activity_rx.borrow_and_update();
                        match activity {
                            InputQueueActivity::Mailbox => {
                                if targets.is_empty()
                                    || mailbox_matches_targets(session, targets).await
                                {
                                    let revision =
                                        newly_arrived_message_revision(session, targets, baseline)
                                            .await;
                                    return WaitOutcome::Progress {
                                        revision,
                                        reason: WaitAgentWakeReason::Message,
                                    };
                                }
                            }
                            InputQueueActivity::Steer => {
                                return WaitOutcome::Steered { revision: baseline };
                            }
                        }
                    }
                    Ok(Err(_)) | Err(_) => return timeout_outcome(session, targets, baseline, timeout_duration).await,
                }
            }
            revision = timeout_at(deadline, revision_rx.changed()) => {
                match revision {
                    Ok(Ok(())) => {
                        if let Some((revision, kind)) = latest_matching_change(
                            session,
                            targets,
                            baseline,
                            /*include_current_terminal*/ false,
                        ).await {
                            return WaitOutcome::Progress {
                                revision,
                                reason: wake_reason(kind),
                            };
                        }
                    }
                    Ok(Err(_)) | Err(_) => return timeout_outcome(session, targets, baseline, timeout_duration).await,
                }
            }
        }
    }
}

async fn timeout_outcome(
    session: &crate::session::session::Session,
    targets: &[ResolvedTarget],
    baseline: u64,
    timeout_duration: Duration,
) -> WaitOutcome {
    let snapshots = target_snapshots(&session.services.agent_control, targets).await;
    let needs_attention = snapshots.iter().any(|target| {
        target.waiting_terminal.as_ref().is_some_and(|terminal| {
            terminal.state == crate::unified_exec::TerminalProcessState::NeedsAttention
        }) || matches!(
            target.status,
            WaitAgentTargetStatus::Running
                | WaitAgentTargetStatus::WaitingForTool
                | WaitAgentTargetStatus::WaitingForApproval
                | WaitAgentTargetStatus::WaitingForUser
        ) && target
            .idle_ms
            .is_none_or(|idle_ms| idle_ms >= timeout_duration.as_millis() as u64)
    });
    WaitOutcome::TimedOut {
        revision: baseline,
        needs_attention,
    }
}

async fn latest_matching_change(
    session: &crate::session::session::Session,
    targets: &[ResolvedTarget],
    baseline: u64,
    include_current_terminal: bool,
) -> Option<(u64, AgentChangeKind)> {
    let control = &session.services.agent_control;
    let entries: Vec<(ThreadId, AgentPath)> = if targets.is_empty() {
        control
            .agent_entries_for_prefix(None)
            .into_iter()
            .filter(|(_, path)| !path.is_root())
            .collect()
    } else {
        targets
            .iter()
            .flat_map(|target| control.agent_entries_for_prefix(Some(&target.path)))
            .chain(
                targets
                    .iter()
                    .map(|target| (target.thread_id, target.path.clone())),
            )
            .collect()
    };
    let mut seen = HashSet::new();
    let mut latest = None;
    for (thread_id, _) in entries {
        if !seen.insert(thread_id) {
            continue;
        }
        if include_current_terminal && is_final(&control.get_status(thread_id).await) {
            let change = control.last_agent_change(thread_id);
            if change.is_none_or(|change| change.revision <= baseline) {
                return Some((baseline, AgentChangeKind::Terminal));
            }
        }
        let Some(change) = control.last_agent_change(thread_id) else {
            continue;
        };
        if change.revision > baseline
            && latest.is_none_or(|(revision, _)| change.revision > revision)
        {
            latest = Some((change.revision, change.kind));
        }
    }
    latest
}

fn wake_reason(kind: AgentChangeKind) -> WaitAgentWakeReason {
    match kind {
        AgentChangeKind::Message => WaitAgentWakeReason::Message,
        AgentChangeKind::StatusChanged => WaitAgentWakeReason::StatusChanged,
        AgentChangeKind::NeedsAttention => WaitAgentWakeReason::NeedsAttention,
        AgentChangeKind::Terminal => WaitAgentWakeReason::Terminal,
    }
}

async fn matching_message_revision(
    session: &crate::session::session::Session,
    targets: &[ResolvedTarget],
    baseline: u64,
) -> u64 {
    latest_matching_change(
        session, targets, baseline, /*include_current_terminal*/ false,
    )
    .await
    .map(|(revision, _)| revision)
    .unwrap_or_else(|| session.services.agent_control.current_revision())
}

async fn matching_message_is_new(
    session: &crate::session::session::Session,
    targets: &[ResolvedTarget],
    baseline: u64,
) -> bool {
    latest_matching_change(
        session, targets, baseline, /*include_current_terminal*/ false,
    )
    .await
    .is_some_and(|(_, kind)| kind == AgentChangeKind::Message)
}

async fn newly_arrived_message_revision(
    session: &crate::session::session::Session,
    targets: &[ResolvedTarget],
    baseline: u64,
) -> u64 {
    if !matching_message_is_new(session, targets, baseline).await {
        let authors = session.input_queue.pending_mailbox_authors().await;
        for author in authors {
            let Ok(author) = AgentPath::try_from(author.as_str()) else {
                continue;
            };
            if targets.is_empty()
                || targets
                    .iter()
                    .any(|target| path_is_under(&author, &target.path))
            {
                session.services.agent_control.record_agent_message(&author);
            }
        }
    }
    matching_message_revision(session, targets, baseline).await
}

async fn mailbox_matches_targets(
    session: &crate::session::session::Session,
    targets: &[ResolvedTarget],
) -> bool {
    let authors = session.input_queue.pending_mailbox_authors().await;
    authors.iter().any(|author| {
        let Ok(author) = AgentPath::try_from(author.as_str()) else {
            return false;
        };
        targets
            .iter()
            .any(|target| path_is_under(&author, &target.path))
    })
}

fn path_is_under(path: &AgentPath, prefix: &AgentPath) -> bool {
    prefix.is_root()
        || path == prefix
        || path
            .as_str()
            .strip_prefix(prefix.as_str())
            .is_some_and(|suffix| suffix.starts_with('/'))
}

/// FORK: what every live agent was last observed doing.
async fn live_agent_snapshots(
    session: &crate::session::session::Session,
    turn: &crate::session::turn_context::TurnContext,
) -> Vec<WaitAgentSnapshot> {
    let Ok(agents) = session
        .services
        .agent_control
        .list_agents(&turn.session_source, /*path_prefix*/ None)
        .await
    else {
        return Vec::new();
    };
    agents
        .into_iter()
        .map(|agent| WaitAgentSnapshot {
            agent_name: agent.agent_name,
            status: agent.status,
            generation: agent.generation,
            last_activity: agent.last_activity,
            idle_seconds: agent.idle_seconds,
        })
        .collect()
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct WaitArgs {
    timeout_ms: Option<i64>,
    /// FORK: only wake for these agents.
    #[serde(default)]
    targets: Option<Vec<String>>,
    /// Causal revision after which progress is considered new.
    #[serde(default, rename = "afterRevision", alias = "after_revision")]
    after_revision: Option<u64>,
}

#[cfg(test)]
#[path = "wait_tests.rs"]
mod tests;
