//! Durable inter-agent mailbox admission and delivery helpers.
//!
//! The workflow store owns message order and fencing. Core owns the small
//! wire envelope that carries an InterAgentCommunication together with the
//! explicit turn-start lineage needed when a follow-up wakes a recipient.

use crate::session::session::Session;
use codex_history::RolloutItem;
use codex_protocol::ResponseItemId;
use codex_protocol::models::ResponseItem;
use codex_protocol::protocol::InterAgentCommunication;
use codex_protocol::turn_input::CyberAccessProgram;
use codex_protocol::turn_input::TurnStartOptions;
use codex_state::WorkflowMailboxAckRequest;
use codex_state::WorkflowMailboxChannel;
use codex_state::WorkflowMailboxClaim;
use codex_state::WorkflowMailboxClaimRequest;
use codex_state::WorkflowMailboxMessageCreate;
use codex_state::WorkflowStore;
use codex_thread_store::LiveThread;
use serde::Deserialize;
use serde::Serialize;
use serde_json::Value;
use tracing::warn;
use uuid::Uuid;

pub(crate) const MAILBOX_CLAIM_LEASE_MS: i64 = 5 * 60 * 1_000;

pub(crate) struct ClaimedCommunication {
    pub claim: WorkflowMailboxClaim,
    pub communication: InterAgentCommunication,
    pub start_options: TurnStartOptions,
}

/// One claimed mailbox row, decoded when possible.
///
/// An undecodable payload keeps its claim so the row can be dead-lettered
/// (acknowledged without application) instead of poisoning the head of the
/// recipient's queue — an unackable head blocks every later message and
/// eventually backpressures the sender at queue capacity.
pub(crate) enum ClaimedDelivery {
    Decoded(ClaimedCommunication),
    Undecodable {
        claim: WorkflowMailboxClaim,
        error: String,
    },
}

/// Result of one `deliver_pending` call.
#[derive(Debug, Clone, Default)]
pub(crate) struct DeliverPendingOutcome {
    /// Whether any delivered message or wake asked for a new turn.
    pub(crate) has_trigger_turn: bool,
    /// Rows that were dead-lettered because their payload did not decode.
    /// The op handler uses this to fall back to the op's own copy of the
    /// content when the poisoned row is the op's message.
    pub(crate) undecodable_message_ids: Vec<String>,
}

/// Canonical delivery facts for one mailbox UUID.
///
/// The content and trigger metadata are read from the rollout rather than
/// inferred from the in-memory input queue. `wake_applied` is an append-only
/// receipt used to make a redelivery idempotent across process restarts.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct CanonicalMailboxState {
    pub(crate) content_present: bool,
    pub(crate) trigger_turn: bool,
    pub(crate) wake_applied: bool,
}

