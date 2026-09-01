use super::*;

use crate::SqliteConfig;
use crate::runtime::test_support::test_thread_metadata;
use crate::runtime::test_support::unique_temp_dir;
use codex_protocol::ThreadId;
use codex_utils_absolute_path::test_support::PathExt;

#[tokio::test]
async fn fork_invariant_tombstone_preserves_durable_thread_row() -> anyhow::Result<()> {
    let codex_home = unique_temp_dir();
    let runtime = StateRuntime::init(
        SqliteConfig::new_for_testing(codex_home.as_path().abs()),
        "test-provider".to_string(),
    )
    .await?;
    let thread_id = ThreadId::from_u128(1);
    runtime
        .upsert_thread(&test_thread_metadata(
            &codex_home,
            thread_id,
            codex_home.clone(),
        ))
        .await?;
    assert!(runtime.get_thread(thread_id).await?.is_some());

    assert!(runtime.tombstone_thread(thread_id).await?);
    assert!(runtime.is_thread_tombstoned(thread_id).await?);
    assert!(runtime.get_thread(thread_id).await?.is_none());
    assert!(runtime.thread_exists(thread_id).await?);
    assert!(!runtime.tombstone_thread(thread_id).await?);

    let visible: i64 = sqlx::query_scalar("SELECT visible FROM threads WHERE id = ?")
        .bind(thread_id.to_string())
        .fetch_one(runtime.pool.as_ref())
        .await?;
    assert_eq!(visible, 0);
    Ok(())
}
