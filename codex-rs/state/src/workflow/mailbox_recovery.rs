//! Crash recovery for durable workflow mailbox claims.

use anyhow::Result;

use super::WorkflowStore;
use super::mailbox::MAILBOX_COLUMNS;
use super::mailbox::mailbox_from_row;
use super::mailbox::validate_mailbox_id;
use super::mailbox_types::WorkflowMailboxMessage;
use super::types::validate_nonnegative_i64;

impl WorkflowStore {
    /// Return expired delivering claims to pending and fence old consumers.
    /// This method never schedules or performs a retry.
    pub async fn reclaim_expired_mailbox(
        &self,
        now_ms: i64,
    ) -> Result<Vec<WorkflowMailboxMessage>> {
        self.reclaim_expired_mailbox_inner(None, now_ms).await
    }

    /// Return expired delivering claims to `pending` for one recipient.
    ///
    /// Session delivery uses this scoped form so one recipient cannot reclaim
    /// another recipient's in-flight message without also waking that
    /// recipient's causal waiters.
    pub async fn reclaim_expired_mailbox_for_recipient(
        &self,
        recipient_run_id: &str,
        now_ms: i64,
    ) -> Result<Vec<WorkflowMailboxMessage>> {
        validate_mailbox_id(recipient_run_id, "mailbox recipient run id")?;
        self.reclaim_expired_mailbox_inner(Some(recipient_run_id), now_ms)
            .await
    }

    // `applied_at_ms` is intentionally absent from the SET list below (and in
    // `requeue_undelivered_mailbox`): reclaims reset delivery state, not
    // application history. The receipt must survive so a redelivery of an
    // applied-but-unacked row skips re-applying its content.
    async fn reclaim_expired_mailbox_inner(
        &self,
        recipient_run_id: Option<&str>,
        now_ms: i64,
    ) -> Result<Vec<WorkflowMailboxMessage>> {
        validate_nonnegative_i64(now_ms, "mailbox reclaim timestamp")?;
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        let recipient_filter = if recipient_run_id.is_some() {
            " AND recipient_run_id = ?"
        } else {
            ""
        };
        let mut query = sqlx::query(sqlx::AssertSqlSafe(format!(
            "UPDATE workflow_mailbox
             SET state = 'pending', claim_owner = NULL, claim_token = NULL,
                 claim_expires_at_ms = NULL, generation = generation + 1
             WHERE state = 'delivering' AND claim_expires_at_ms IS NOT NULL
               AND claim_expires_at_ms <= ?{recipient_filter}
             RETURNING {MAILBOX_COLUMNS}"
        )))
        .bind(now_ms);
        if let Some(recipient_run_id) = recipient_run_id {
            query = query.bind(recipient_run_id);
        }
        let rows = query.fetch_all(&mut *tx).await?;
        let mut messages = rows
            .iter()
            .map(mailbox_from_row)
            .collect::<Result<Vec<_>>>()?;
        messages.sort_by(|left, right| {
            left.recipient_run_id
                .cmp(&right.recipient_run_id)
                .then_with(|| left.channel.as_str().cmp(right.channel.as_str()))
                .then_with(|| left.sequence.cmp(&right.sequence))
        });
        tx.commit().await?;
        Ok(messages)
    }

    /// Return every non-delivered message for a recipient to `pending` during
    /// session rehydration.
    ///
    /// A process can disappear after claiming a row and before the canonical
    /// rollout append or acknowledgement. Waiting for that process's lease
    /// would leave the message unavailable for the whole lease duration, so a
    /// new session fences all old claims immediately. Delivering generations
    /// advance while pending generations remain stable; delivered rows are
    /// never touched.
    pub async fn requeue_undelivered_mailbox(
        &self,
        recipient_run_id: &str,
    ) -> Result<Vec<WorkflowMailboxMessage>> {
        validate_mailbox_id(recipient_run_id, "mailbox recipient run id")?;
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        let rows = sqlx::query(sqlx::AssertSqlSafe(format!(
            "UPDATE workflow_mailbox
             SET state = 'pending', claim_owner = NULL, claim_token = NULL,
                 claim_expires_at_ms = NULL, acked_at_ms = NULL,
                 generation = CASE WHEN state = 'delivering'
                                    THEN generation + 1 ELSE generation END
             WHERE recipient_run_id = ? AND state IN ('pending', 'delivering')
             RETURNING {MAILBOX_COLUMNS}"
        )))
        .bind(recipient_run_id)
        .fetch_all(&mut *tx)
        .await?;
        let mut messages = rows
            .iter()
            .map(mailbox_from_row)
            .collect::<Result<Vec<_>>>()?;
        messages.sort_by(|left, right| {
            left.sequence
                .cmp(&right.sequence)
                .then_with(|| left.channel.as_str().cmp(right.channel.as_str()))
        });
        tx.commit().await?;
        Ok(messages)
    }
}