/// Result of reconciling one claimed mailbox message with canonical history.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct DurableMailboxPersistence {
    pub(crate) already_persisted: bool,
    pub(crate) trigger_turn: bool,
    pub(crate) wake_applied: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct DurableMailboxPayload {
    communication: InterAgentCommunication,
    start_options: DurableTurnStartOptions,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct DurableTurnStartOptions {
    #[serde(default)]
    turn_trigger: Option<String>,
    #[serde(default)]
    final_output_json_schema: Option<Value>,
    #[serde(default)]
    service_tier: Option<String>,
    #[serde(default)]
    parent_turn_id: Option<String>,
    #[serde(default)]
    root_turn_id: Option<String>,
    #[serde(default)]
    cyber_access_program: Option<CyberAccessProgram>,
}

impl From<&TurnStartOptions> for DurableTurnStartOptions {
    fn from(value: &TurnStartOptions) -> Self {
        Self {
            turn_trigger: value.turn_trigger.clone(),
            final_output_json_schema: value.final_output_json_schema.clone(),
            service_tier: value.service_tier.clone(),
            parent_turn_id: value.parent_turn_id.clone(),
            root_turn_id: value.root_turn_id.clone(),
            cyber_access_program: value.cyber_access_program,
        }
    }
}

impl From<DurableTurnStartOptions> for TurnStartOptions {
    fn from(value: DurableTurnStartOptions) -> Self {
        Self {
            // A Guardian ticket is a receipt for one live server response and is
            // never persisted, so a rehydrated mailbox turn carries none.
            guardian_ticket: None,
            turn_trigger: value.turn_trigger,
            final_output_json_schema: value.final_output_json_schema,
            service_tier: value.service_tier,
            parent_turn_id: value.parent_turn_id,
            root_turn_id: value.root_turn_id,
            cyber_access_program: value.cyber_access_program,
        }
    }
}

pub(crate) fn ensure_message_id(communication: &mut InterAgentCommunication) -> String {
    if let Some(id) = communication.id.as_ref() {
        return id.to_string();
    }
    let message_id = Uuid::now_v7().to_string();
    communication.id = Some(ResponseItemId::from_server(message_id.clone()));
    message_id
}

pub(crate) fn message_id(communication: &InterAgentCommunication) -> Option<String> {
    communication.id.as_ref().map(ToString::to_string)
}

pub(crate) fn payload(
    communication: &InterAgentCommunication,
    start_options: &TurnStartOptions,
) -> Result<Value, serde_json::Error> {
    serde_json::to_value(DurableMailboxPayload {
        communication: communication.clone(),
        start_options: DurableTurnStartOptions::from(start_options),
    })
}

pub(crate) fn decode_payload(
    payload: Value,
    message_id: &str,
) -> Result<(InterAgentCommunication, TurnStartOptions), String> {
    let DurableMailboxPayload {
        mut communication,
        start_options,
    } = serde_json::from_value(payload)
        .map_err(|_| "durable mailbox payload could not be decoded".to_string())?;
    let decoded_id = communication
        .id
        .as_ref()
        .map(ToString::to_string)
        .ok_or_else(|| "durable mailbox payload is missing its message id".to_string())?;
    if decoded_id != message_id {
        return Err("durable mailbox payload message id does not match its row".to_string());
    }
    communication.id = Some(ResponseItemId::from_server(message_id.to_string()));
    Ok((communication, start_options.into()))
}

/// Check the in-memory model context for a mailbox UUID.
///
/// This is the best-effort dedupe for threads whose canonical history the
/// store cannot read back (Paginated history mode, unmaterialized rollouts):
/// a resumed session rebuilds its model context from persisted history before
/// mailbox rehydration runs, so a previously applied communication shows up
/// here as the `AgentMessage` recorded at consumption time.
pub(crate) fn model_context_contains_mailbox_id<'a>(
    items: impl Iterator<Item = &'a ResponseItem>,
    message_id: &str,
) -> bool {
    let mut items = items;
    items.any(|item| match item {
        ResponseItem::AgentMessage { id: Some(id), .. } => id.as_str() == message_id,
        ResponseItem::Message { role, content, .. } if role == "assistant" => {
            InterAgentCommunication::from_message_content(content)
                .and_then(|communication| communication.id)
                .is_some_and(|id| id.as_str() == message_id)
        }
        _ => false,
    })
}

/// Read the canonical rollout facts for one mailbox UUID.
///
/// Callers must treat an `Err` as "unknown": Paginated threads reject full
/// history reads by design and unmaterialized threads have no rollout yet.
pub(crate) async fn canonical_mailbox_state(
    live_thread: &LiveThread,
    message_id: &str,
) -> anyhow::Result<CanonicalMailboxState> {
    let history = live_thread.load_history(true).await?;
    Ok(history_mailbox_state(&history.items, message_id))
}

fn history_mailbox_state(history: &[RolloutItem], message_id: &str) -> CanonicalMailboxState {
    let mut state = CanonicalMailboxState::default();
    for item in history {
        match item {
            RolloutItem::InterAgentCommunication(communication)
                if communication
                    .id
                    .as_ref()
                    .is_some_and(|id| id.as_str() == message_id) =>
            {
                state.content_present = true;
                state.trigger_turn = communication.trigger_turn;
            }
            RolloutItem::ResponseItem(response_item) => match &response_item.item {
                ResponseItem::AgentMessage { id: Some(id), .. } if id.as_str() == message_id => {
                    state.content_present = true;
                }
                ResponseItem::Message { role, content, .. } if role == "assistant" => {
                    if let Some(communication) =
                        InterAgentCommunication::from_message_content(content)
                        && communication
                            .id
                            .as_ref()
                            .is_some_and(|id| id.as_str() == message_id)
                    {
                        state.content_present = true;
                        state.trigger_turn = communication.trigger_turn;
                    }
                }
                _ => {}
            },
            RolloutItem::InterAgentCommunicationMetadata {
                message_id: Some(metadata_message_id),
                trigger_turn,
                wake_applied,
            } if metadata_message_id.as_str() == message_id => {
                state.trigger_turn = *trigger_turn;
                state.wake_applied = *wake_applied;
            }
            RolloutItem::SessionMeta(_)
            | RolloutItem::InterAgentCommunication(_)
            | RolloutItem::InterAgentCommunicationMetadata { .. }
            | RolloutItem::Compacted(_)
            | RolloutItem::TurnContext(_)
            | RolloutItem::WorldState(_)
            | RolloutItem::SecurityRiskScore(_)
            | RolloutItem::EventMsg(_)
            | RolloutItem::RealtimeItem(_)
            | RolloutItem::RetainedContext(_)
            | RolloutItem::TokenUsageRecord(_) => {}
        }
    }
    state
}

