//! Crash-safe workflow mailbox delivery.
//!
//! Mailbox rows are durable coordination records.  A message is written before
//! its sender receives an acknowledgement, claims are leased and fenced by a
//! generation/token pair, and reclaiming an expired claim only returns it to
//! `pending`; no retry is scheduled by this store.

use anyhow::Result;
use anyhow::bail;
use sqlx::Row;
use sqlx::Sqlite;
use uuid::Uuid;

use super::WorkflowStore;
use super::mailbox_types::*;
use super::types::*;

pub(super) const MAILBOX_COLUMNS: &str = "message_id, root_run_id, sender_run_id,
    recipient_run_id, sequence, channel, state, payload_json, created_at_ms,
    claim_owner, claim_token, claim_expires_at_ms, acked_at_ms, applied_at_ms,
    generation";

impl WorkflowStore {
    /// Persist one message before returning.  Reusing a message ID with equal
    /// immutable content is idempotent; a divergent payload or routing tuple
    /// is a typed conflict and never consumes queue capacity.
    pub async fn insert_mailbox_message(
        &self,
        input: &WorkflowMailboxMessageCreate,
    ) -> Result<WorkflowMailboxMessage> {
        let payload_json = validate_message_create(input)?;
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        let existing = sqlx::query(sqlx::AssertSqlSafe(format!(
            "SELECT {MAILBOX_COLUMNS} FROM workflow_mailbox WHERE message_id = ?"
        )))
        .bind(&input.message_id)
        .fetch_optional(&mut *tx)
        .await?;
        if let Some(row) = existing {
            let message = mailbox_from_row(&row)?;
            if message_matches_input(&message, input) {
                tx.commit().await?;
                return Ok(message);
            }
            tx.rollback().await?;
            return Err(anyhow::Error::new(WorkflowMailboxError::Conflict {
                message_id: input.message_id.clone(),
            }));
        }

        let depth = mailbox_depth_in_tx(&mut tx, &input.root_run_id, input.channel).await?;
        if depth >= i64::from(DEFAULT_WORKFLOW_MAILBOX_CAPACITY) {
            tx.rollback().await?;
            return Err(anyhow::Error::new(WorkflowMailboxError::Backpressured {
                depth: depth_to_u32(depth)?,
                capacity: DEFAULT_WORKFLOW_MAILBOX_CAPACITY,
            }));
        }
        let sequence = sqlx::query_scalar::<_, i64>(
            "SELECT COALESCE(MAX(sequence), -1) + 1
             FROM workflow_mailbox WHERE recipient_run_id = ?",
        )
        .bind(&input.recipient_run_id)
        .fetch_one(&mut *tx)
        .await?;
        let created_at_ms = input.created_at_ms.unwrap_or_else(now_ms);
        sqlx::query(
            "INSERT INTO workflow_mailbox
             (message_id, root_run_id, sender_run_id, recipient_run_id, sequence,
              channel, state, payload_json, created_at_ms, generation)
             VALUES (?, ?, ?, ?, ?, ?, 'pending', ?, ?, 0)",
        )
        .bind(&input.message_id)
        .bind(&input.root_run_id)
        .bind(&input.sender_run_id)
        .bind(&input.recipient_run_id)
        .bind(sequence)
        .bind(input.channel.as_str())
        .bind(&payload_json)
        .bind(created_at_ms)
        .execute(&mut *tx)
        .await?;
        let row = sqlx::query(sqlx::AssertSqlSafe(format!(
            "SELECT {MAILBOX_COLUMNS} FROM workflow_mailbox WHERE message_id = ?"
        )))
        .bind(&input.message_id)
        .fetch_one(&mut *tx)
        .await?;
        let message = mailbox_from_row(&row)?;
        tx.commit().await?;
        Ok(message)
    }

    /// Alias used by callers that describe insertion as enqueueing.
    pub async fn enqueue_mailbox_message(
        &self,
        input: &WorkflowMailboxMessageCreate,
    ) -> Result<WorkflowMailboxMessage> {
        self.insert_mailbox_message(input).await
    }

    /// Read one message by its durable message ID.
    pub async fn get_mailbox_message(
        &self,
        message_id: &str,
    ) -> Result<Option<WorkflowMailboxMessage>> {
        validate_mailbox_id(message_id, "mailbox message id")?;
        sqlx::query(sqlx::AssertSqlSafe(format!(
            "SELECT {MAILBOX_COLUMNS} FROM workflow_mailbox WHERE message_id = ?"
        )))
        .bind(message_id)
        .fetch_optional(self.pool.as_ref())
        .await?
        .as_ref()
        .map(mailbox_from_row)
        .transpose()
    }

