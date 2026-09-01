use super::*;
use crate::agent::AgentActivity;
use crate::agent::AgentLifecycle;
use crate::agent::AgentLifecycleStatus;
use crate::agent::AgentStatus;
use crate::unified_exec::TerminalProcessSnapshot;
use crate::unified_exec::TerminalProcessState;
use codex_protocol::AgentPath;
use codex_protocol::ThreadId;
use serde::Deserialize;
use serde::Serialize;

#[derive(Clone, Debug)]
pub(super) struct ResolvedTarget {
    pub(super) thread_id: ThreadId,
    pub(super) path: AgentPath,
}

pub(super) async fn target_snapshots(
    control: &crate::agent::AgentControl,
    targets: &[ResolvedTarget],
) -> Vec<WaitAgentTargetSnapshot> {
    let targets = if targets.is_empty() {
        control
            .agent_entries_for_prefix(None)
            .into_iter()
            .filter(|(_, path)| !path.is_root())
            .map(|(thread_id, path)| ResolvedTarget { thread_id, path })
            .collect()
    } else {
        targets.to_vec()
    };
    let mut snapshots = Vec::with_capacity(targets.len());
    for target in targets {
        let lifecycle = control.agent_lifecycle(target.thread_id).await;
        let activity = control.agent_activity(target.thread_id);
        let terminal = control
            .terminal_observability_snapshots(target.thread_id)
            .await
            .into_iter()
            .find(|snapshot| {
                matches!(
                    snapshot.state,
                    TerminalProcessState::Waiting | TerminalProcessState::NeedsAttention
                )
            });
        snapshots.push(target_snapshot_with_lifecycle(
            target.path,
            lifecycle,
            activity,
            terminal,
        ));
    }
    snapshots
}

fn target_snapshot(
    path: AgentPath,
    status: AgentStatus,
    activity: Option<AgentActivity>,
) -> WaitAgentTargetSnapshot {
    target_snapshot_with_lifecycle(
        path,
        AgentLifecycle::from_agent_status(&status, 0, activity.as_ref().map(|a| a.label.as_str())),
        activity,
        None,
    )
}

fn target_snapshot_with_lifecycle(
    path: AgentPath,
    lifecycle: AgentLifecycle,
    activity: Option<AgentActivity>,
    terminal: Option<TerminalProcessSnapshot>,
) -> WaitAgentTargetSnapshot {
    let status = if !lifecycle.status.is_terminal()
        && terminal.as_ref().is_some_and(|snapshot| {
            matches!(
                snapshot.state,
                TerminalProcessState::Waiting | TerminalProcessState::NeedsAttention
            )
        }) {
        AgentLifecycleStatus::WaitingForTool
    } else {
        lifecycle.status
    };
    let waiting_tool = matches!(status, AgentLifecycleStatus::WaitingForTool)
        .then(|| "tool".to_string())
        .filter(|_| {
            terminal.is_none()
                && activity.as_ref().is_some_and(|activity| {
                    !activity.label.to_ascii_lowercase().contains("terminal")
                })
        });
    let (last_activity_at, idle_ms) = terminal
        .as_ref()
        .map(|snapshot| {
            (
                Some(snapshot.last_activity_at),
                Some(now_ms().saturating_sub(snapshot.last_activity_at)),
            )
        })
        .or_else(|| {
            activity.as_ref().map(|activity| {
                (
                    Some(activity.at_ms),
                    Some(now_ms().saturating_sub(activity.at_ms)),
                )
            })
        })
        .unwrap_or((None, None));
    WaitAgentTargetSnapshot {
        canonical_path: path.to_string(),
        status,
        generation: lifecycle.generation,
        last_activity_at,
        idle_ms,
        waiting_terminal: terminal,
        waiting_tool,
    }
}

fn wait_status(
    status: AgentStatus,
    activity: Option<&AgentActivity>,
) -> (AgentLifecycleStatus, Option<String>) {
    let status = AgentLifecycleStatus::from_agent_status(
        &status,
        activity.map(|activity| activity.label.as_str()),
    );
    let waiting_tool = matches!(status, AgentLifecycleStatus::WaitingForTool)
        .then(|| "tool".to_string())
        .filter(|_| {
            activity
                .is_some_and(|activity| !activity.label.to_ascii_lowercase().contains("terminal"))
        });
    (status, waiting_tool)
}

pub(super) fn is_final(status: &AgentStatus) -> bool {
    matches!(
        status,
        AgentStatus::Completed(_)
            | AgentStatus::Errored(_)
            | AgentStatus::Interrupted
            | AgentStatus::Shutdown
            | AgentStatus::NotFound
    )
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis() as u64)
        .unwrap_or_default()
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub(crate) enum WaitAgentWakeReason {
    Message,
    StatusChanged,
    Terminal,
    Timeout,
    NeedsAttention,
}

