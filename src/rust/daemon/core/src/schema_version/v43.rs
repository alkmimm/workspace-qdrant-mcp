//! Migration v43: repair `treesitter_status` clobbered by the branch-dedup path.
//!
//! The cross-branch dedup fast-path (`strategies::processing::file::branch_dedup`)
//! re-tags an unchanged file for a new branch by UPSERTing the shared content-row.
//! Until the companion fix, it hardcoded `treesitter_status = 'none'`, so a file
//! that was semantically chunked on its first branch was REBASED to `'none'` the
//! moment any other branch checked it out — even though the shared Qdrant points
//! (and their `qdrant_chunks` mirror) still hold the semantic chunks.
//!
//! Symptoms:
//! - `tracked_files_by_chunking` / the "Code Chunking Coverage" dashboard
//!   under-reports semantic coverage (measured: 54.8% overall, java 22%, dart 28%).
//! - The capability-upgrade query (`treesitter_status IN ('none','failed','skipped')`)
//!   treats these files as perpetual re-chunk candidates that never converge.
//!
//! This migration re-derives the truth from the mirror: a file whose
//! `qdrant_chunks` contains ANY non-NULL `chunk_type` (function/method/class/…)
//! was tree-sitter chunked, so its `treesitter_status` is set back to `'done'`.
//!
//! LIMITATION — `chunk_type IS NULL` is ambiguous: it is stored both by the text
//! fallback (genuinely NOT tree-sitter chunked) AND by STRUCTURAL chunking of
//! no-pattern grammars (css/html/json), which is legitimately `'done'`. This
//! migration cannot tell them apart, so a structurally-chunked file that was
//! clobbered to `'none'` is NOT recovered here and stays a re-chunk candidate
//! until a genuine content change or the startup grammar-backfill sweep re-runs
//! it. Only files with a non-NULL `chunk_type` in the mirror are repaired.
//!
//! The companion `branch_dedup` fix (carry the source row's status) prevents new
//! corruption; this migration cleans up rows already clobbered before that fix.
//!
//! Idempotent: re-running only re-affirms `'done'` on already-semantic rows.
//! No-op on a fresh DB (empty `qdrant_chunks`) or before `qdrant_chunks` exists.

use async_trait::async_trait;
use sqlx::{Executor, SqlitePool};
use tracing::{debug, info};

use super::migration::Migration;
use super::SchemaError;

/// Repair statement: flip `treesitter_status` to `'done'` for any file the mirror
/// proves was semantically chunked but whose status was left non-`done`.
pub const BACKFILL_TREESITTER_STATUS_SQL: &str = r#"
UPDATE tracked_files
   SET treesitter_status = 'done'
 WHERE (treesitter_status IS NULL OR treesitter_status <> 'done')
   AND EXISTS (
        SELECT 1 FROM qdrant_chunks qc
         WHERE qc.file_id = tracked_files.file_id
           AND qc.chunk_type IS NOT NULL
   )
"#;

pub struct V43Migration;

#[async_trait]
impl Migration for V43Migration {
    async fn up(&self, pool: &SqlitePool) -> Result<(), SchemaError> {
        info!("Migration v43: backfill treesitter_status from qdrant_chunks mirror (dedup clobber repair)");

        let mut conn = pool.acquire().await?;

        // Both tables must exist; either being absent (fresh/partial DB) makes
        // this a no-op — the normal ingest path will set status correctly.
        let ready: bool = sqlx::query_scalar(
            "SELECT \
               EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='tracked_files') \
               AND \
               EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='qdrant_chunks')",
        )
        .fetch_one(&mut *conn)
        .await?;

        if !ready {
            debug!("Migration v43: tracked_files/qdrant_chunks absent; nothing to backfill");
            return Ok(());
        }

        let result = conn.execute(BACKFILL_TREESITTER_STATUS_SQL).await?;
        info!(
            "Migration v43 complete: repaired treesitter_status on {} file row(s)",
            result.rows_affected()
        );
        Ok(())
    }

    fn version(&self) -> i32 {
        43
    }

    fn description(&self) -> &'static str {
        "Backfill treesitter_status='done' from the qdrant_chunks mirror for files the branch-dedup \
         path clobbered to 'none' (semantic chunks still present)"
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

    /// Minimal shapes: only the columns this migration reads/writes.
    async fn setup_tables(pool: &SqlitePool) {
        sqlx::query(
            "CREATE TABLE tracked_files (\
               file_id INTEGER PRIMARY KEY, \
               language TEXT, \
               chunking_method TEXT, \
               treesitter_status TEXT)",
        )
        .execute(pool)
        .await
        .unwrap();
        sqlx::query(
            "CREATE TABLE qdrant_chunks (\
               chunk_id INTEGER PRIMARY KEY, \
               file_id INTEGER, \
               chunk_type TEXT)",
        )
        .execute(pool)
        .await
        .unwrap();
    }

    async fn status_of(pool: &SqlitePool, file_id: i64) -> Option<String> {
        sqlx::query_scalar("SELECT treesitter_status FROM tracked_files WHERE file_id = ?1")
            .bind(file_id)
            .fetch_one(pool)
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn v43_flips_dedup_clobbered_semantic_file_to_done() {
        let pool = fresh_pool().await;
        setup_tables(&pool).await;
        // A file clobbered to 'none' by dedup, but the mirror holds a semantic chunk.
        sqlx::query(
            "INSERT INTO tracked_files (file_id, language, chunking_method, treesitter_status) \
             VALUES (1, 'java', 'dedup_share', 'none')",
        )
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query("INSERT INTO qdrant_chunks (file_id, chunk_type) VALUES (1, 'method')")
            .execute(&pool)
            .await
            .unwrap();

        V43Migration.up(&pool).await.unwrap();

        assert_eq!(status_of(&pool, 1).await.as_deref(), Some("done"));
    }

    #[tokio::test]
    async fn v43_leaves_text_only_file_untouched() {
        let pool = fresh_pool().await;
        setup_tables(&pool).await;
        // A genuinely text-chunked file: mirror has only NULL-type chunks.
        sqlx::query(
            "INSERT INTO tracked_files (file_id, language, chunking_method, treesitter_status) \
             VALUES (2, 'yaml', 'dedup_share', 'none')",
        )
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query("INSERT INTO qdrant_chunks (file_id, chunk_type) VALUES (2, NULL)")
            .execute(&pool)
            .await
            .unwrap();

        V43Migration.up(&pool).await.unwrap();

        assert_eq!(
            status_of(&pool, 2).await.as_deref(),
            Some("none"),
            "text-only file must NOT be flipped to done"
        );
    }

    #[tokio::test]
    async fn v43_is_idempotent_and_preserves_done() {
        let pool = fresh_pool().await;
        setup_tables(&pool).await;
        sqlx::query(
            "INSERT INTO tracked_files (file_id, language, chunking_method, treesitter_status) \
             VALUES (3, 'rust', 'tree_sitter', 'done')",
        )
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query("INSERT INTO qdrant_chunks (file_id, chunk_type) VALUES (3, 'function')")
            .execute(&pool)
            .await
            .unwrap();

        V43Migration.up(&pool).await.unwrap();
        V43Migration.up(&pool).await.unwrap();

        assert_eq!(status_of(&pool, 3).await.as_deref(), Some("done"));
    }

    #[tokio::test]
    async fn v43_noop_without_tables() {
        let pool = fresh_pool().await;
        // No tables created — must not error.
        V43Migration.up(&pool).await.unwrap();
    }
}
