//! Migration v41: Layer 2 stage 2 — collapse `tracked_files` to one content-row.
//!
//! Stage 1 made `base_point` branch-agnostic, so one physical Qdrant point is
//! shared across every branch that holds identical content. Stage 2 finishes the
//! model on the SQLite side: the per-branch `branch` column is replaced by a
//! `branches` JSON array (the set of branches that hold this content), and the
//! UNIQUE constraint moves from `(watch_folder_id, relative_path, branch)` to
//! `(watch_folder_id, relative_path, file_hash)` — ONE row per content version of
//! a path, regardless of how many branches share it.
//!
//! Pre-release "NO MIGRATION EFFORT": this rebuild **discards** the legacy rows
//! and clears `qdrant_chunks`; the post-deploy reembed repopulates one row per
//! content with its branch set. (Layer 2 always requires a reembed because the
//! point identity changed in stage 1.)
//!
//! Idempotent: skips when `tracked_files` already carries the `branches` column.

use async_trait::async_trait;
use sqlx::{Executor, SqlitePool};
use tracing::{debug, info};

use super::migration::Migration;
use super::SchemaError;

pub struct V41Migration;

#[async_trait]
impl Migration for V41Migration {
    async fn up(&self, pool: &SqlitePool) -> Result<(), SchemaError> {
        info!(
            "Migration v41: Layer 2 — collapse tracked_files to one content-row (branches JSON set)"
        );

        let mut conn = pool.acquire().await?;

        let exists: bool = sqlx::query_scalar(
            "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='tracked_files')",
        )
        .fetch_one(&mut *conn)
        .await?;

        if exists {
            let sql: String = sqlx::query_scalar(
                "SELECT sql FROM sqlite_master WHERE type='table' AND name='tracked_files'",
            )
            .fetch_one(&mut *conn)
            .await?;
            if sql.contains("branches TEXT") {
                debug!("Migration v41: tracked_files already content-keyed; skipping");
                return Ok(());
            }
        }

        // Rebuild tracked_files into the content-keyed shape. The FK child
        // qdrant_chunks is cleared below; FK checks are disabled for the swap.
        conn.execute("PRAGMA foreign_keys = OFF").await?;
        conn.execute("DROP TABLE IF EXISTS tracked_files_old").await?;
        if exists {
            // legacy_alter_table keeps dependent FKs pointing at the symbolic
            // name `tracked_files`, which we re-create immediately.
            conn.execute("PRAGMA legacy_alter_table = ON").await?;
            conn.execute("ALTER TABLE tracked_files RENAME TO tracked_files_old")
                .await?;
            conn.execute("PRAGMA legacy_alter_table = OFF").await?;
        }
        conn.execute(crate::tracked_files_schema::CREATE_TRACKED_FILES_V41_SQL)
            .await?;
        conn.execute("DROP TABLE IF EXISTS tracked_files_old").await?;

        for stmt in crate::tracked_files_schema::CREATE_TRACKED_FILES_V41_INDEXES_SQL {
            conn.execute(*stmt).await?;
        }
        conn.execute(crate::tracked_files_schema::CREATE_RECONCILE_INDEX_SQL)
            .await?;
        conn.execute(crate::tracked_files_schema::CREATE_BASE_POINT_INDEX_SQL)
            .await?;
        conn.execute(crate::tracked_files_schema::CREATE_REFCOUNT_INDEX_SQL)
            .await?;

        // qdrant_chunks references the now-discarded rows — clear the mirror so
        // the reembed rebuilds it fresh against the new content-rows.
        let qc_exists: bool = sqlx::query_scalar(
            "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='qdrant_chunks')",
        )
        .fetch_one(&mut *conn)
        .await?;
        if qc_exists {
            conn.execute("DELETE FROM qdrant_chunks").await?;
        }

        conn.execute("PRAGMA foreign_keys = ON").await?;

        info!(
            "Migration v41 complete: tracked_files content-keyed (branches JSON), qdrant_chunks \
             cleared — a full reembed is required to repopulate"
        );
        Ok(())
    }

    fn version(&self) -> i32 {
        41
    }

    fn description(&self) -> &'static str {
        "Layer 2 stage 2: tracked_files content-keyed with branches JSON set (drop scalar branch); \
         clear qdrant_chunks (reembed required)"
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

    async fn has_branches_column(pool: &SqlitePool) -> bool {
        sqlx::query_scalar::<_, bool>(
            "SELECT COUNT(*) > 0 FROM pragma_table_info('tracked_files') WHERE name = 'branches'",
        )
        .fetch_one(pool)
        .await
        .unwrap()
    }

    #[tokio::test]
    async fn v41_rebuilds_to_branches_set_and_drops_branch() {
        let pool = fresh_pool().await;
        // Pre-v41 shape (v37/v40): per-branch with scalar `branch`.
        sqlx::query(crate::tracked_files_schema::CREATE_TRACKED_FILES_V37_SQL)
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query(crate::tracked_files_schema::CREATE_QDRANT_CHUNKS_SQL)
            .execute(&pool)
            .await
            .unwrap();

        V41Migration.up(&pool).await.unwrap();

        assert!(has_branches_column(&pool).await, "branches column present");
        let has_branch: bool = sqlx::query_scalar(
            "SELECT COUNT(*) > 0 FROM pragma_table_info('tracked_files') WHERE name = 'branch'",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert!(!has_branch, "scalar branch column dropped");
    }

    #[tokio::test]
    async fn v41_is_idempotent() {
        let pool = fresh_pool().await;
        sqlx::query(crate::tracked_files_schema::CREATE_TRACKED_FILES_V41_SQL)
            .execute(&pool)
            .await
            .unwrap();
        assert!(has_branches_column(&pool).await);
        V41Migration.up(&pool).await.unwrap();
        V41Migration.up(&pool).await.unwrap();
        assert!(has_branches_column(&pool).await);
    }
}