    /// Claim the oldest pending message for one recipient and channel.
    pub async fn claim_mailbox_message(
        &self,
        request: &WorkflowMailboxClaimRequest,
    ) -> Result<Option<WorkflowMailboxClaim>> {
        validate_mailbox_claim(request)?;
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        let row = sqlx::query(sqlx::AssertSqlSafe(format!(
            "SELECT {MAILBOX_COLUMNS} FROM workflow_mailbox
             WHERE recipient_run_id = ? AND channel = ? AND state = 'pending'
             ORDER BY sequence ASC LIMIT 1"
        )))
        .bind(&request.recipient_run_id)
        .bind(request.channel.as_str())
        .fetch_optional(&mut *tx)
        .await?;
        let Some(row) = row else {
            tx.commit().await?;
            return Ok(None);
        };
        let message = mailbox_from_row(&row)?;
        let generation = message
            .generation
            .checked_add(1)
            .ok_or_else(|| anyhow::anyhow!("mailbox claim generation overflow"))?;
        let now = now_ms();
        let expires_at_ms = now
            .checked_add(request.lease_duration_ms)
            .ok_or_else(|| anyhow::anyhow!("mailbox lease timestamp overflow"))?;
        let token = Uuid::new_v4().to_string();
        let claimed = sqlx::query(sqlx::AssertSqlSafe(format!(
            "UPDATE workflow_mailbox
             SET state = 'delivering', claim_owner = ?, claim_token = ?,
                 claim_expires_at_ms = ?, generation = ?
             WHERE message_id = ? AND state = 'pending'
             RETURNING {MAILBOX_COLUMNS}"
        )))
        .bind(&request.owner)
        .bind(&token)
        .bind(expires_at_ms)
        .bind(generation)
        .bind(&message.message_id)
        .fetch_optional(&mut *tx)
        .await?;
        let Some(row) = claimed else {
            tx.rollback().await?;
            return Err(anyhow::Error::new(WorkflowMailboxError::StaleClaim {
                message_id: message.message_id,
            }));
        };
        let message = mailbox_from_row(&row)?;
        tx.commit().await?;
        Ok(Some(WorkflowMailboxClaim {
            message,
            owner: request.owner.clone(),
            token,
            generation,
            lease_expires_at_ms: expires_at_ms,
        }))
    }

    /// Acknowledge a claim.  A repeated acknowledgement of a delivered row
    /// is idempotent, while pending/delivering rows require the exact fence.
    pub async fn ack_mailbox_message(
        &self,
        request: &WorkflowMailboxAckRequest,
    ) -> Result<WorkflowMailboxMessage> {
        validate_mailbox_ack(request)?;
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        let row = sqlx::query(sqlx::AssertSqlSafe(format!(
            "SELECT {MAILBOX_COLUMNS} FROM workflow_mailbox WHERE message_id = ?"
        )))
        .bind(&request.message_id)
        .fetch_optional(&mut *tx)
        .await?;
        let Some(row) = row else {
            tx.rollback().await?;
            return Err(anyhow::Error::new(WorkflowMailboxError::Missing {
                message_id: request.message_id.clone(),
            }));
        };
        let message = mailbox_from_row(&row)?;
        if message.state == WorkflowMailboxState::Delivered {
            tx.commit().await?;
            return Ok(message);
        }
        let claim_owner = row.try_get::<Option<String>, _>("claim_owner")?;
        let claim_token = row.try_get::<Option<String>, _>("claim_token")?;
        let now = now_ms();
        if message.state != WorkflowMailboxState::Delivering
            || claim_owner.as_deref() != Some(request.owner.as_str())
            || claim_token.as_deref() != Some(request.token.as_str())
            || message.generation != request.generation
            || message
                .claim_expires_at_ms
                .is_none_or(|expires_at_ms| expires_at_ms <= now)
        {
            tx.rollback().await?;
            return Err(anyhow::Error::new(WorkflowMailboxError::StaleClaim {
                message_id: request.message_id.clone(),
            }));
        }
        let acked_at_ms = now;
        let row = sqlx::query(sqlx::AssertSqlSafe(format!(
            "UPDATE workflow_mailbox
             SET state = 'delivered', claim_owner = NULL, claim_token = NULL,
                 claim_expires_at_ms = NULL, acked_at_ms = ?
             WHERE message_id = ? AND state = 'delivering'
               AND claim_owner = ? AND claim_token = ? AND generation = ?
             RETURNING {MAILBOX_COLUMNS}"
        )))
        .bind(acked_at_ms)
        .bind(&request.message_id)
        .bind(&request.owner)
        .bind(&request.token)
        .bind(request.generation)
        .fetch_optional(&mut *tx)
        .await?;
        let Some(row) = row else {
            tx.rollback().await?;
            return Err(anyhow::Error::new(WorkflowMailboxError::StaleClaim {
                message_id: request.message_id.clone(),
            }));
        };
        let message = mailbox_from_row(&row)?;
        tx.commit().await?;
        Ok(message)
    }

