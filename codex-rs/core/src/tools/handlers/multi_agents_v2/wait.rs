use super::*;
use crate::agent::AgentChangeKind;
use crate::session::InputQueueActivity;
use crate::tools::handlers::multi_agents_spec::WaitAgentTimeoutOptions;
use crate::tools::handlers::multi_agents_spec::create_wait_agent_tool_v2;
use codex_protocol::AgentPath;
use codex_protocol::ThreadId;
use codex_protocol::protocol::CollabAgentRef;
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
use super::wait_state::WaitAgentTargetSnapshot;
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
        let mode = args.mode;

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
        let (receiver_thread_ids, receiver_agents) = receiver_details(control, &targets);

        session
            .emit_turn_item_started(
                &turn,
                &TurnItem::CollabAgentToolCall(CollabAgentToolCallItem {
                    id: call_id.clone(),
                    tool: CollabAgentTool::Wait,
                    status: CollabAgentToolCallStatus::InProgress,
                    sender_thread_id: session.thread_id,
                    receiver_thread_ids: receiver_thread_ids.clone(),
                    receiver_agents: receiver_agents.clone(),
                    prompt: None,
                    model: None,
                    reasoning_effort: None,
                    agents_states: Default::default(),
                }),
            )
            .await;

        let total_timeout_ms = match mode {
            WaitMode::Bounded => timeout_ms,
            WaitMode::UntilChange => max_timeout_ms.max(timeout_ms),
        };
        let timeout_duration = if mode == WaitMode::UntilChange && timeout_ms <= 0 {
            Duration::from_millis(total_timeout_ms.max(1) as u64)
        } else {
            Duration::from_millis(timeout_ms as u64)
        };
        let deadline = Instant::now() + Duration::from_millis(total_timeout_ms as u64);
        let outcome = wait_for_outcome(
            WaitParameters {
                session: &session,
                targets: &targets,
                baseline,
                include_current_terminal: after_revision.is_none(),
                allow_stale_final_escape: mode == WaitMode::UntilChange && after_revision.is_some(),
                accept_existing_mailbox: after_revision.is_none(),
                mode,
                timeout_duration,
            },
            pending_activity,
            &mut activity_rx,
            &mut revision_rx,
            deadline,
        )
        .await;
        let target_snapshots = result_target_snapshots(
            control,
            &targets,
            after_revision,
            baseline,
            &outcome,
            timeout_duration,
            mode,
        )
        .await;
        let include_agents = matches!(
            outcome,
            WaitOutcome::TimedOut {
                needs_attention: true,
                ..
            }
        );
        let agents = if include_agents {
            let target_paths = target_snapshots
                .iter()
                .map(|target| target.canonical_path.as_str())
                .collect::<HashSet<_>>();
            live_agent_snapshots(&session, &turn, &targets)
                .await
                .into_iter()
                .filter(|agent| target_paths.contains(agent.agent_name.as_str()))
                .collect()
        } else {
            Vec::new()
        };
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
                    receiver_thread_ids,
                    receiver_agents,
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
    allow_stale_final_escape: bool,
    accept_existing_mailbox: bool,
    mode: WaitMode,
    timeout_duration: Duration,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "snake_case")]
