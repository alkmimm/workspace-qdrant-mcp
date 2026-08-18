//! Migration v44: make the `token_savings` effectiveness probes match the
//! signals the MCP server actually writes.
//!
//! Spec 20 §1.2 defines `had_followup` / `had_escalation`, and the MCP
//! server (as of the effectiveness-signals change) writes them as:
//!   - followup:   a re-issued search is logged with `op = 'followup'` AND
//!     `parent_event_id` = the origin search event;
//!   - escalation: a retrieve of a document a recent search returned is
//!     logged with `op = 'retrieve'` AND `parent_event_id` = the origin.
//!
//! The v38 view predates those writers and misses both:
//!   - its followup probe requires `nxt.session_id = se.session_id`, which
//!     never matches when either side is NULL (all pre-v44 rows) and is
//!     less precise than the explicit parent link;
//!   - its escalation probe only accepts `op IN ('open','expand')` — ops no
//!     writer emits — so the linked `op='retrieve'` rows don't count.
//!
//! v44 recreates the view with parent-link-first probes (keeping the legacy
//! session-window clause as a fallback for any external writer that follows
//! the original spec wording) and adds an index on `parent_event_id` to
//! drive them.

use async_trait::async_trait;
use sqlx::SqlitePool;
use tracing::info;

use super::migration::Migration;
use super::SchemaError;

pub struct V44Migration;

pub const CREATE_PARENT_EVENT_ID_INDEX_SQL: &str =
    "CREATE INDEX IF NOT EXISTS idx_search_events_parent_event_id \
     ON search_events(parent_event_id)";

pub const DROP_TOKEN_SAVINGS_VIEW_SQL: &str = "DROP VIEW IF EXISTS token_savings";

pub const CREATE_TOKEN_SAVINGS_VIEW_V44_SQL: &str = r#"
CREATE VIEW IF NOT EXISTS token_savings AS
SELECT
    se.id,
    se.session_id,
    se.project_id,
    se.tool,
    se.op,
    se.shape_mode,
    se.tool_version,
    se.ts,
    se.bytes_in,
    se.bytes_out,
    se.hits_truncated,
    CASE
        WHEN se.bytes_in IS NOT NULL AND se.bytes_out IS NOT NULL
            THEN se.bytes_in - se.bytes_out
        ELSE NULL
    END AS savings_bytes,
    CASE
        WHEN se.bytes_in IS NOT NULL AND se.bytes_in > 0 AND se.bytes_out IS NOT NULL
            THEN 1.0 * (se.bytes_in - se.bytes_out) / se.bytes_in
        ELSE NULL
    END AS savings_ratio,
    EXISTS (
        SELECT 1 FROM search_events nxt
        WHERE nxt.op = 'followup'
          AND julianday(nxt.ts) - julianday(se.ts) BETWEEN 0 AND 0.000694
          AND (nxt.parent_event_id = se.id
               OR (nxt.session_id IS NOT NULL
                   AND nxt.session_id = se.session_id
                   AND nxt.tool = se.tool))
    ) AS had_followup,
    EXISTS (
        SELECT 1 FROM search_events nxt
        WHERE nxt.parent_event_id = se.id
          AND nxt.op IN ('open', 'expand', 'retrieve')
          AND julianday(nxt.ts) - julianday(se.ts) BETWEEN 0 AND 0.001389
    ) AS had_escalation
FROM search_events se
WHERE se.bytes_in IS NOT NULL
"#;

#[async_trait]
impl Migration for V44Migration {
    async fn up(&self, pool: &SqlitePool) -> Result<(), SchemaError> {
        info!("Migration v44: recreate token_savings view with parent-linked effectiveness probes");

        sqlx::query(CREATE_PARENT_EVENT_ID_INDEX_SQL)
            .execute(pool)
            .await?;
        sqlx::query(DROP_TOKEN_SAVINGS_VIEW_SQL)
            .execute(pool)
            .await?;
        sqlx::query(CREATE_TOKEN_SAVINGS_VIEW_V44_SQL)
            .execute(pool)
            .await?;

        info!("Migration v44 complete");
        Ok(())
    }

    fn version(&self) -> i32 {
        44
    }

    fn description(&self) -> &'static str {
        "Recreate token_savings view: followup/escalation probes match the parent_event_id + \
         op='followup'/'retrieve' linkage the MCP server writes"
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

    async fn setup(pool: &SqlitePool) {
        use crate::search_events_schema::{
            CREATE_SEARCH_EVENTS_INDEXES_SQL, CREATE_SEARCH_EVENTS_SQL,
        };
        sqlx::query(CREATE_SEARCH_EVENTS_SQL)
            .execute(pool)
            .await
            .unwrap();
        for index_sql in CREATE_SEARCH_EVENTS_INDEXES_SQL {
            sqlx::query(index_sql).execute(pool).await.unwrap();
        }
        crate::schema_version::v38::V38Migration
            .up(pool)
            .await
            .unwrap();
        V44Migration.up(pool).await.unwrap();
    }

    #[tokio::test]
    async fn migration_is_idempotent() {
        let pool = fresh_pool().await;
        setup(&pool).await;
        V44Migration.up(&pool).await.unwrap();
        let count: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM sqlite_master WHERE type='view' AND name='token_savings'",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(count, 1);
    }

