//! Durable logical deletion for threads.
//!
//! Tombstoning changes only the visibility projection. Rollouts, receipts,
//! artifacts, goals, and history references remain in their owning stores.

use chrono::Utc;
use codex_protocol::ThreadId;
use std::collections::HashSet;

use super::StateRuntime;

impl StateRuntime {
    /// Mark threads invisible without deleting their durable state.
    ///
    /// Repeating the operation is idempotent and returns only rows whose
    /// visible projection changed during this call.
    pub async fn tombstone_threads(&self, thread_ids: &[ThreadId]) -> anyhow::Result<u64> {
        if thread_ids.is_empty() {
            return Ok(0);
        }
        let timestamp = Utc::now().timestamp_millis();
        let mut unique_ids = HashSet::with_capacity(thread_ids.len());
        let mut tx = self.pool.begin().await?;
        let mut changed = 0;
        for thread_id in thread_ids {
            if !unique_ids.insert(*thread_id) {
                continue;
            }
            changed += sqlx::query(
                "UPDATE threads
                 SET tombstoned_at = COALESCE(tombstoned_at, ?), visible = 0
                 WHERE id = ? AND (visible != 0 OR tombstoned_at IS NULL)",
            )
            .bind(timestamp)
            .bind(thread_id.to_string())
            .execute(&mut *tx)
            .await?
            .rows_affected();
        }
        tx.commit().await?;
        Ok(changed)
    }

    /// Mark one thread invisible without removing its durable row.
    pub async fn tombstone_thread(&self, thread_id: ThreadId) -> anyhow::Result<bool> {
        Ok(self.tombstone_threads(&[thread_id]).await? == 1)
    }

    /// Return whether a thread exists and has been logically deleted.
    pub async fn is_thread_tombstoned(&self, thread_id: ThreadId) -> anyhow::Result<bool> {
        Ok(sqlx::query_scalar::<_, i64>(
            "SELECT EXISTS(
                 SELECT 1 FROM threads
                 WHERE id = ? AND (visible = 0 OR tombstoned_at IS NOT NULL)
             )",
        )
        .bind(thread_id.to_string())
        .fetch_one(self.pool.as_ref())
        .await?
            != 0)
    }

    /// Return whether the durable main-state row exists, including tombstones.
    pub async fn thread_exists(&self, thread_id: ThreadId) -> anyhow::Result<bool> {
        Ok(
            sqlx::query_scalar::<_, i64>("SELECT EXISTS(SELECT 1 FROM threads WHERE id = ?)")
                .bind(thread_id.to_string())
                .fetch_one(self.pool.as_ref())
                .await?
                != 0,
        )
    }
}

#[cfg(test)]
#[path = "thread_tombstone_tests.rs"]
mod tests;