#[cfg(test)]
fn history_contains_mailbox_id(history: &[RolloutItem], message_id: &str) -> bool {
    history_mailbox_state(history, message_id).content_present
}

pub(crate) async fn enqueue(
    workflow: &WorkflowStore,
    root_run_id: String,
    sender_run_id: String,
    recipient_run_id: String,
    communication: &InterAgentCommunication,
    start_options: &TurnStartOptions,
) -> Result<(), anyhow::Error> {
    workflow
        .enqueue_mailbox_message(&WorkflowMailboxMessageCreate {
            message_id: ensure_existing_message_id(communication)?,
            root_run_id,
            sender_run_id,
            recipient_run_id,
            channel: WorkflowMailboxChannel::Data,
            payload: payload(communication, start_options)?,
            created_at_ms: None,
        })
        .await
        .map(|_| ())
}

fn ensure_existing_message_id(
    communication: &InterAgentCommunication,
) -> Result<String, anyhow::Error> {
    message_id(communication)
        .ok_or_else(|| anyhow::anyhow!("inter-agent communication is missing its message id"))
}

pub(crate) fn new_claim_owner(recipient_run_id: &str) -> String {
    format!("core-mailbox:{recipient_run_id}:{}", Uuid::now_v7())
}

pub(crate) async fn claim_data(
    workflow: &WorkflowStore,
    recipient_run_id: &str,
    owner: String,
) -> Result<Option<WorkflowMailboxClaim>, anyhow::Error> {
    workflow
        .claim_mailbox_message(&WorkflowMailboxClaimRequest::new(
            recipient_run_id,
            WorkflowMailboxChannel::Data,
            owner,
            MAILBOX_CLAIM_LEASE_MS,
        ))
        .await
}

pub(crate) async fn claim_next_for_session(
    session: &Session,
) -> Result<Option<ClaimedDelivery>, anyhow::Error> {
    let Some(state_db) = session.state_db() else {
        return Ok(None);
    };
    let workflow = state_db.workflow_store().clone();
    let recipient_run_id = session.thread_id.to_string();
    let owner = new_claim_owner(&recipient_run_id);
    let Some(claim) = claim_data(&workflow, &recipient_run_id, owner).await? else {
        return Ok(None);
    };
    match decode_payload(claim.message.payload.clone(), &claim.message.message_id) {
        Ok((communication, start_options)) => {
            Ok(Some(ClaimedDelivery::Decoded(ClaimedCommunication {
                claim,
                communication,
                start_options,
            })))
        }
        Err(error) => Ok(Some(ClaimedDelivery::Undecodable { claim, error })),
    }
}

pub(crate) async fn durable_message_channel_for_session(
    session: &Session,
    message_id: &str,
) -> Result<Option<WorkflowMailboxChannel>, anyhow::Error> {
    let Some(state_db) = session.state_db() else {
        return Ok(None);
    };
    let Some(message) = state_db
        .workflow_store()
        .get_mailbox_message(message_id)
        .await?
    else {
        return Ok(None);
    };
    if message.recipient_run_id != session.thread_id.to_string() {
        return Ok(None);
    }
    Ok(Some(message.channel))
}

pub(crate) async fn acknowledge_for_session(
    session: &Session,
    claim: &WorkflowMailboxClaim,
) -> Result<(), anyhow::Error> {
    let Some(state_db) = session.state_db() else {
        return Ok(());
    };
    acknowledge(&state_db.workflow_store().clone(), claim).await
}

