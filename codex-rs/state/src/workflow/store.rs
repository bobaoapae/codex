//! Shared workflow database handle.

use anyhow::Result;
use sqlx::SqlitePool;
use std::sync::Arc;

use crate::SqliteConfig;
use crate::migrations::runtime_workflow_migrator;

#[derive(Clone)]
pub struct WorkflowStore {
    pub(super) pool: Arc<SqlitePool>,
}

impl WorkflowStore {
    /// Open and migrate the workflow database beneath the configured SQLite
    /// home. The database uses the same WAL and busy-timeout configuration as
    /// the other state databases.
    pub async fn open(sqlite: &SqliteConfig) -> Result<Self> {
        tokio::fs::create_dir_all(sqlite.home()).await?;
        let migrator = runtime_workflow_migrator();
        let pool = sqlite.open_workflow_db(&migrator, None).await?;
        Ok(Self::from_pool(Arc::new(pool)))
    }

    pub(crate) fn from_pool(pool: Arc<SqlitePool>) -> Self {
        Self { pool }
    }

    /// Return the underlying pool for narrowly scoped projection queries.
    pub fn pool(&self) -> &SqlitePool {
        self.pool.as_ref()
    }

    /// Close the workflow pool and wait for its workers to stop.
    pub async fn close(&self) {
        self.pool.close().await;
    }
}
