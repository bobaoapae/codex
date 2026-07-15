use crate::LocalExtensionsConfig;
use anyhow::Context;
use serde::Serialize;
use serde::de::DeserializeOwned;
use sqlx::Row;
use sqlx::SqlitePool;
use sqlx::sqlite::SqliteConnectOptions;
use sqlx::sqlite::SqliteJournalMode;
use sqlx::sqlite::SqlitePoolOptions;
use sqlx::sqlite::SqliteSynchronous;
use std::path::Path;
use std::path::PathBuf;
use std::str::FromStr;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::OnceCell;

const DATABASE_NAME: &str = "local-extensions.sqlite";
const SCHEMA_VERSION: i64 = 1;

/// Lazily opened storage for rebuildable data owned by local fork extensions.
///
/// Callers must treat every error as a cache miss and continue through the
/// canonical Codex path. The store never reads or modifies upstream databases.
#[derive(Debug, Clone)]
pub struct LocalExtensionsStore {
    enabled: bool,
    path: PathBuf,
    pool: Arc<OnceCell<SqlitePool>>,
}

impl LocalExtensionsStore {
    pub fn new(codex_home: &Path, config: &LocalExtensionsConfig) -> Self {
        Self {
            enabled: config.any_enabled(),
            path: codex_home.join(DATABASE_NAME),
            pool: Arc::new(OnceCell::new()),
        }
    }

    pub fn is_enabled(&self) -> bool {
        self.enabled
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub async fn save_latest_plan<T: Serialize>(
        &self,
        thread_id: &str,
        plan: &T,
    ) -> anyhow::Result<()> {
        let Some(pool) = self.pool().await? else {
            return Ok(());
        };
        let plan_json = serde_json::to_string(plan).context("serialize latest plan")?;
        sqlx::query(
            "INSERT INTO latest_plans(thread_id, plan_json, updated_at) VALUES(?1, ?2, unixepoch()) \
             ON CONFLICT(thread_id) DO UPDATE SET plan_json=excluded.plan_json, \
             updated_at=excluded.updated_at",
        )
        .bind(thread_id)
        .bind(plan_json)
        .execute(pool)
        .await
        .context("save latest local plan")?;
        tracing::debug!(target: "codex_local_features", feature = "operations_dock", "saved local plan snapshot");
        Ok(())
    }

    pub async fn load_latest_plan<T: DeserializeOwned>(
        &self,
        thread_id: &str,
    ) -> anyhow::Result<Option<T>> {
        let Some(pool) = self.pool().await? else {
            return Ok(None);
        };
        let row = sqlx::query("SELECT plan_json FROM latest_plans WHERE thread_id=?1")
            .bind(thread_id)
            .fetch_optional(pool)
            .await
            .context("load latest local plan")?;
        row.map(|row| {
            serde_json::from_str(row.get::<&str, _>("plan_json"))
                .context("decode latest local plan")
        })
        .transpose()
    }

    async fn pool(&self) -> anyhow::Result<Option<&SqlitePool>> {
        if !self.enabled {
            return Ok(None);
        }
        let path = self.path.clone();
        self.pool
            .get_or_try_init(|| async move { open_database(&path).await })
            .await
            .map(Some)
    }
}

async fn open_database(path: &Path) -> anyhow::Result<SqlitePool> {
    let options = SqliteConnectOptions::from_str(&format!("sqlite://{}", path.display()))?
        .create_if_missing(true)
        .journal_mode(SqliteJournalMode::Wal)
        .synchronous(SqliteSynchronous::Normal)
        .busy_timeout(Duration::from_secs(5));
    let pool = SqlitePoolOptions::new()
        .max_connections(2)
        .connect_with(options)
        .await
        .context("open local extension database")?;
    if let Err(error) = initialize_schema(&pool).await {
        pool.close().await;
        return Err(error);
    }
    tracing::debug!(target: "codex_local_features", "opened local extension database");
    Ok(pool)
}

async fn initialize_schema(pool: &SqlitePool) -> anyhow::Result<()> {
    let mut transaction = pool.begin().await?;
    sqlx::query(
        "CREATE TABLE IF NOT EXISTS local_schema(\
            singleton INTEGER PRIMARY KEY CHECK(singleton = 1),\
            schema_version INTEGER NOT NULL\
        )",
    )
    .execute(&mut *transaction)
    .await?;
    sqlx::query(
        "CREATE TABLE IF NOT EXISTS latest_plans(\
            thread_id TEXT PRIMARY KEY,\
            plan_json TEXT NOT NULL,\
            updated_at INTEGER NOT NULL\
        )",
    )
    .execute(&mut *transaction)
    .await?;
    sqlx::query(
        "INSERT INTO local_schema(singleton, schema_version) VALUES(1, ?1) \
         ON CONFLICT(singleton) DO UPDATE SET schema_version=excluded.schema_version",
    )
    .bind(SCHEMA_VERSION)
    .execute(&mut *transaction)
    .await?;
    transaction.commit().await?;
    Ok(())
}

#[cfg(test)]
#[path = "store_tests.rs"]
mod tests;
