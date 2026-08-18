//! Migration v47: accept `rules` and `scratchpad` in the search_events `op` CHECK.
//!
//! PR #267 wired `search_events` telemetry into the rules/scratchpad MCP ops,
//! but the live table still constrained `op` to the v39 set — every INSERT
//! with `op = 'rules'` failed the CHECK and vanished inside the fire-and-forget
//! write path (caught by live verification: 0 rows despite successful calls,
//! no error anywhere; the exact silent-loss shape v39 fixed for
//! `search_exact`). SQLite cannot ALTER a CHECK in place, so this migration
//! rebuilds the table with the widened set — same recipe as v39, updated for
//! the columns/indexes/view that landed since (v38 columns, v44
//! parent_event_id index, v46 token_savings view).
//!
//! Idempotent: skips the rebuild when the new CHECK is already present
//! (fresh DBs create the table from the updated
//! `search_events_schema::CREATE_SEARCH_EVENTS_SQL` constant).

use async_trait::async_trait;
use sqlx::SqlitePool;
use tracing::{debug, info};

use super::migration::Migration;
use super::SchemaError;

pub struct V47Migration;

#[async_trait]
impl Migration for V47Migration {
    async fn up(&self, pool: &SqlitePool) -> Result<(), SchemaError> {
        info!("Migration v47: widening op CHECK on search_events (+rules, +scratchpad)");

        let table_exists: bool = sqlx::query_scalar(
            "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='search_events')",
        )
        .fetch_one(pool)
        .await?;
        if !table_exists {
            debug!("Migration v47: search_events does not exist; nothing to do");
            return Ok(());
        }

        let current_sql: String = sqlx::query_scalar(
            "SELECT sql FROM sqlite_master WHERE type='table' AND name='search_events'",
        )
        .fetch_one(pool)
        .await?;

        // `'scratchpad'` appears ONLY in the widened op CHECK (unlike
        // `'rules'`, which is a collection name elsewhere in the system but
        // not in this table's DDL — still, 'scratchpad' is the safer probe).
        if current_sql.contains("'scratchpad'") {
            debug!("Migration v47: op CHECK already widened; skipping rebuild");
            return Ok(());
        }

        // Same pool-per-statement rebuild as v39 (avoids the in-memory-DB
        // stale-schema quirk with a separately acquired connection).
        //
        // EVERY view over search_events must be dropped before the rename:
        // `ALTER TABLE ... RENAME` rewrites view definitions to follow the
        // renamed table (`PRAGMA legacy_alter_table` is per-connection, and
        // the pool may run the ALTER elsewhere). The first cut dropped only
        // token_savings and left `search_behavior` dangling on the temp name
        // — which crashed the daemon at the NEXT schema touch (v48, in
        // production). Recreated from the canonical v45 constant below.
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
        sqlx::query("DROP TABLE IF EXISTS search_events_v47_old")
            .execute(pool)
            .await?;
        sqlx::query("ALTER TABLE search_events RENAME TO search_events_v47_old")
            .execute(pool)
            .await?;
        sqlx::query("PRAGMA legacy_alter_table = OFF")
            .execute(pool)
            .await?;

        // Plain CREATE (no IF NOT EXISTS) so SQLite evaluates the DDL fresh
        // after the rename — see v39 for the rationale. Column set mirrors
        // the LIVE post-v38 table.
        sqlx::query(
            r#"CREATE TABLE search_events (
                id TEXT PRIMARY KEY NOT NULL,
                ts TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
                session_id TEXT,
                project_id TEXT,
                actor TEXT NOT NULL CHECK (actor IN ('claude', 'user', 'daemon')),
                tool TEXT NOT NULL CHECK (tool IN ('mcp_qdrant', 'rg', 'grep', 'ctags', 'lsp', 'filesearch')),
                op TEXT NOT NULL CHECK (op IN ('search', 'expand', 'open', 'followup', 'grep', 'retrieve', 'list', 'search_exact', 'rules', 'scratchpad')),
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

        // Explicit column list so a future column addition can't silently
        // scramble positions.
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
            FROM search_events_v47_old",
        )
        .execute(pool)
        .await?;

        sqlx::query("DROP TABLE search_events_v47_old")
            .execute(pool)
            .await?;

        // Recreate every index/view the table has accumulated: canonical v12
        // trio, v38 session_tool_ts, v44 parent_event_id, v46 token_savings.
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
            new_sql.contains("'scratchpad'"),
            "v47 rebuild left CHECK unchanged"
        );

