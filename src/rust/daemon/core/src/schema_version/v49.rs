//! Migration v49: add 'help' to the search_events `op` CHECK.
//!
//! PR #358 shipped the static `help` tool (on-demand topical manual) outside
//! OP_EVENT_TOOLS precisely because this CHECK did not accept a 'help' op —
//! every insert would have been a silent CHECK-violation loss (the v39/v47
//! lesson). With the op accepted, the TypeScript dispatcher instruments
//! `help` like the other op-event tools, so its adoption shows up in the
//! same search_events lane the mcp-server dashboard's "by op & actor"
//! panels read (the daemon exporter GROUPs BY op dynamically — no exporter
//! change needed).
//!
//! Same rebuild recipe as v48 (SQLite cannot ALTER a CHECK in place), with
//! the same view discipline the v47 production incident taught: EVERY view
//! over search_events is dropped BEFORE the rename (ALTER TABLE ... RENAME
//! rewrites view definitions to follow the renamed table) and recreated from
//! its canonical constant after. Idempotent via the `'help'` literal, which
//! appears only in the widened CHECK.

use async_trait::async_trait;
use sqlx::SqlitePool;
use tracing::{debug, info};

use super::migration::Migration;
use super::SchemaError;

pub struct V49Migration;

#[async_trait]
impl Migration for V49Migration {
    async fn up(&self, pool: &SqlitePool) -> Result<(), SchemaError> {
        info!("Migration v49: adding 'help' to the op CHECK on search_events");

        let table_exists: bool = sqlx::query_scalar(
            "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='search_events')",
        )
        .fetch_one(pool)
        .await?;
        if !table_exists {
            debug!("Migration v49: search_events does not exist; nothing to do");
            return Ok(());
        }

        let current_sql: String = sqlx::query_scalar(
            "SELECT sql FROM sqlite_master WHERE type='table' AND name='search_events'",
        )
        .fetch_one(pool)
        .await?;

        if current_sql.contains("'help'") {
            debug!("Migration v49: op CHECK already accepts 'help'; skipping rebuild");
            return Ok(());
        }

        // Drop EVERY view over search_events before touching the table — the
        // rename would rewrite their definitions to the temp name and leave
        // them dangling once it is dropped (the v47 production incident).
        sqlx::query("DROP VIEW IF EXISTS token_savings")
            .execute(pool)
            .await?;
        sqlx::query("DROP VIEW IF EXISTS search_behavior")
            .execute(pool)
            .await?;
        sqlx::query("PRAGMA foreign_keys = OFF")
            .execute(pool)
            .await?;
        sqlx::query("PRAGMA legacy_alter_table = ON")
            .execute(pool)
            .await?;
        sqlx::query("DROP TABLE IF EXISTS search_events_v49_old")
            .execute(pool)
            .await?;
        sqlx::query("ALTER TABLE search_events RENAME TO search_events_v49_old")
            .execute(pool)
            .await?;
        sqlx::query("PRAGMA legacy_alter_table = OFF")
            .execute(pool)
            .await?;

        sqlx::query(
            r#"CREATE TABLE search_events (
                id TEXT PRIMARY KEY NOT NULL,
                ts TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
                session_id TEXT,
                project_id TEXT,
                actor TEXT NOT NULL CHECK (actor IN ('claude', 'user', 'daemon')),
                tool TEXT NOT NULL CHECK (tool IN ('mcp_qdrant', 'rg', 'grep', 'ctags', 'lsp', 'filesearch')),
                op TEXT NOT NULL CHECK (op IN ('search', 'expand', 'open', 'followup', 'grep', 'retrieve', 'list', 'search_exact', 'rules', 'scratchpad', 'graph', 'store', 'embedding', 'workspace_index', 'search_eval', 'help')),
                query_text TEXT,
                filters TEXT,
                top_k INTEGER,
                result_count INTEGER,
                latency_ms INTEGER,
                top_result_refs TEXT,
                outcome TEXT,
                parent_event_id TEXT,
                bytes_in INTEGER,
                bytes_out INTEGER,
                hits_truncated INTEGER,
                shape_mode TEXT,
                tool_version TEXT,
                created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now'))
            )"#,
        )
        .execute(pool)
        .await?;

        sqlx::query(
            "INSERT INTO search_events ( \
                id, ts, session_id, project_id, actor, tool, op, \
                query_text, filters, top_k, result_count, latency_ms, \
                top_result_refs, outcome, parent_event_id, \
                bytes_in, bytes_out, hits_truncated, shape_mode, tool_version, \
                created_at \
            ) SELECT \
                id, ts, session_id, project_id, actor, tool, op, \
                query_text, filters, top_k, result_count, latency_ms, \
                top_result_refs, outcome, parent_event_id, \
                bytes_in, bytes_out, hits_truncated, shape_mode, tool_version, \
                created_at \
            FROM search_events_v49_old",
        )
        .execute(pool)
        .await?;

        sqlx::query("DROP TABLE search_events_v49_old")
            .execute(pool)
            .await?;

