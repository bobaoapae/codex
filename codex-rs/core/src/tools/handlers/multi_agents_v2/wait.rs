use super::*;
use crate::session::InputQueueActivity;
use crate::tools::handlers::multi_agents_spec::WaitAgentTimeoutOptions;
use crate::tools::handlers::multi_agents_spec::create_wait_agent_tool_v2;
use codex_tools::ToolSpec;
use std::collections::HashMap;
use std::time::Duration;
use tokio::time::Instant;
use tokio::time::timeout_at;

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

        let turn_state = session
            .input_queue
            .turn_state_for_sub_id(&session.active_turn, &turn.sub_id)
            .await;
        let (mut activity_rx, pending_activity) = session
            .input_queue
            .subscribe_activity(turn_state.as_deref())
            .await;

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
        let targets = args.targets.unwrap_or_default();
        let mut outcome = wait_for_activity(&mut activity_rx, pending_activity, deadline).await;
        // FORK: mail from an agent the caller did not ask about is not the event
        // it was waiting for. Keep waiting until the deadline instead of
        // returning and making it call again.
        while outcome == WaitOutcome::MailboxActivity
            && !targets.is_empty()
            && !mailbox_matches_targets(&session, &targets).await
        {
            outcome =
                wait_for_activity(&mut activity_rx, /*pending_activity*/ None, deadline).await;
        }
        let agents = live_agent_snapshots(&session, &turn).await;
        let result =
            WaitAgentResult::from_outcome(outcome, requested_timeout_ms, timeout_ms, agents);

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

/// FORK: whether any waiting mail is from one of the named agents.
///
/// Matching is by prefix so `targets: ["/root/explorer"]` also wakes for that
/// agent's own children, which report through it.
async fn mailbox_matches_targets(
    session: &crate::session::session::Session,
    targets: &[String],
) -> bool {
    let authors = session.input_queue.pending_mailbox_authors().await;
    authors.iter().any(|author| {
        targets
            .iter()
            .any(|target| author == target || author.starts_with(&format!("{target}/")))
    })
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
}

#[derive(Debug, Deserialize, Serialize, PartialEq, Eq)]
pub(crate) struct WaitAgentResult {
    pub(crate) message: String,
    pub(crate) timed_out: bool,
    /// FORK: what each live agent was last observed doing.
    ///
    /// A bare "wait timed out" told the parent nothing, so it interrupted on a
    /// hunch. This is the line that distinguishes a child running `cargo test`
    /// from one that has genuinely stopped.
    pub(crate) agents: Vec<WaitAgentSnapshot>,
}

#[derive(Debug, Deserialize, Serialize, PartialEq, Eq)]
pub(crate) struct WaitAgentSnapshot {
    pub(crate) agent_name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) last_activity: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) idle_seconds: Option<u64>,
}

impl WaitAgentResult {
    fn from_outcome(
        outcome: WaitOutcome,
        requested_timeout_ms: Option<i64>,
        timeout_ms: i64,
        agents: Vec<WaitAgentSnapshot>,
    ) -> Self {
        let message = match outcome {
            WaitOutcome::MailboxActivity => "Wait completed.",
            WaitOutcome::Steered => "Wait interrupted by new input.",
            WaitOutcome::TimedOut => "Wait timed out.",
        };
        let message = match requested_timeout_ms {
            Some(requested_timeout_ms) if requested_timeout_ms < timeout_ms => format!(
                "{message}\n\nRequested timeout of {requested_timeout_ms}ms was clamped to the minimum of {timeout_ms}ms."
            ),
            Some(_) | None => message.to_string(),
        };
        // On a timeout, say what the agents are actually doing rather than
        // leaving the parent to guess.
        let message = if outcome == WaitOutcome::TimedOut && !agents.is_empty() {
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
            timed_out: outcome == WaitOutcome::TimedOut,
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
enum WaitOutcome {
    MailboxActivity,
    Steered,
    TimedOut,
}

async fn wait_for_activity(
    activity_rx: &mut tokio::sync::watch::Receiver<InputQueueActivity>,
    pending_activity: Option<InputQueueActivity>,
    deadline: Instant,
) -> WaitOutcome {
    if let Some(activity) = pending_activity {
        return match activity {
            InputQueueActivity::Mailbox => WaitOutcome::MailboxActivity,
            InputQueueActivity::Steer => WaitOutcome::Steered,
        };
    }
    match timeout_at(deadline, activity_rx.changed()).await {
        Ok(Ok(())) => match *activity_rx.borrow_and_update() {
            InputQueueActivity::Mailbox => WaitOutcome::MailboxActivity,
            InputQueueActivity::Steer => WaitOutcome::Steered,
        },
        Ok(Err(_)) | Err(_) => WaitOutcome::TimedOut,
    }
}