        info!("Migration v47 complete");
        Ok(())
    }

    fn version(&self) -> i32 {
        47
    }

    fn description(&self) -> &'static str {
        "Widen op CHECK on search_events to include rules/scratchpad (silent CHECK-violation loss)"
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

    /// Build a DB at the LIVE pre-v47 shape: v39 CHECK, v38 columns.
    async fn setup_pre_v47(pool: &SqlitePool) {
        sqlx::query(
            r#"CREATE TABLE search_events (
                id TEXT PRIMARY KEY NOT NULL,
                ts TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
                session_id TEXT,
                project_id TEXT,
                actor TEXT NOT NULL CHECK (actor IN ('claude', 'user', 'daemon')),
                tool TEXT NOT NULL CHECK (tool IN ('mcp_qdrant', 'rg', 'grep', 'ctags', 'lsp', 'filesearch')),
                op TEXT NOT NULL CHECK (op IN ('search', 'expand', 'open', 'followup', 'grep', 'retrieve', 'list', 'search_exact')),
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
    async fn v47_rebuilds_table_to_accept_rules_and_scratchpad() {
        let pool = fresh_pool().await;
        setup_pre_v47(&pool).await;

        // Pre-v47: 'rules' is rejected — the exact silent-loss bug.
        let pre = sqlx::query(
            "INSERT INTO search_events (id, actor, tool, op, ts) \
             VALUES ('x', 'claude', 'mcp_qdrant', 'rules', '2026-07-15T00:00:00.000Z')",
        )
        .execute(&pool)
        .await;
        assert!(pre.is_err(), "pre-v47 CHECK should reject 'rules'");

        V47Migration.up(&pool).await.unwrap();

        for op in ["rules", "scratchpad", "grep", "search"] {
            sqlx::query(
                "INSERT INTO search_events (id, actor, tool, op, ts) \
                 VALUES (?1, 'claude', 'mcp_qdrant', ?2, '2026-07-15T00:00:00.000Z')",
            )
            .bind(format!("evt-{}", op))
            .bind(op)
            .execute(&pool)
            .await
            .unwrap_or_else(|e| panic!("post-v47 should accept op='{}': {}", op, e));
        }
    }

    #[tokio::test]
    async fn v47_preserves_rows_and_recreates_indexes_and_view() {
        let pool = fresh_pool().await;
        setup_pre_v47(&pool).await;
        sqlx::query(
            "INSERT INTO search_events \
              (id, session_id, actor, tool, op, result_count, bytes_in, bytes_out, ts) \
             VALUES ('e1', 'sess', 'claude', 'mcp_qdrant', 'grep', 7, 9000, 1200, '2026-07-15T00:00:00.000Z')",
        )
        .execute(&pool)
        .await
        .unwrap();

        V47Migration.up(&pool).await.unwrap();

        let row: (String, i64, i64) =
            sqlx::query_as("SELECT op, result_count, bytes_in FROM search_events WHERE id = 'e1'")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(row, ("grep".to_string(), 7, 9000));

        for name in [
            "idx_search_events_session",
            "idx_search_events_tool",
            "idx_search_events_project",
            "idx_search_events_session_tool_ts",
            "idx_search_events_parent_event_id",
        ] {
            let exists: bool = sqlx::query_scalar(
                "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='index' AND name=?1)",
            )
            .bind(name)
            .fetch_one(&pool)
            .await
            .unwrap();
            assert!(exists, "v47 must recreate index {}", name);
        }
        let view_exists: bool = sqlx::query_scalar(
            "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='view' AND name='token_savings')",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert!(view_exists, "v47 must recreate the token_savings view");
    }

    #[tokio::test]
    async fn v47_is_idempotent_on_fresh_db_with_new_check() {
        let pool = fresh_pool().await;
        sqlx::query(crate::search_events_schema::CREATE_SEARCH_EVENTS_SQL)
            .execute(&pool)
            .await
            .unwrap();

        V47Migration.up(&pool).await.unwrap();
        V47Migration.up(&pool).await.unwrap();

        sqlx::query(
            "INSERT INTO search_events (id, actor, tool, op, ts) \
             VALUES ('y', 'claude', 'mcp_qdrant', 'scratchpad', '2026-07-15T00:00:00.000Z')",
        )
        .execute(&pool)
        .await
        .unwrap();
    }
}
