//! Bounded mailbox values and typed coordination errors.

use anyhow::Result;
use anyhow::bail;
use serde::Deserialize;
use serde::Serialize;
use serde_json::Value;
use std::fmt;

use super::types::*;

pub const DEFAULT_WORKFLOW_MAILBOX_CAPACITY: u32 = 100;
pub(super) const MAX_MAILBOX_LEASE_MS: i64 = 86_400_000;

/// Independent delivery channels. Capacity is scoped by `(root_run_id,
/// channel)`, while the delivery sequence remains monotonic per recipient.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum WorkflowMailboxChannel {
    Data,
    Control,
}

impl WorkflowMailboxChannel {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Data => "data",
            Self::Control => "control",
        }
    }

    pub(super) fn from_str(value: &str) -> Result<Self> {
        match value {
            "data" => Ok(Self::Data),
            "control" => Ok(Self::Control),
            _ => bail!("unknown mailbox channel: {value}"),
        }
    }
}

/// Durable mailbox lifecycle state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum WorkflowMailboxState {
    Pending,
    Delivering,
    Delivered,
}

impl WorkflowMailboxState {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Delivering => "delivering",
            Self::Delivered => "delivered",
        }
    }

    pub(super) fn from_str(value: &str) -> Result<Self> {
        match value {
            "pending" => Ok(Self::Pending),
            "delivering" => Ok(Self::Delivering),
            "delivered" => Ok(Self::Delivered),
            _ => bail!("unknown mailbox state: {value}"),
        }
    }
}

/// Input for one durable mailbox message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkflowMailboxMessageCreate {
    pub message_id: String,
    pub root_run_id: String,
    pub sender_run_id: String,
    pub recipient_run_id: String,
    pub channel: WorkflowMailboxChannel,
    pub payload: Value,
    pub created_at_ms: Option<i64>,
}

/// One mailbox row, including its current fencing generation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkflowMailboxMessage {
    pub message_id: String,
    pub root_run_id: String,
    pub sender_run_id: String,
    pub recipient_run_id: String,
    pub sequence: i64,
    pub channel: WorkflowMailboxChannel,
    pub state: WorkflowMailboxState,
    pub payload: Value,
    pub created_at_ms: i64,
    pub claim_owner: Option<String>,
    pub claim_expires_at_ms: Option<i64>,
    pub acked_at_ms: Option<i64>,
    /// When the message content was durably applied to the recipient.  Set
    /// under the claim fence before acknowledgement; survives requeues so a
    /// crash between apply and ack cannot re-apply the content.
    pub applied_at_ms: Option<i64>,
    pub generation: i64,
}

/// Request to claim the oldest pending message in one recipient/channel queue.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkflowMailboxClaimRequest {
    pub recipient_run_id: String,
    pub channel: WorkflowMailboxChannel,
    pub owner: String,
    pub lease_duration_ms: i64,
}

impl WorkflowMailboxClaimRequest {
    pub fn new(
        recipient_run_id: impl Into<String>,
        channel: WorkflowMailboxChannel,
        owner: impl Into<String>,
        lease_duration_ms: i64,
    ) -> Self {
        Self {
            recipient_run_id: recipient_run_id.into(),
            channel,
            owner: owner.into(),
            lease_duration_ms,
        }
    }
}

/// Fencing data returned with a successful claim.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkflowMailboxClaim {
    pub message: WorkflowMailboxMessage,
    pub owner: String,
    pub token: String,
    pub generation: i64,
    pub lease_expires_at_ms: i64,
}

/// Acknowledge one claimed message with its fencing data.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkflowMailboxAckRequest {
    pub message_id: String,
    pub owner: String,
    pub token: String,
    pub generation: i64,
}

/// Bounded pending-message listing request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkflowMailboxListRequest {
    pub recipient_run_id: String,
    pub channel: WorkflowMailboxChannel,
    pub limit: u32,
}

impl WorkflowMailboxListRequest {
    pub fn new(
        recipient_run_id: impl Into<String>,
        channel: WorkflowMailboxChannel,
        limit: u32,
    ) -> Result<Self> {
        let recipient_run_id = recipient_run_id.into();
        validate_text(&recipient_run_id, MAX_ID_BYTES, "mailbox recipient run id")?;
        if recipient_run_id.contains('\0') {
            bail!("mailbox recipient run id must not contain NUL");
        }
        validate_page_size(limit)?;
        Ok(Self {
            recipient_run_id,
            channel,
            limit,
        })
    }
}

/// Stable typed errors for mailbox coordination failures.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WorkflowMailboxError {
    Backpressured { depth: u32, capacity: u32 },
    Conflict { message_id: String },
    Missing { message_id: String },
    StaleClaim { message_id: String },
}

impl fmt::Display for WorkflowMailboxError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Backpressured { depth, capacity } => {
                write!(
                    formatter,
                    "mailbox backpressured: depth {depth}, capacity {capacity}"
                )
            }
            Self::Conflict { message_id } => {
                write!(
                    formatter,
                    "mailbox message {message_id} conflicts with existing content"
                )
            }
            Self::Missing { message_id } => {
                write!(formatter, "mailbox message {message_id} does not exist")
            }
            Self::StaleClaim { message_id } => {
                write!(formatter, "mailbox claim for message {message_id} is stale")
            }
        }
    }
}

impl std::error::Error for WorkflowMailboxError {}