    /// Record the durable apply receipt for a claimed message.
    ///
    /// The fence is the claim identity (owner/token/generation), not the
    /// lease clock: no rival claimant can exist while the generation still
    /// matches, so allowing an apply after lease expiry only shrinks the
    /// window in which a crash could lead to re-applying content.  A
    /// delivered row returns idempotently; a repeated apply keeps the first
    /// receipt timestamp (`COALESCE`).
    pub async fn mark_mailbox_applied(
        &self,
        request: &WorkflowMailboxAckRequest,
    ) -> Result<WorkflowMailboxMessage> {
        validate_mailbox_ack(request)?;
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        let row = sqlx::query(sqlx::AssertSqlSafe(format!(
            "SELECT {MAILBOX_COLUMNS} FROM workflow_mailbox WHERE message_id = ?"
        )))
        .bind(&request.message_id)
        .fetch_optional(&mut *tx)
        .await?;
        let Some(row) = row else {
            tx.rollback().await?;
            return Err(anyhow::Error::new(WorkflowMailboxError::Missing {
                message_id: request.message_id.clone(),
            }));
        };
        let message = mailbox_from_row(&row)?;
        if message.state == WorkflowMailboxState::Delivered {
            tx.commit().await?;
            return Ok(message);
        }
        let applied_at_ms = now_ms();
        let row = sqlx::query(sqlx::AssertSqlSafe(format!(
            "UPDATE workflow_mailbox
             SET applied_at_ms = COALESCE(applied_at_ms, ?)
             WHERE message_id = ? AND state = 'delivering'
               AND claim_owner = ? AND claim_token = ? AND generation = ?
             RETURNING {MAILBOX_COLUMNS}"
        )))
        .bind(applied_at_ms)
        .bind(&request.message_id)
        .bind(&request.owner)
        .bind(&request.token)
        .bind(request.generation)
        .fetch_optional(&mut *tx)
        .await?;
        let Some(row) = row else {
            tx.rollback().await?;
            return Err(anyhow::Error::new(WorkflowMailboxError::StaleClaim {
                message_id: request.message_id.clone(),
            }));
        };
        let message = mailbox_from_row(&row)?;
        tx.commit().await?;
        Ok(message)
    }

    /// Convenience acknowledgement for the complete claim returned by this
    /// store.
    pub async fn ack_mailbox_claim(
        &self,
        claim: &WorkflowMailboxClaim,
    ) -> Result<WorkflowMailboxMessage> {
        self.ack_mailbox_message(&WorkflowMailboxAckRequest {
            message_id: claim.message.message_id.clone(),
            owner: claim.owner.clone(),
            token: claim.token.clone(),
            generation: claim.generation,
        })
        .await
    }

    /// List pending rows oldest-first for one recipient/channel.
    pub async fn list_mailbox_pending(
        &self,
        request: &WorkflowMailboxListRequest,
    ) -> Result<Vec<WorkflowMailboxMessage>> {
        validate_mailbox_list(request)?;
        let rows = sqlx::query(sqlx::AssertSqlSafe(format!(
            "SELECT {MAILBOX_COLUMNS} FROM workflow_mailbox
             WHERE recipient_run_id = ? AND channel = ? AND state = 'pending'
             ORDER BY sequence ASC LIMIT ?"
        )))
        .bind(&request.recipient_run_id)
        .bind(request.channel.as_str())
        .bind(i64::from(request.limit))
        .fetch_all(self.pool.as_ref())
        .await?;
        rows.iter().map(mailbox_from_row).collect()
    }

    /// Alias emphasizing that the listing excludes delivering/delivered rows.
    pub async fn list_pending_mailbox(
        &self,
        request: &WorkflowMailboxListRequest,
    ) -> Result<Vec<WorkflowMailboxMessage>> {
        self.list_mailbox_pending(request).await
    }

    /// Count pending and delivering rows for one root/channel capacity bucket.
    pub async fn mailbox_depth(
        &self,
        root_run_id: &str,
        channel: WorkflowMailboxChannel,
    ) -> Result<u32> {
        validate_mailbox_id(root_run_id, "mailbox root run id")?;
        let depth = sqlx::query_scalar::<_, i64>(
            "SELECT COUNT(*) FROM workflow_mailbox
             WHERE root_run_id = ? AND channel = ? AND state IN ('pending', 'delivering')",
        )
        .bind(root_run_id)
        .bind(channel.as_str())
        .fetch_one(self.pool.as_ref())
        .await?;
        depth_to_u32(depth)
    }