        use crate::search_events_schema::CREATE_SEARCH_EVENTS_INDEXES_SQL;
        for index_sql in CREATE_SEARCH_EVENTS_INDEXES_SQL {
            sqlx::query(index_sql).execute(pool).await?;
        }
        use crate::schema_version::v38::CREATE_SESSION_TOOL_TS_INDEX_SQL;
        use crate::schema_version::v44::CREATE_PARENT_EVENT_ID_INDEX_SQL;
        use crate::schema_version::v45::CREATE_SEARCH_BEHAVIOR_VIEW_V45_SQL;
        use crate::schema_version::v46::CREATE_TOKEN_SAVINGS_VIEW_V46_SQL;
        sqlx::query(CREATE_SESSION_TOOL_TS_INDEX_SQL)
            .execute(pool)
            .await?;
        sqlx::query(CREATE_PARENT_EVENT_ID_INDEX_SQL)
            .execute(pool)
            .await?;
        sqlx::query(CREATE_TOKEN_SAVINGS_VIEW_V46_SQL)
            .execute(pool)
            .await?;
        sqlx::query(CREATE_SEARCH_BEHAVIOR_VIEW_V45_SQL)
            .execute(pool)
            .await?;

        sqlx::query("PRAGMA foreign_keys = ON")
            .execute(pool)
            .await?;

        let new_sql: String = sqlx::query_scalar(
            "SELECT sql FROM sqlite_master WHERE type='table' AND name='search_events'",
        )
        .fetch_one(pool)
        .await?;
        debug_assert!(
            new_sql.contains("'help'"),
            "v49 rebuild left CHECK unchanged"
        );

        info!("Migration v49 complete");
        Ok(())
    }

    fn version(&self) -> i32 {
        49
    }

    fn description(&self) -> &'static str {
        "Add 'help' to the op CHECK on search_events (PR #358 follow-up: help adoption telemetry)"
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

    /// Build a DB at the post-v48 shape (full census CHECK, no 'help').
    async fn setup_pre_v49(pool: &SqlitePool) {
        sqlx::query(
            r#"CREATE TABLE search_events (
                id TEXT PRIMARY KEY NOT NULL,
                ts TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
                session_id TEXT,
                project_id TEXT,
                actor TEXT NOT NULL CHECK (actor IN ('claude', 'user', 'daemon')),
                tool TEXT NOT NULL CHECK (tool IN ('mcp_qdrant', 'rg', 'grep', 'ctags', 'lsp', 'filesearch')),
                op TEXT NOT NULL CHECK (op IN ('search', 'expand', 'open', 'followup', 'grep', 'retrieve', 'list', 'search_exact', 'rules', 'scratchpad', 'graph', 'store', 'embedding', 'workspace_index', 'search_eval')),
                query_text TEXT,
                filters TEXT,
                top_k INTEGER,
                result_count INTEGER,
                latency_ms INTEGER,
                top_result_refs TEXT,
                outcome TEXT,
                parent_event_id TEXT,
                bytes_in INTEGER,
                bytes_out INTEGER,
                hits_truncated INTEGER,
                shape_mode TEXT,
                tool_version TEXT,
                created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now'))
            )"#,
        )
        .execute(pool)
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn v49_rebuilds_table_to_accept_help() {
        let pool = fresh_pool().await;
        setup_pre_v49(&pool).await;

        let pre = sqlx::query(
            "INSERT INTO search_events (id, actor, tool, op, ts) \
             VALUES ('x', 'claude', 'mcp_qdrant', 'help', '2026-08-26T00:00:00.000Z')",
        )
        .execute(&pool)
        .await;
        assert!(pre.is_err(), "pre-v49 CHECK should reject 'help'");

        V49Migration.up(&pool).await.unwrap();

        for op in ["help", "search", "workspace_index", "search_eval"] {
            sqlx::query(
                "INSERT INTO search_events (id, actor, tool, op, ts) \
                 VALUES (?1, 'claude', 'mcp_qdrant', ?2, '2026-08-26T00:00:00.000Z')",
            )
            .bind(format!("evt-{}", op))
            .bind(op)
            .execute(&pool)
            .await
            .unwrap_or_else(|e| panic!("post-v49 should accept op='{}': {}", op, e));
        }
    }

    #[tokio::test]
    async fn v49_preserves_rows_views_and_is_idempotent() {
        let pool = fresh_pool().await;
        setup_pre_v49(&pool).await;
        sqlx::query(
            "INSERT INTO search_events (id, actor, tool, op, result_count, bytes_in, ts) \
             VALUES ('e1', 'claude', 'mcp_qdrant', 'grep', 7, 1234, '2026-08-26T00:00:00.000Z')",
        )
        .execute(&pool)
        .await
        .unwrap();

        V49Migration.up(&pool).await.unwrap();
        V49Migration.up(&pool).await.unwrap();

        let row: (String, i64, i64) =
            sqlx::query_as("SELECT op, result_count, bytes_in FROM search_events WHERE id = 'e1'")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(row, ("grep".to_string(), 7, 1234));

        for view in ["token_savings", "search_behavior"] {
            let exists: bool = sqlx::query_scalar(
                "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='view' AND name=?1)",
            )
            .bind(view)
            .fetch_one(&pool)
            .await
            .unwrap();
            assert!(exists, "v49 must recreate view {}", view);
            // And it must be queryable (not dangling on the temp name).
            sqlx::query(&format!("SELECT * FROM {} LIMIT 1", view))
                .fetch_optional(&pool)
                .await
                .unwrap();
        }
    }
}