enum WaitMode {
    #[default]
    Bounded,
    UntilChange,
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

fn receiver_details(
    control: &crate::agent::AgentControl,
    targets: &[ResolvedTarget],
) -> (Vec<ThreadId>, Vec<CollabAgentRef>) {
    let entries: Vec<(ThreadId, AgentPath)> = if targets.is_empty() {
        control
            .agent_entries_for_prefix(None)
            .into_iter()
            .filter(|(_, path)| !path.is_root())
            .collect()
    } else {
        targets
            .iter()
            .map(|target| (target.thread_id, target.path.clone()))
            .collect()
    };
    let mut seen = HashSet::new();
    let mut receiver_thread_ids = Vec::with_capacity(entries.len());
    let mut receiver_agents = Vec::with_capacity(entries.len());
    for (thread_id, _) in entries {
        if !seen.insert(thread_id) {
            continue;
        }
        let metadata = control.get_agent_metadata(thread_id).unwrap_or_default();
        receiver_thread_ids.push(thread_id);
        receiver_agents.push(CollabAgentRef {
            thread_id,
            agent_nickname: metadata.agent_nickname,
            agent_role: metadata.agent_role,
        });
    }
    (receiver_thread_ids, receiver_agents)
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
        allow_stale_final_escape,
        accept_existing_mailbox,
        mode,
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
    if let Some((revision, kind)) = latest_matching_change(
        session,
        targets,
        baseline,
        include_current_terminal,
        allow_stale_final_escape,
        /*include_stale_attention*/ true,
    )
    .await
    {
        return WaitOutcome::Progress {
            revision,
            reason: wake_reason(kind),
        };
    }

    let mut interval_deadline = match mode {
        WaitMode::Bounded => deadline,
        WaitMode::UntilChange => (Instant::now() + timeout_duration).min(deadline),
    };
    loop {
        let interval_expired = tokio::select! {
            activity = timeout_at(interval_deadline, activity_rx.changed()) => {
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
                        false
                    }
                    Ok(Err(_)) => return timeout_outcome(session, targets, baseline, timeout_duration).await,
                    Err(_) => true,
                }
            }
            revision = timeout_at(interval_deadline, revision_rx.changed()) => {
                match revision {
                    Ok(Ok(())) => {
                        if let Some((revision, kind)) = latest_matching_change(
                            session,
                            targets,
                            baseline,
                            include_current_terminal,
                            allow_stale_final_escape,
                            /*include_stale_attention*/ true,
                        ).await {
                            return WaitOutcome::Progress {
                                revision,
                                reason: wake_reason(kind),
                            };
                        }
                        false
                    }
                    Ok(Err(_)) => return timeout_outcome(session, targets, baseline, timeout_duration).await,
                    Err(_) => true,
                }
            }
        };

        if !interval_expired {
            continue;
        }

        if let Some((revision, kind)) = latest_matching_change(
            session,
            targets,
            baseline,
            include_current_terminal,
            allow_stale_final_escape,
            /*include_stale_attention*/ true,
        )
        .await
        {
            return WaitOutcome::Progress {
                revision,
                reason: wake_reason(kind),
            };
        }

        let outcome = timeout_outcome(session, targets, baseline, timeout_duration).await;
        if mode == WaitMode::Bounded
            || !matches!(
                outcome,
                WaitOutcome::TimedOut {
                    needs_attention: false,
                    ..
                }
            )
            || Instant::now() >= deadline
        {
            return outcome;
        }
        interval_deadline = (Instant::now() + timeout_duration).min(deadline);
    }
}

async fn timeout_outcome(
    session: &crate::session::session::Session,
    targets: &[ResolvedTarget],
    baseline: u64,
    timeout_duration: Duration,
) -> WaitOutcome {
    let snapshots = target_snapshots(&session.services.agent_control, targets).await;
    let needs_attention = snapshots
        .iter()
        .any(|target| target_needs_attention(target, timeout_duration));
    WaitOutcome::TimedOut {
        revision: baseline,
        needs_attention,
    }
}

async fn result_target_snapshots(
    control: &crate::agent::AgentControl,
    targets: &[ResolvedTarget],
    after_revision: Option<u64>,
    baseline: u64,
    outcome: &WaitOutcome,
    timeout_duration: Duration,
    mode: WaitMode,
) -> Vec<WaitAgentTargetSnapshot> {
    if matches!(
        outcome,
        WaitOutcome::Steered { .. }
            | WaitOutcome::TimedOut {
                needs_attention: false,
                ..
            }
    ) {
        return Vec::new();
    }
    let snapshots = target_snapshots(control, targets).await;
    let result_targets = if targets.is_empty() {
        control
            .agent_entries_for_prefix(None)
            .into_iter()
            .filter(|(_, path)| !path.is_root())
            .map(|(thread_id, path)| ResolvedTarget { thread_id, path })
            .collect::<Vec<_>>()
    } else {
        targets.to_vec()
    };
    let has_cursor = after_revision.is_some();
    let stale_terminal_escape = matches!(
        outcome,
        WaitOutcome::Progress {
            revision,
            reason: WaitAgentWakeReason::Terminal,
        } if *revision <= baseline
    );
    let progress_reason = match outcome {
        WaitOutcome::Progress { reason, .. } => Some(*reason),
        WaitOutcome::Steered { .. } | WaitOutcome::TimedOut { .. } => None,
    };
    let timed_out_with_attention = matches!(
        outcome,
        WaitOutcome::TimedOut {
            needs_attention: true,
            ..
        }
    );

    let all_target_snapshots_final = snapshots_are_all_final(&snapshots);
    let mut result = Vec::new();
    for snapshot in snapshots {
        let Some(target) = result_targets
            .iter()
            .find(|target| target.path.as_str() == snapshot.canonical_path)
        else {
            continue;
        };
        let changed = target_has_new_change(control, target, baseline);
        let attention = target_needs_attention(&snapshot, timeout_duration);
        let stale_terminal_target = stale_terminal_escape
            && snapshot.status.is_terminal()
            && (!has_cursor || mode == WaitMode::UntilChange && all_target_snapshots_final);
        let include = if timed_out_with_attention {
            attention
        } else if matches!(outcome, WaitOutcome::TimedOut { .. }) {
            false
        } else if let Some(reason) = progress_reason {
            changed
                || attention && reason == WaitAgentWakeReason::NeedsAttention
                || stale_terminal_target
                || !has_cursor && reason == WaitAgentWakeReason::Message
        } else {
            false
        };
        if include {
            result.push(snapshot);
        }
    }
    result
}

