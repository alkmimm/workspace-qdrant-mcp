//! Migration v48: widen the search_events `op` CHECK to the full tool census.
//!
//! The telemetry-coverage sweep after v47 instrumented the remaining MCP
//! tools (graph, store, embedding, workspace_index, search_eval) with the
//! same dispatcher-level `search_events` records rules/scratchpad gained in
//! PR #267 — and each new op name would hit the same silent CHECK-violation
//! loss v39 and v47 fixed one op set at a time. This migration widens the
//! CHECK once to every tool that logs (or self-logs) an event, so adding
//! telemetry never needs another rebuild unless a genuinely NEW tool ships.
//!
//! Same rebuild recipe as v47 (SQLite cannot ALTER a CHECK in place) — plus
//! the lesson v47 taught in production: `ALTER TABLE ... RENAME` REWRITES
//! view definitions to follow the renamed table (`PRAGMA legacy_alter_table`
//! is per-connection and the pool ran the ALTER on a different connection),
//! so v47 left `search_behavior` dangling on `search_events_v47_old` and the
//! first v48 attempt crashed the daemon at startup (and, being
//! non-transactional, dropped `token_savings` before failing). EVERY view
//! over search_events must therefore be dropped BEFORE the rename and
//! recreated from its canonical constant after — this migration handles both
//! the healthy and the half-broken (dangling/missing views) starting states.
//! Idempotent via the `'workspace_index'` literal, which appears only in the
//! widened CHECK.

use async_trait::async_trait;
use sqlx::SqlitePool;
use tracing::{debug, info};

use super::migration::Migration;
use super::SchemaError;

pub struct V48Migration;

#[async_trait]
impl Migration for V48Migration {
    async fn up(&self, pool: &SqlitePool) -> Result<(), SchemaError> {
        info!("Migration v48: widening op CHECK on search_events to the full tool census");

        let table_exists: bool = sqlx::query_scalar(
            "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='search_events')",
        )
        .fetch_one(pool)
        .await?;
        if !table_exists {
            debug!("Migration v48: search_events does not exist; nothing to do");
            return Ok(());
        }

        let current_sql: String = sqlx::query_scalar(
            "SELECT sql FROM sqlite_master WHERE type='table' AND name='search_events'",
        )
        .fetch_one(pool)
        .await?;

        if current_sql.contains("'workspace_index'") {
            debug!("Migration v48: op CHECK already at the full census; skipping rebuild");
            return Ok(());
        }

        // Drop EVERY view over search_events before touching the table — the
        // rename would rewrite their definitions to the temp name and leave
        // them dangling once it is dropped (the v47 production incident).
        // IF EXISTS keeps this tolerant of the half-broken state that
        // incident left behind (token_savings already gone).
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
        sqlx::query("DROP TABLE IF EXISTS search_events_v48_old")
            .execute(pool)
            .await?;
        sqlx::query("ALTER TABLE search_events RENAME TO search_events_v48_old")
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
            FROM search_events_v48_old",
        )
        .execute(pool)
        .await?;

        sqlx::query("DROP TABLE search_events_v48_old")
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
            new_sql.contains("'workspace_index'"),
            "v48 rebuild left CHECK unchanged"
        );

        info!("Migration v48 complete");
        Ok(())
    }

    fn version(&self) -> i32 {
        48
    }

    fn description(&self) -> &'static str {
        "Widen op CHECK on search_events to the full tool census (graph/store/embedding/workspace_index/search_eval)"
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

    /// Build a DB at the post-v47 shape (v47 CHECK, v38 columns).
    async fn setup_pre_v48(pool: &SqlitePool) {
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
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn v48_rebuilds_table_to_accept_the_full_census() {
        let pool = fresh_pool().await;
        setup_pre_v48(&pool).await;

        let pre = sqlx::query(
            "INSERT INTO search_events (id, actor, tool, op, ts) \
             VALUES ('x', 'claude', 'mcp_qdrant', 'graph', '2026-07-15T00:00:00.000Z')",
        )
        .execute(&pool)
        .await;
        assert!(pre.is_err(), "pre-v48 CHECK should reject 'graph'");

        V48Migration.up(&pool).await.unwrap();

        for op in [
            "graph",
            "store",
            "embedding",
            "workspace_index",
            "search_eval",
            "rules",
            "grep",
        ] {
            sqlx::query(
                "INSERT INTO search_events (id, actor, tool, op, ts) \
                 VALUES (?1, 'claude', 'mcp_qdrant', ?2, '2026-07-15T00:00:00.000Z')",
            )
            .bind(format!("evt-{}", op))
            .bind(op)
            .execute(&pool)
            .await
            .unwrap_or_else(|e| panic!("post-v48 should accept op='{}': {}", op, e));
        }
    }

    /// The exact production starting state the first v48 attempt crashed on:
    /// `search_behavior` dangling on `search_events_v47_old` (v47's rename
    /// rewrote it) and `token_savings` already dropped (the failed attempt
    /// was non-transactional). v48 must complete and recreate BOTH views.
    #[tokio::test]
    async fn v48_heals_the_dangling_view_state_from_the_v47_incident() {
        let pool = fresh_pool().await;
        setup_pre_v48(&pool).await;
        // Dangling view: references a table that does not exist.
        sqlx::query(
            "CREATE VIEW search_behavior AS SELECT session_id FROM search_events_v47_old",
        )
        .execute(&pool)
        .await
        .unwrap();
        // (no token_savings view — dropped by the failed first attempt)

        V48Migration.up(&pool).await.unwrap();

        for view in ["token_savings", "search_behavior"] {
            let exists: bool = sqlx::query_scalar(
                "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='view' AND name=?1)",
            )
            .bind(view)
            .fetch_one(&pool)
            .await
            .unwrap();
            assert!(exists, "v48 must recreate view {}", view);
        }
        // The recreated search_behavior must be QUERYABLE (not dangling).
        sqlx::query("SELECT * FROM search_behavior LIMIT 1")
            .fetch_optional(&pool)
            .await
            .unwrap();
        // And the widened CHECK is in place.
        sqlx::query(
            "INSERT INTO search_events (id, actor, tool, op, ts) \
             VALUES ('g', 'claude', 'mcp_qdrant', 'graph', '2026-07-15T00:00:00.000Z')",
        )
        .execute(&pool)
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn v48_preserves_rows_and_is_idempotent() {
        let pool = fresh_pool().await;
        setup_pre_v48(&pool).await;
        sqlx::query(
            "INSERT INTO search_events (id, actor, tool, op, result_count, ts) \
             VALUES ('e1', 'claude', 'mcp_qdrant', 'rules', 2, '2026-07-15T00:00:00.000Z')",
        )
        .execute(&pool)
        .await
        .unwrap();

        V48Migration.up(&pool).await.unwrap();
        V48Migration.up(&pool).await.unwrap();

        let row: (String, i64) =
            sqlx::query_as("SELECT op, result_count FROM search_events WHERE id = 'e1'")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(row, ("rules".to_string(), 2));

        let view_exists: bool = sqlx::query_scalar(
            "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='view' AND name='token_savings')",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert!(view_exists, "v48 must recreate the token_savings view");
    }
}