/// Record the durable apply receipt for a claimed message under its fence.
pub(crate) async fn mark_applied_for_session(
    session: &Session,
    claim: &WorkflowMailboxClaim,
) -> Result<(), anyhow::Error> {
    let Some(state_db) = session.state_db() else {
        return Ok(());
    };
    state_db
        .workflow_store()
        .mark_mailbox_applied(&WorkflowMailboxAckRequest {
            message_id: claim.message.message_id.clone(),
            owner: claim.owner.clone(),
            token: claim.token.clone(),
            generation: claim.generation,
        })
        .await
        .map(|_| ())
}

pub(crate) async fn reclaim_expired_for_session(session: &Session) -> Result<(), anyhow::Error> {
    let Some(state_db) = session.state_db() else {
        return Ok(());
    };
    state_db
        .workflow_store()
        .reclaim_expired_mailbox_for_recipient(
            &session.thread_id.to_string(),
            chrono::Utc::now().timestamp_millis(),
        )
        .await
        .map(|_| ())
}

/// Claims, applies, queues and acknowledges every pending data message for a
/// session.
///
/// The workflow row is the delivery authority: its `applied_at_ms` receipt,
/// written under the claim fence before acknowledgement, is what makes a
/// redelivery skip re-applying content. Rollout persistence and canonical
/// reads are best-effort (`Session::apply_durable_mailbox_content`) and can
/// never block delivery. Acknowledgement failures are logged, never
/// propagated: a stale ack (the lease expired mid-processing) triggers one
/// bounded second pass, where the reclaimed rows short-circuit on their apply
/// receipt and are re-acked under a fresh lease.
pub(crate) async fn deliver_pending(
    session: &std::sync::Arc<Session>,
) -> Result<DeliverPendingOutcome, anyhow::Error> {
    let mut outcome = DeliverPendingOutcome::default();
    if session.state_db().is_none() {
        return Ok(outcome);
    }
    for _attempt in 0..2 {
        let pass = deliver_pending_once(session).await?;
        outcome.has_trigger_turn |= pass.outcome.has_trigger_turn;
        outcome
            .undecodable_message_ids
            .extend(pass.outcome.undecodable_message_ids);
        if !pass.saw_stale_ack {
            break;
        }
    }
    Ok(outcome)
}

struct DeliverPass {
    outcome: DeliverPendingOutcome,
    /// An acknowledgement failed its fence (typically an expired lease).
    saw_stale_ack: bool,
}