fn snapshots_are_all_final(snapshots: &[WaitAgentTargetSnapshot]) -> bool {
    !snapshots.is_empty()
        && snapshots
            .iter()
            .all(|snapshot| snapshot.status.is_terminal())
}

fn target_has_new_change(
    control: &crate::agent::AgentControl,
    target: &ResolvedTarget,
    baseline: u64,
) -> bool {
    control
        .agent_entries_for_prefix(Some(&target.path))
        .into_iter()
        .chain(std::iter::once((target.thread_id, target.path.clone())))
        .map(|(thread_id, _)| thread_id)
        .collect::<HashSet<_>>()
        .into_iter()
        .any(|thread_id| {
            control
                .last_agent_change(thread_id)
                .is_some_and(|change| change.revision > baseline)
        })
}

fn target_needs_attention(target: &WaitAgentTargetSnapshot, timeout_duration: Duration) -> bool {
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
}

async fn latest_matching_change(
    session: &crate::session::session::Session,
    targets: &[ResolvedTarget],
    baseline: u64,
    include_current_terminal: bool,
    allow_stale_final_escape: bool,
    include_stale_attention: bool,
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
    let mut any_final = false;
    let mut all_final = !entries.is_empty();
    let mut stale_final = None;
    let mut stale_attention = None;
    for (thread_id, _) in entries {
        if !seen.insert(thread_id) {
            continue;
        }
        let status = control.get_status(thread_id).await;
        let final_status = is_final(&status);
        any_final |= final_status;
        all_final &= final_status;
        let change = control.last_agent_change(thread_id);
        if final_status && change.is_none_or(|change| change.revision <= baseline) {
            stale_final = Some((baseline, AgentChangeKind::Terminal));
        }
        if include_stale_attention
            && control
                .terminal_observability_snapshots(thread_id)
                .await
                .iter()
                .any(|snapshot| {
                    matches!(
                        snapshot.state,
                        crate::unified_exec::TerminalProcessState::NeedsAttention
                    )
                })
        {
            stale_attention = Some((baseline, AgentChangeKind::NeedsAttention));
        }
        if let Some(change) = change
            && change.revision > baseline
            && latest.is_none_or(|(revision, _)| change.revision > revision)
        {
            latest = Some((change.revision, change.kind));
        }
    }
    latest.or(stale_attention).or_else(|| {
        if include_current_terminal && any_final && stale_final.is_some() {
            stale_final
        } else if allow_stale_final_escape && all_final {
            stale_final
        } else {
            None
        }
    })
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
        /*allow_stale_final_escape*/ false, /*include_stale_attention*/ false,
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
        /*allow_stale_final_escape*/ false, /*include_stale_attention*/ false,
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
    targets: &[ResolvedTarget],
) -> Vec<WaitAgentSnapshot> {
    let agents = if targets.is_empty() {
        let Ok(agents) = session
            .services
            .agent_control
            .list_agents(&turn.session_source, /*path_prefix*/ None)
            .await
        else {
            return Vec::new();
        };
        agents
    } else {
        let mut agents = Vec::new();
        for target in targets {
            let Ok(target_agents) = session
                .services
                .agent_control
                .list_agents(&turn.session_source, Some(target.path.as_str()))
                .await
            else {
                continue;
            };
            agents.extend(
                target_agents
                    .into_iter()
                    .filter(|agent| agent.agent_name == target.path.as_str()),
            );
        }
        agents
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
    /// Use the bounded legacy interval or keep waiting until a causal change.
    #[serde(default)]
    mode: WaitMode,
    /// Causal revision after which progress is considered new.
    #[serde(default, rename = "afterRevision", alias = "after_revision")]
    after_revision: Option<u64>,
}

#[cfg(test)]
#[path = "wait_tests.rs"]
mod tests;
