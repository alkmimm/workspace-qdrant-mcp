//! Migration v42: per-session liveness tracking for `watch_folders.is_active`.
//!
//! `is_active` was intended (see `priority_manager` docs) to reflect the number
//! of live sessions on a project, but `register_session`/`unregister_session`
//! mutated it as a free-running counter (`is_active + 1` / `- 1`) with no record
//! of *which* sessions were counted. A session that ended ungracefully — or the
//! MCP server's own `ensureSelfRepoRegistered`, which re-registers the self-repo
//! on every (re)start with no balancing decrement — leaked `+1` forever (observed:
//! the self-repo at `is_active = 12`). The inflated counter is behaviourally
//! benign (every reader tests `is_active > 0`) but defeats the `DeprioritizeProject`
//! lever, which would need N decrements to actually deactivate a project.
//!
//! This migration introduces `project_sessions` — one row per live `(tenant,
//! collection, session_id)` — so register/unregister/heartbeat become idempotent
//! and `is_active` is recomputed as `COUNT(*)` of live sessions. Stale sessions
//! (no heartbeat within the timeout) are reaped by the session monitor.
//!
//! Existing `is_active` values are leaked counters with no surviving sessions
//! (the daemon restart that applies this migration drops every live session), so
//! they are reset to 0. The MCP server re-registers active sessions on connect.
//!
//! Idempotent: `CREATE TABLE IF NOT EXISTS` + the reset is safe to re-run.

use async_trait::async_trait;
use sqlx::{Executor, SqlitePool};
use tracing::{debug, info};

use super::migration::Migration;
use super::SchemaError;

/// `project_sessions`: one row per live session on a project. `is_active` on
/// `watch_folders` is derived from `COUNT(*)` of these rows per `(tenant_id,
/// collection)`.
pub const CREATE_PROJECT_SESSIONS_SQL: &str = r#"
CREATE TABLE IF NOT EXISTS project_sessions (
    tenant_id         TEXT NOT NULL,
    collection        TEXT NOT NULL,
    session_id        TEXT NOT NULL,
    registered_at     TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
    last_heartbeat_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
    PRIMARY KEY (tenant_id, collection, session_id)
)
"#;

/// Index on `last_heartbeat_at` so the orphan-session reaper can scan stale
/// rows without a full table scan.
pub const CREATE_PROJECT_SESSIONS_HEARTBEAT_INDEX_SQL: &str = r#"
CREATE INDEX IF NOT EXISTS idx_project_sessions_heartbeat
    ON project_sessions (last_heartbeat_at)
"#;

pub struct V42Migration;

#[async_trait]
impl Migration for V42Migration {
    async fn up(&self, pool: &SqlitePool) -> Result<(), SchemaError> {
        info!("Migration v42: per-session liveness tracking (project_sessions); reset leaked is_active");

        let mut conn = pool.acquire().await?;

        conn.execute(CREATE_PROJECT_SESSIONS_SQL).await?;
        conn.execute(CREATE_PROJECT_SESSIONS_HEARTBEAT_INDEX_SQL)
            .await?;

        // Drop the leaked free-running counters. No session survives the daemon
        // restart that applies this migration, so every project starts idle and
        // is re-activated from project_sessions when its session re-registers.
        let wf_exists: bool = sqlx::query_scalar(
            "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='watch_folders')",
        )
        .fetch_one(&mut *conn)
        .await?;
        if wf_exists {
            let reset = sqlx::query("UPDATE watch_folders SET is_active = 0 WHERE is_active <> 0")
                .execute(&mut *conn)
                .await?;
            debug!(
                "Migration v42: reset is_active on {} watch_folder row(s)",
                reset.rows_affected()
            );
        }

        info!("Migration v42 complete: project_sessions created, is_active reset");
        Ok(())
    }

    fn version(&self) -> i32 {
        42
    }

    fn description(&self) -> &'static str {
        "Per-session liveness: project_sessions table; is_active derived from live-session count \
         (reset leaked counters)"
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

    async fn table_exists(pool: &SqlitePool, name: &str) -> bool {
        sqlx::query_scalar::<_, bool>(
            "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name = ?1)",
        )
        .bind(name)
        .fetch_one(pool)
        .await
        .unwrap()
    }

    #[tokio::test]
    async fn v42_creates_project_sessions() {
        let pool = fresh_pool().await;
        V42Migration.up(&pool).await.unwrap();
        assert!(table_exists(&pool, "project_sessions").await);
    }

    #[tokio::test]
    async fn v42_is_idempotent() {
        let pool = fresh_pool().await;
        V42Migration.up(&pool).await.unwrap();
        V42Migration.up(&pool).await.unwrap();
        assert!(table_exists(&pool, "project_sessions").await);
    }

    #[tokio::test]
    async fn v42_resets_leaked_is_active() {
        let pool = fresh_pool().await;
        // Minimal watch_folders shape with a leaked counter.
        sqlx::query(
            "CREATE TABLE watch_folders (watch_id TEXT PRIMARY KEY, tenant_id TEXT, \
             collection TEXT, is_active INTEGER DEFAULT 0)",
        )
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO watch_folders (watch_id, tenant_id, collection, is_active) \
             VALUES ('w1', 't1', 'projects', 12)",
        )
        .execute(&pool)
        .await
        .unwrap();

        V42Migration.up(&pool).await.unwrap();

        let active: i32 =
            sqlx::query_scalar("SELECT is_active FROM watch_folders WHERE watch_id = 'w1'")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(active, 0, "leaked counter reset to 0");
    }
}