pub(crate) use crate::agent::AgentLifecycleStatus as WaitAgentTargetStatus;

#[derive(Debug, Deserialize, Serialize, PartialEq, Eq)]
pub(crate) struct WaitAgentTargetSnapshot {
    #[serde(rename = "canonicalPath")]
    pub(crate) canonical_path: String,
    pub(crate) status: WaitAgentTargetStatus,
    pub(crate) generation: u64,
    #[serde(rename = "lastActivityAt")]
    pub(crate) last_activity_at: Option<u64>,
    #[serde(rename = "idleMs")]
    pub(crate) idle_ms: Option<u64>,
    #[serde(rename = "waitingTerminal")]
    pub(crate) waiting_terminal: Option<TerminalProcessSnapshot>,
    #[serde(rename = "waitingTool")]
    pub(crate) waiting_tool: Option<String>,
}

#[derive(Debug, Deserialize, Serialize, PartialEq, Eq)]
pub(crate) struct WaitAgentResult {
    pub(crate) message: String,
    pub(crate) timed_out: bool,
    pub(crate) revision: u64,
    pub(crate) reason: WaitAgentWakeReason,
    pub(crate) targets: Vec<WaitAgentTargetSnapshot>,
    /// FORK: what each live agent was last observed doing.
    pub(crate) agents: Vec<WaitAgentSnapshot>,
}

#[derive(Debug, Deserialize, Serialize, PartialEq, Eq)]
pub(crate) struct WaitAgentSnapshot {
    pub(crate) agent_name: String,
    pub(crate) status: WaitAgentTargetStatus,
    pub(crate) generation: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) last_activity: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) idle_seconds: Option<u64>,
}

impl WaitAgentResult {
    pub(crate) fn from_outcome(
        outcome: WaitOutcome,
        requested_timeout_ms: Option<i64>,
        timeout_ms: i64,
        targets: Vec<WaitAgentTargetSnapshot>,
        agents: Vec<WaitAgentSnapshot>,
    ) -> Self {
        let was_steered = matches!(outcome, WaitOutcome::Steered { .. });
        let (revision, reason, timed_out) = match outcome {
            WaitOutcome::Progress { revision, reason } => (revision, reason, false),
            WaitOutcome::Steered { revision } => {
                (revision, WaitAgentWakeReason::StatusChanged, false)
            }
            WaitOutcome::TimedOut {
                revision,
                needs_attention,
            } => (
                revision,
                if needs_attention {
                    WaitAgentWakeReason::NeedsAttention
                } else {
                    WaitAgentWakeReason::Timeout
                },
                true,
            ),
        };
        let message = if was_steered {
            "Wait interrupted by new input."
        } else {
            match reason {
                WaitAgentWakeReason::Message
                | WaitAgentWakeReason::StatusChanged
                | WaitAgentWakeReason::Terminal
                | WaitAgentWakeReason::NeedsAttention => "Wait completed.",
                WaitAgentWakeReason::Timeout => "Wait timed out.",
            }
        };
        let message = match requested_timeout_ms {
            Some(requested_timeout_ms) if requested_timeout_ms < timeout_ms && !was_steered => {
                format!(
                    "{message}\n\nRequested timeout of {requested_timeout_ms}ms was clamped to the minimum of {timeout_ms}ms."
                )
            }
            Some(_) | None => message.to_string(),
        };
        let message = if timed_out && !agents.is_empty() {
            let lines: Vec<String> = agents
                .iter()
                .map(|agent| match (&agent.last_activity, agent.idle_seconds) {
                    (Some(activity), Some(idle)) => {
                        format!("- {} {activity} {idle}s ago", agent.agent_name)
                    }
                    (Some(activity), None) => format!("- {} {activity}", agent.agent_name),
                    _ => format!("- {} has not reported activity yet", agent.agent_name),
                })
                .collect();
            format!("{message}\n\nLive agents:\n{}", lines.join("\n"))
        } else {
            message
        };
        Self {
            message,
            timed_out,
            revision,
            reason,
            targets,
            agents,
        }
    }
}

impl ToolOutput for WaitAgentResult {
    fn log_output(&self) -> String {
        tool_output_json_text(self, "wait_agent")
    }

    fn success_for_logging(&self) -> bool {
        true
    }

    fn to_response_item(&self, call_id: &str, payload: &ToolPayload) -> ResponseInputItem {
        tool_output_response_item(call_id, payload, self, /*success*/ None, "wait_agent")
    }

    fn code_mode_result(&self, _payload: &ToolPayload) -> JsonValue {
        tool_output_code_mode_result(self, "wait_agent")
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum WaitOutcome {
    Progress {
        revision: u64,
        reason: WaitAgentWakeReason,
    },
    Steered {
        revision: u64,
    },
    TimedOut {
        revision: u64,
        needs_attention: bool,
    },
}

#[cfg(test)]
#[path = "wait_state_tests.rs"]
mod tests;