    #[tokio::test]
    async fn followup_detected_via_parent_link_with_null_sessions() {
        let pool = fresh_pool().await;
        setup(&pool).await;

        // NULL session_id on both rows — the v38 session-equality probe never
        // fired here; the parent link must carry the signal alone.
        sqlx::query(
            "INSERT INTO search_events (id, ts, actor, tool, op, bytes_in, bytes_out, shape_mode) \
             VALUES ('e1', '2026-07-07T12:00:00.000Z', 'claude', 'mcp_qdrant', 'search', 5000, 1000, 'truncate')",
        )
        .execute(&pool).await.unwrap();
        sqlx::query(
            "INSERT INTO search_events (id, ts, actor, tool, op, parent_event_id) \
             VALUES ('e2', '2026-07-07T12:00:30.000Z', 'claude', 'mcp_qdrant', 'followup', 'e1')",
        )
        .execute(&pool)
        .await
        .unwrap();

        let had_followup: bool =
            sqlx::query_scalar("SELECT had_followup FROM token_savings WHERE id = 'e1'")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert!(had_followup);
    }

    #[tokio::test]
    async fn followup_outside_window_not_counted() {
        let pool = fresh_pool().await;
        setup(&pool).await;

        sqlx::query(
            "INSERT INTO search_events (id, ts, actor, tool, op, bytes_in, bytes_out, shape_mode) \
             VALUES ('e1', '2026-07-07T12:00:00.000Z', 'claude', 'mcp_qdrant', 'search', 5000, 1000, 'truncate')",
        )
        .execute(&pool).await.unwrap();
        // 5 minutes later — outside the 60s window even though parent-linked.
        sqlx::query(
            "INSERT INTO search_events (id, ts, actor, tool, op, parent_event_id) \
             VALUES ('e2', '2026-07-07T12:05:00.000Z', 'claude', 'mcp_qdrant', 'followup', 'e1')",
        )
        .execute(&pool)
        .await
        .unwrap();

        let had_followup: bool =
            sqlx::query_scalar("SELECT had_followup FROM token_savings WHERE id = 'e1'")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert!(!had_followup);
    }

    #[tokio::test]
    async fn escalation_detected_via_linked_retrieve() {
        let pool = fresh_pool().await;
        setup(&pool).await;

        sqlx::query(
            "INSERT INTO search_events (id, ts, actor, tool, op, bytes_in, bytes_out, shape_mode) \
             VALUES ('e1', '2026-07-07T12:00:00.000Z', 'claude', 'mcp_qdrant', 'search', 5000, 1000, 'truncate')",
        )
        .execute(&pool).await.unwrap();
        // The op the MCP retrieve tool actually writes — v38 only accepted
        // 'open'/'expand' here and missed it.
        sqlx::query(
            "INSERT INTO search_events (id, ts, actor, tool, op, parent_event_id) \
             VALUES ('e2', '2026-07-07T12:01:00.000Z', 'claude', 'mcp_qdrant', 'retrieve', 'e1')",
        )
        .execute(&pool)
        .await
        .unwrap();

        let had_escalation: bool =
            sqlx::query_scalar("SELECT had_escalation FROM token_savings WHERE id = 'e1'")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert!(had_escalation);
    }

    #[tokio::test]
    async fn unlinked_retrieve_is_not_an_escalation() {
        let pool = fresh_pool().await;
        setup(&pool).await;

        sqlx::query(
            "INSERT INTO search_events (id, ts, actor, tool, op, bytes_in, bytes_out, shape_mode) \
             VALUES ('e1', '2026-07-07T12:00:00.000Z', 'claude', 'mcp_qdrant', 'search', 5000, 1000, 'truncate')",
        )
        .execute(&pool).await.unwrap();
        // Standalone retrieve (no parent link) — must not count against e1.
        sqlx::query(
            "INSERT INTO search_events (id, ts, actor, tool, op) \
             VALUES ('e2', '2026-07-07T12:01:00.000Z', 'claude', 'mcp_qdrant', 'retrieve')",
        )
        .execute(&pool)
        .await
        .unwrap();

        let had_escalation: bool =
            sqlx::query_scalar("SELECT had_escalation FROM token_savings WHERE id = 'e1'")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert!(!had_escalation);
    }

    #[tokio::test]
    async fn legacy_session_window_followup_still_counts() {
        let pool = fresh_pool().await;
        setup(&pool).await;

        // No parent link, but same non-NULL session inside the window — the
        // legacy clause (spec 20 §1.2 original wording) must still fire.
        sqlx::query(
            "INSERT INTO search_events (id, session_id, ts, actor, tool, op, bytes_in, bytes_out, shape_mode) \
             VALUES ('e1', 'sess-1', '2026-07-07T12:00:00.000Z', 'claude', 'mcp_qdrant', 'search', 5000, 1000, 'truncate')",
        )
        .execute(&pool).await.unwrap();
        sqlx::query(
            "INSERT INTO search_events (id, session_id, ts, actor, tool, op) \
             VALUES ('e2', 'sess-1', '2026-07-07T12:00:30.000Z', 'claude', 'mcp_qdrant', 'followup')",
        )
        .execute(&pool).await.unwrap();

        let had_followup: bool =
            sqlx::query_scalar("SELECT had_followup FROM token_savings WHERE id = 'e1'")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert!(had_followup);
    }
}