    /// Compatibility alias for callers that name the count a pending depth.
    pub async fn pending_mailbox_depth(
        &self,
        root_run_id: &str,
        channel: WorkflowMailboxChannel,
    ) -> Result<u32> {
        self.mailbox_depth(root_run_id, channel).await
    }
}

async fn mailbox_depth_in_tx(
    tx: &mut sqlx::Transaction<'_, Sqlite>,
    root_run_id: &str,
    channel: WorkflowMailboxChannel,
) -> Result<i64> {
    sqlx::query_scalar::<_, i64>(
        "SELECT COUNT(*) FROM workflow_mailbox
         WHERE root_run_id = ? AND channel = ? AND state IN ('pending', 'delivering')",
    )
    .bind(root_run_id)
    .bind(channel.as_str())
    .fetch_one(&mut **tx)
    .await
    .map_err(Into::into)
}

fn validate_message_create(input: &WorkflowMailboxMessageCreate) -> Result<String> {
    validate_mailbox_id(&input.message_id, "mailbox message id")?;
    validate_mailbox_id(&input.root_run_id, "mailbox root run id")?;
    validate_mailbox_id(&input.sender_run_id, "mailbox sender run id")?;
    validate_mailbox_id(&input.recipient_run_id, "mailbox recipient run id")?;
    let payload_json = serde_json::to_string(&input.payload)?;
    validate_json_bytes(&payload_json, "mailbox payload")?;
    if input.created_at_ms.is_some_and(|value| value < 0) {
        bail!("mailbox created timestamp must be non-negative");
    }
    Ok(payload_json)
}

fn validate_mailbox_claim(request: &WorkflowMailboxClaimRequest) -> Result<()> {
    validate_mailbox_id(&request.recipient_run_id, "mailbox recipient run id")?;
    validate_mailbox_id(&request.owner, "mailbox claim owner")?;
    if !(1..=MAX_MAILBOX_LEASE_MS).contains(&request.lease_duration_ms) {
        bail!("mailbox lease must be between 1 and {MAX_MAILBOX_LEASE_MS} milliseconds");
    }
    Ok(())
}

fn validate_mailbox_ack(request: &WorkflowMailboxAckRequest) -> Result<()> {
    validate_mailbox_id(&request.message_id, "mailbox message id")?;
    validate_mailbox_id(&request.owner, "mailbox claim owner")?;
    validate_mailbox_id(&request.token, "mailbox claim token")?;
    validate_nonnegative_i64(request.generation, "mailbox claim generation")
}

fn validate_mailbox_list(request: &WorkflowMailboxListRequest) -> Result<()> {
    validate_mailbox_id(&request.recipient_run_id, "mailbox recipient run id")?;
    validate_page_size(request.limit)
}

pub(super) fn validate_mailbox_id(value: &str, name: &str) -> Result<()> {
    validate_text(value, MAX_ID_BYTES, name)?;
    if value.contains('\0') {
        bail!("{name} must not contain NUL");
    }
    Ok(())
}

fn message_matches_input(
    message: &WorkflowMailboxMessage,
    input: &WorkflowMailboxMessageCreate,
) -> bool {
    message.root_run_id == input.root_run_id
        && message.sender_run_id == input.sender_run_id
        && message.recipient_run_id == input.recipient_run_id
        && message.channel == input.channel
        && message.payload == input.payload
}

pub(super) fn mailbox_from_row(row: &sqlx::sqlite::SqliteRow) -> Result<WorkflowMailboxMessage> {
    let payload_json = row.try_get::<String, _>("payload_json")?;
    validate_json_bytes(&payload_json, "mailbox payload")?;
    Ok(WorkflowMailboxMessage {
        message_id: row.try_get("message_id")?,
        root_run_id: row.try_get("root_run_id")?,
        sender_run_id: row.try_get("sender_run_id")?,
        recipient_run_id: row.try_get("recipient_run_id")?,
        sequence: row.try_get("sequence")?,
        channel: WorkflowMailboxChannel::from_str(&row.try_get::<String, _>("channel")?)?,
        state: WorkflowMailboxState::from_str(&row.try_get::<String, _>("state")?)?,
        payload: serde_json::from_str(&payload_json)?,
        created_at_ms: row.try_get("created_at_ms")?,
        claim_owner: row.try_get("claim_owner")?,
        claim_expires_at_ms: row.try_get("claim_expires_at_ms")?,
        acked_at_ms: row.try_get("acked_at_ms")?,
        applied_at_ms: row.try_get("applied_at_ms")?,
        generation: row.try_get("generation")?,
    })
}

fn depth_to_u32(depth: i64) -> Result<u32> {
    u32::try_from(depth).map_err(|_| anyhow::anyhow!("mailbox depth exceeds u32"))
}