async fn deliver_pending_once(
    session: &std::sync::Arc<Session>,
) -> Result<DeliverPass, anyhow::Error> {
    reclaim_expired_for_session(session).await?;
    let turn_context = session.new_default_turn().await;
    let mut outcome = DeliverPendingOutcome::default();
    let mut claims_to_acknowledge = Vec::new();
    loop {
        let Some(claimed) = claim_next_for_session(session).await? else {
            break;
        };
        let claimed = match claimed {
            ClaimedDelivery::Decoded(claimed) => claimed,
            ClaimedDelivery::Undecodable { claim, error } => {
                // Dead-letter: acknowledge so the poisoned row cannot block
                // the queue head or backpressure the sender forever. The op
                // handler falls back to the op's own copy of the content.
                warn!(
                    message_id = %claim.message.message_id,
                    "dead-lettering undecodable mailbox row: {error}"
                );
                outcome
                    .undecodable_message_ids
                    .push(claim.message.message_id.clone());
                claims_to_acknowledge.push(claim);
                continue;
            }
        };
        session
            .services
            .agent_control
            .record_agent_message(&claimed.communication.author);
        // `decode_payload` guarantees the communication carries the row's
        // message id.
        let message_id = claimed.claim.message.message_id.clone();
        let already_applied = claimed.claim.message.applied_at_ms.is_some();
        if already_applied {
            // A crash landed between the apply receipt and the ack: the
            // content already reached this recipient. Re-arm the wake only.
            if claimed.communication.trigger_turn {
                session
                    .input_queue
                    .enqueue_mailbox_wake(message_id.clone(), claimed.start_options)
                    .await;
                outcome.has_trigger_turn = true;
            }
            claims_to_acknowledge.push(claimed.claim);
            continue;
        }
        let persistence = session
            .apply_durable_mailbox_content(&turn_context, &claimed.communication)
            .await;
        let trigger_turn = persistence.trigger_turn || claimed.communication.trigger_turn;
        if !persistence.already_persisted {
            if session.input_queue.note_mailbox_enqueued(&message_id).await {
                session
                    .input_queue
                    .enqueue_mailbox_communication(
                        claimed.communication,
                        claimed.start_options.clone(),
                    )
                    .await;
            }
            outcome.has_trigger_turn |= trigger_turn;
        } else if trigger_turn && !persistence.wake_applied {
            session
                .input_queue
                .enqueue_mailbox_wake(message_id.clone(), claimed.start_options)
                .await;
            outcome.has_trigger_turn = true;
        }
        // Write the durable apply receipt while the claim is still fenced. A
        // failure here surfaces again as a stale ack below and is healed by
        // the retry pass, so it is not fatal on its own.
        if let Err(error) = mark_applied_for_session(session, &claimed.claim).await {
            warn!(
                message_id = %message_id,
                "failed to record mailbox apply receipt: {error}"
            );
        }
        claims_to_acknowledge.push(claimed.claim);
    }
    // Start the wake while the claims are still fenced: a crash before the
    // acks leaves the rows delivering, and the next session's requeue +
    // redelivery re-arms the wake off the apply receipts.
    if outcome.has_trigger_turn {
        session.maybe_start_turn_for_pending_work().await;
    }
    let mut saw_stale_ack = false;
    for claim in claims_to_acknowledge {
        if let Err(error) = acknowledge_for_session(session, &claim).await {
            match error.downcast_ref::<codex_state::WorkflowMailboxError>() {
                Some(codex_state::WorkflowMailboxError::StaleClaim { .. }) => {
                    saw_stale_ack = true;
                    warn!(
                        message_id = %claim.message.message_id,
                        "mailbox ack lost its claim fence; retrying via reclaim"
                    );
                }
                _ => {
                    warn!(
                        message_id = %claim.message.message_id,
                        "mailbox ack failed: {error}"
                    );
                }
            }
        }
    }
    Ok(DeliverPass {
        outcome,
        saw_stale_ack,
    })
}

pub(crate) async fn rehydrate(session: &std::sync::Arc<Session>) -> Result<bool, anyhow::Error> {
    if session.state_db().is_none() {
        return Ok(false);
    }
    requeue_undelivered_for_session(session).await?;
    deliver_pending(session)
        .await
        .map(|outcome| outcome.has_trigger_turn)
}

/// Fence claims left by a dead session before attempting delivery again.
///
/// The workflow store deliberately returns every non-delivered row to
/// `pending`, including claims whose lease has not elapsed.  Rehydration is a
/// process/session boundary, so retaining those claims would turn a crash into
/// an avoidable lease-duration outage.
pub(crate) async fn requeue_undelivered_for_session(
    session: &Session,
) -> Result<(), anyhow::Error> {
    let Some(state_db) = session.state_db() else {
        return Ok(());
    };
    state_db
        .workflow_store()
        .requeue_undelivered_mailbox(&session.thread_id.to_string())
        .await
        .map(|_| ())
}

pub(crate) async fn has_durable_pending(session: &Session) -> bool {
    let Some(state_db) = session.state_db() else {
        return false;
    };
    let workflow = state_db.workflow_store();
    // TODO: `mailbox_depth` buckets by `root_run_id`, but this passes the
    // recipient's thread id, so for child agents it measures the wrong
    // bucket (pre-existing; left as-is to keep this change reviewable).
    let recipient_run_id = session.thread_id.to_string();
    for channel in [
        WorkflowMailboxChannel::Data,
        WorkflowMailboxChannel::Control,
    ] {
        match workflow.mailbox_depth(&recipient_run_id, channel).await {
            Ok(depth) if depth > 0 => return true,
            Ok(_) => {}
            Err(_) => return true,
        }
    }
    false
}

pub(crate) async fn acknowledge(
    workflow: &WorkflowStore,
    claim: &WorkflowMailboxClaim,
) -> Result<(), anyhow::Error> {
    workflow
        .ack_mailbox_message(&WorkflowMailboxAckRequest {
            message_id: claim.message.message_id.clone(),
            owner: claim.owner.clone(),
            token: claim.token.clone(),
            generation: claim.generation,
        })
        .await
        .map(|_| ())
}

#[cfg(test)]
#[path = "mailbox_tests.rs"]
mod tests;
