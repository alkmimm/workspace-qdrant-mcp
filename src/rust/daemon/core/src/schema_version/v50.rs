//! Migration v50: dequeue candidate index on `unified_queue`.
//!
//! Dequeue used to rank the WHOLE queue inside `BEGIN IMMEDIATE`: ~0.9 s of
//! CPU per dequeue at 663k pending rows (2026-11-10), during which every other
//! writer waited — a primary-key UPDATE took p50 3 s and the queue drained
//! slower than it filled. Dequeue now ranks a short candidate list collected by
//! index (`queue_operations/dequeue_candidates.rs`); this index is what makes
//! that collection ~1 ms.
//!
//! Additive and idempotent (`CREATE INDEX IF NOT EXISTS`). On a large queue the
//! build is a one-off at startup (a few seconds for ~700k rows).

use async_trait::async_trait;
use sqlx::SqlitePool;
use tracing::{debug, info};

use super::migration::Migration;
use super::SchemaError;
use crate::unified_queue_schema::CREATE_STATUS_GROUP_INDEX_SQL;

pub struct V50Migration;

#[async_trait]
impl Migration for V50Migration {
    async fn up(&self, pool: &SqlitePool) -> Result<(), SchemaError> {
        let table_exists: bool = sqlx::query_scalar(
            "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='unified_queue')",
        )
        .fetch_one(pool)
        .await?;
        if !table_exists {
            debug!("Migration v50: unified_queue does not exist; nothing to do");
            return Ok(());
        }
        info!("Migration v50: creating idx_unified_queue_status_group");
        sqlx::query(CREATE_STATUS_GROUP_INDEX_SQL)
            .execute(pool)
            .await?;
        info!("Migration v50 complete");
        Ok(())
    }

    fn version(&self) -> i32 {
        50
    }

    fn description(&self) -> &'static str {
        "Add idx_unified_queue_status_group so dequeue ranks a candidate list, not the whole queue"
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sqlx::sqlite::SqlitePoolOptions;

    async fn fresh_pool() -> SqlitePool {
        SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .unwrap()
    }

    async fn index_exists(pool: &SqlitePool) -> bool {
        sqlx::query_scalar(
            "SELECT EXISTS(SELECT 1 FROM sqlite_master \
             WHERE type='index' AND name='idx_unified_queue_status_group')",
        )
        .fetch_one(pool)
        .await
        .unwrap()
    }

    #[tokio::test]
    async fn v50_creates_index_and_is_idempotent() {
        let pool = fresh_pool().await;
        sqlx::query(crate::unified_queue_schema::CREATE_UNIFIED_QUEUE_SQL)
            .execute(&pool)
            .await
            .unwrap();
        assert!(!index_exists(&pool).await);

        V50Migration.up(&pool).await.unwrap();
        V50Migration.up(&pool).await.unwrap();
        assert!(index_exists(&pool).await);

        // The group enumeration seek must use it (an equality prefix on status).
        for stmt in crate::unified_queue_schema::CREATE_UNIFIED_QUEUE_INDEXES_SQL {
            sqlx::query(stmt).execute(&pool).await.unwrap();
        }
        let plan: Vec<String> = sqlx::query(
            "EXPLAIN QUERY PLAN \
             SELECT tenant_id FROM unified_queue WHERE status = 'pending' \
             AND tenant_id > 'a' ORDER BY tenant_id LIMIT 1",
        )
        .fetch_all(&pool)
        .await
        .unwrap()
        .iter()
        .map(|row| sqlx::Row::get::<String, _>(row, "detail"))
        .collect();
        assert!(
            plan.iter()
                .any(|d| d.contains("idx_unified_queue_status_group")),
            "group seek must use the v50 index, got {:?}",
            plan
        );
    }

    #[tokio::test]
    async fn v50_without_queue_table_is_a_noop() {
        let pool = fresh_pool().await;
        V50Migration.up(&pool).await.unwrap();
        assert!(!index_exists(&pool).await);
    }
}
