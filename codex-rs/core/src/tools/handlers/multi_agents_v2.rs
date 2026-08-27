//! Implements the MultiAgentV2 collaboration tool surface.

use crate::agent::AgentStatus;
use crate::agent::agent_resolver::resolve_agent_target;
use crate::context::ContextualUserFragment;
use crate::context::InterAgentMessage;
use crate::context::InterAgentMessageType;
use crate::function_tool::FunctionCallError;
use crate::tools::context::ToolInvocation;
use crate::tools::context::ToolOutput;
use crate::tools::context::ToolPayload;
use crate::tools::context::boxed_tool_output;
use crate::tools::handlers::multi_agents_common::*;
use crate::tools::handlers::parse_arguments;
use crate::tools::registry::CoreToolRuntime;
use crate::tools::registry::ToolExecutor;
use codex_protocol::AgentPath;
use codex_protocol::items::CollabAgentTool;
use codex_protocol::items::CollabAgentToolCallItem;
use codex_protocol::items::CollabAgentToolCallStatus;
use codex_protocol::items::SubAgentActivityItem;
use codex_protocol::items::TurnItem;
use codex_protocol::models::ResponseInputItem;
use codex_protocol::openai_models::ReasoningEffort;
use codex_protocol::protocol::InterAgentCommunication;
use codex_protocol::protocol::SubAgentActivityKind;
use codex_tools::ToolName;
use serde::Deserialize;
use serde::Serialize;
use serde_json::Value as JsonValue;

pub(crate) use followup_task::Handler as FollowupTaskHandler;
pub(crate) use interrupt_agent::Handler as InterruptAgentHandler;
pub(crate) use list_agents::Handler as ListAgentsHandler;
pub(crate) use send_message::Handler as SendMessageHandler;
pub(crate) use spawn::Handler as SpawnAgentHandler;
pub(crate) use wait::Handler as WaitAgentHandler;

mod analytics;
mod followup_task;
mod interrupt_agent;
mod list_agents;
mod message_tool;
mod send_message;
mod spawn;
pub(crate) mod wait;

pub(crate) async fn emit_sub_agent_activity(
    session: &crate::session::session::Session,
    turn: &crate::session::turn_context::TurnContext,
    item: SubAgentActivityItem,
) {
    let item = TurnItem::SubAgentActivity(item);
    session.emit_turn_item_started(turn, &item).await;
    session.emit_turn_item_completed(turn, item).await;
}

/// Picks the message argument the caller actually filled.
///
/// The tools expose two: `message`, which the schema marks as encrypted, and
/// `plaintext_message` for agents whose backend holds no decryption key.
pub(crate) fn tool_message_argument(
    message: Option<String>,
    plaintext_message: Option<String>,
    tool_name: &str,
) -> Result<(String, ToolMessageForm), FunctionCallError> {
    match (message, plaintext_message) {
        (Some(_), Some(_)) => Err(FunctionCallError::RespondToModel(format!(
            "{tool_name} takes either message or plaintext_message, not both"
        ))),
        (None, None) => Err(FunctionCallError::RespondToModel(format!(
            "{tool_name} requires message (or plaintext_message for agents that cannot decrypt it)"
        ))),
        (Some(message), None) => Ok((non_empty_message(message)?, ToolMessageForm::Encrypted)),
        (None, Some(message)) => Ok((non_empty_message(message)?, ToolMessageForm::Plaintext)),
    }
}

fn non_empty_message(message: String) -> Result<String, FunctionCallError> {
    if message.trim().is_empty() {
        return Err(FunctionCallError::RespondToModel(
            "Empty message can't be sent to an agent".to_string(),
        ));
    }
    Ok(message)
}

/// Rejects an encrypted payload aimed at an agent that cannot read one.
///
/// The encryption key lives with the model backend, so a locally served agent
/// would receive an opaque blob and stall. Failing here instead lets the caller
/// resend the same task through `plaintext_message`.
pub(crate) fn require_readable_message_form(
    config: &crate::config::Config,
    form: ToolMessageForm,
    tool_name: &str,
) -> Result<(), FunctionCallError> {
    // FORK: `chatgpt_web` children cannot decrypt either.
    if form == ToolMessageForm::Plaintext
        || !matches!(
            config.model_provider.wire_api,
            codex_model_provider_info::WireApi::ClaudeCode
                | codex_model_provider_info::WireApi::ChatGptWeb
        )
    {
        return Ok(());
    }
    Err(FunctionCallError::RespondToModel(format!(
        "This agent runs on a local backend that cannot decrypt `message`. \
Call {tool_name} again with the same task in `plaintext_message` instead."
    )))
}

/// How the caller supplied an inter-agent message.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum ToolMessageForm {
    /// The tool's `message` argument, which the schema marks as encrypted. Only
    /// the model backend can read it back.
    Encrypted,
    /// The tool's `plaintext_message` argument, used for agents served by a
    /// backend that has no way to decrypt the payload.
    Plaintext,
}

fn communication_from_tool_message(
    author: AgentPath,
    recipient: AgentPath,
    message: String,
    source: &crate::tools::context::ToolCallSource,
    form: ToolMessageForm,
    trigger_turn: bool,
) -> InterAgentCommunication {
    if form == ToolMessageForm::Encrypted
        && !matches!(
            source,
            crate::tools::context::ToolCallSource::DirectPlaintextMessage
        )
    {
        return InterAgentCommunication::new_encrypted(
            author,
            recipient,
            Vec::new(),
            message,
            trigger_turn,
        );
    }
    let message_type = if trigger_turn {
        InterAgentMessageType::NewTask
    } else {
        InterAgentMessageType::Message
    };
    let content =
        InterAgentMessage::new(message_type, recipient.clone(), author.clone(), message).render();
    InterAgentCommunication::new(author, recipient, Vec::new(), content, trigger_turn)
}
