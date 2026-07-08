//! Migration v45: fix the correctness and scalability defects the merged-set
//! review found in the v44 `token_savings` probes, and stop the newly
//! populated `session_id` from mass-misclassifying `search_behavior`.
//!
//! What was wrong (all triggered by the same root event — the MCP server now
//! stamps `session_id` on 100% of rows, which activated clauses that were
//! inert while it was always NULL):
//!
//! 1. `had_followup`'s legacy session-window clause had no self/identity
//!    guard: every `op='followup'` row with `bytes_in` self-matched
//!    (delta = 0, own session/tool), and one followup smeared
//!    `had_followup=1` across ALL same-session events in its 60s window —
//!    not just its linked parent. The clause is now gated to UNLINKED rows
//!    (`parent_event_id IS NULL`, external-writer compat only) and excludes
//!    the row itself.
//! 2. Both probes bounded their windows with
//!    `julianday(nxt.ts) - julianday(se.ts) BETWEEN ...` — an expression no
//!    index can serve. Measured: 22.5s at 20k rows, >570s at 100k
//!    (quadratic). The legacy window is now a LEXICAL ISO-Z range on the raw
//!    `ts` column (the codebase's established ts-compare pattern), served by
//!    `idx_search_events_session_tool_ts`; the parent-link arms drop the
//!    window entirely — the writer already enforces it at classification
//!    time, and re-checking with INSERT-time deltas only produced false
//!    negatives under write-actor lag. Also fixes the truncated 0.000694d
//!    (= 59.96s) constant that under-covered the writer's 60.0s window.
//! 3. The parent-link followup arm now filters op IN
//!    ('search','search_exact','followup'): the MCP server no longer
//!    rewrites op (op is event identity; op-keyed analytics need the full
//!    census), so a followup is a parent-linked SEARCH row — while interim
//!    rows written by the v44-era server still carry op='followup'. Without
//!    the filter, grep lineage links would count as followups.
//! 4. `had_escalation` now requires `result_count > 0`: a refused or empty
//!    retrieve never delivered a document and must not mark the origin
//!    search as escalated.
//! 5. `search_behavior` (v14) classified ANY mcp event < 2 min after another
//!    same-session event as 'fallback' — inert while session_id was NULL,
//!    catastrophic once populated (normal agent usage = near-100% fallback).
//!    The fallback arm now means what it says: an rg/grep CLI event
//!    preceding an MCP search.
//!
//! All statements run in ONE transaction (v35+ convention): a failure
//! between DROP and CREATE must not leave the DB without the views.

use async_trait::async_trait;
use sqlx::SqlitePool;
use tracing::info;

use super::migration::Migration;
use super::SchemaError;

pub struct V45Migration;

pub const DROP_TOKEN_SAVINGS_VIEW_SQL: &str = "DROP VIEW IF EXISTS token_savings";
pub const DROP_SEARCH_BEHAVIOR_VIEW_SQL: &str = "DROP VIEW IF EXISTS search_behavior";

pub const CREATE_TOKEN_SAVINGS_VIEW_V45_SQL: &str = r#"
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
    (
        EXISTS (
            SELECT 1 FROM search_events nxt
            WHERE nxt.parent_event_id = se.id
              AND nxt.op IN ('search', 'search_exact', 'followup')
        )
        OR EXISTS (
            SELECT 1 FROM search_events nxt
            WHERE nxt.op = 'followup'
              AND nxt.parent_event_id IS NULL
              AND nxt.id != se.id
              AND nxt.session_id IS NOT NULL
              AND nxt.session_id = se.session_id
              AND nxt.tool = se.tool
              AND nxt.ts >= se.ts
              AND nxt.ts <= strftime('%Y-%m-%dT%H:%M:%fZ', se.ts, '+60 seconds')
        )
    ) AS had_followup,
    EXISTS (
        SELECT 1 FROM search_events nxt
        WHERE nxt.parent_event_id = se.id
          AND nxt.op IN ('open', 'expand', 'retrieve')
          AND COALESCE(nxt.result_count, 0) > 0
    ) AS had_escalation
FROM search_events se
WHERE se.bytes_in IS NOT NULL
"#;

pub const CREATE_SEARCH_BEHAVIOR_VIEW_V45_SQL: &str = r#"
CREATE VIEW IF NOT EXISTS search_behavior AS
WITH windowed_events AS (
    SELECT
        session_id,
        tool,
        op,
        ts,
        LAG(tool) OVER (PARTITION BY session_id ORDER BY ts) AS prev_tool,
        LAG(ts) OVER (PARTITION BY session_id ORDER BY ts) AS prev_ts,
        LEAD(op) OVER (PARTITION BY session_id ORDER BY ts) AS next_op,
        (julianday(ts) - julianday(LAG(ts) OVER (PARTITION BY session_id ORDER BY ts))) AS time_since_prev
    FROM search_events
    WHERE session_id IS NOT NULL
)
SELECT
    session_id,
    tool,
    op,
    ts,
    prev_tool,
    next_op,
    CASE
        WHEN tool IN ('rg', 'grep') AND prev_tool IS NULL THEN 'bypass'
        WHEN tool = 'mcp_qdrant' AND (next_op = 'open' OR next_op = 'expand') THEN 'success'
        WHEN tool = 'mcp_qdrant' AND time_since_prev < 0.00139
             AND prev_tool IN ('rg', 'grep') THEN 'fallback'
        ELSE 'unknown'
    END AS behavior
FROM windowed_events
"#;

#[async_trait]
impl Migration for V45Migration {
    async fn up(&self, pool: &SqlitePool) -> Result<(), SchemaError> {
        info!("Migration v45: fix token_savings probes (self-match, smear, quadratic window) + search_behavior fallback arm");

        let mut tx = pool.begin().await?;
        sqlx::query(DROP_TOKEN_SAVINGS_VIEW_SQL).execute(&mut *tx).await?;
        sqlx::query(CREATE_TOKEN_SAVINGS_VIEW_V45_SQL)
            .execute(&mut *tx)
            .await?;
        sqlx::query(DROP_SEARCH_BEHAVIOR_VIEW_SQL)
            .execute(&mut *tx)
            .await?;
        sqlx::query(CREATE_SEARCH_BEHAVIOR_VIEW_V45_SQL)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;

        info!("Migration v45 complete");
        Ok(())
    }

    fn version(&self) -> i32 {
        45
    }

    fn description(&self) -> &'static str {
        "Recreate token_savings (parent-link-first probes without self-match/smear, lexical ISO-Z \
         window, escalation requires results) and search_behavior (fallback = CLI-before-MCP only)"
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
        V45Migration.up(pool).await.unwrap();
    }

    async fn insert(
        pool: &SqlitePool,
        id: &str,
        ts: &str,
        op: &str,
        session: Option<&str>,
        parent: Option<&str>,
        bytes: Option<(i64, i64)>,
        result_count: Option<i32>,
    ) {
        sqlx::query(
            "INSERT INTO search_events (id, session_id, ts, actor, tool, op, parent_event_id, \
             bytes_in, bytes_out, shape_mode, result_count) \
             VALUES (?1, ?2, ?3, 'claude', 'mcp_qdrant', ?4, ?5, ?6, ?7, 'truncate', ?8)",
        )
        .bind(id)
        .bind(session)
        .bind(ts)
        .bind(op)
        .bind(parent)
        .bind(bytes.map(|b| b.0))
        .bind(bytes.map(|b| b.1))
        .bind(result_count)
        .execute(pool)
        .await
        .unwrap();
    }

    async fn had_followup(pool: &SqlitePool, id: &str) -> bool {
        sqlx::query_scalar("SELECT had_followup FROM token_savings WHERE id = ?1")
            .bind(id)
            .fetch_one(pool)
            .await
            .unwrap()
    }

    async fn had_escalation(pool: &SqlitePool, id: &str) -> bool {
        sqlx::query_scalar("SELECT had_escalation FROM token_savings WHERE id = ?1")
            .bind(id)
            .fetch_one(pool)
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn migration_is_idempotent() {
        let pool = fresh_pool().await;
        setup(&pool).await;
        V45Migration.up(&pool).await.unwrap();
        let count: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM sqlite_master WHERE type='view' AND name IN ('token_savings','search_behavior')",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(count, 2);
    }

    #[tokio::test]
    async fn parent_linked_search_row_counts_as_followup_without_op_rewrite() {
        let pool = fresh_pool().await;
        setup(&pool).await;
        insert(&pool, "a", "2026-07-08T12:00:00.000Z", "search", Some("s1"), None, Some((5000, 1000)), Some(5)).await;
        // op preserved as 'search' — the parent link alone carries the signal.
        insert(&pool, "b", "2026-07-08T12:00:30.000Z", "search", Some("s1"), Some("a"), Some((4000, 900)), Some(3)).await;
        assert!(had_followup(&pool, "a").await);
    }

    #[tokio::test]
    async fn interim_followup_op_rows_still_count() {
        let pool = fresh_pool().await;
        setup(&pool).await;
        insert(&pool, "a", "2026-07-08T12:00:00.000Z", "search", Some("s1"), None, Some((5000, 1000)), Some(5)).await;
        insert(&pool, "b", "2026-07-08T12:00:30.000Z", "followup", Some("s1"), Some("a"), Some((4000, 900)), Some(3)).await;
        assert!(had_followup(&pool, "a").await);
    }

    #[tokio::test]
    async fn followup_row_does_not_self_match() {
        let pool = fresh_pool().await;
        setup(&pool).await;
        // Lone parent-linked followup row with economy data: its own probe
        // must not fire on itself (the v44 defect).
        insert(&pool, "orphanless", "2026-07-08T12:00:00.000Z", "followup", Some("s1"), Some("missing"), Some((4000, 900)), Some(3)).await;
        assert!(!had_followup(&pool, "orphanless").await);
    }

    #[tokio::test]
    async fn linked_followup_does_not_smear_unrelated_same_session_events() {
        let pool = fresh_pool().await;
        setup(&pool).await;
        // Unrelated search A, then B, then a followup parent-linked to B only.
        insert(&pool, "a", "2026-07-08T12:00:00.000Z", "search", Some("s1"), None, Some((5000, 1000)), Some(5)).await;
        insert(&pool, "b", "2026-07-08T12:00:10.000Z", "search", Some("s1"), None, Some((5000, 1000)), Some(5)).await;
        insert(&pool, "b2", "2026-07-08T12:00:30.000Z", "search", Some("s1"), Some("b"), Some((4000, 900)), Some(3)).await;
        assert!(!had_followup(&pool, "a").await, "A must not inherit B's followup");
        assert!(had_followup(&pool, "b").await);
    }

    #[tokio::test]
    async fn grep_lineage_link_is_not_a_followup() {
        let pool = fresh_pool().await;
        setup(&pool).await;
        insert(&pool, "a", "2026-07-08T12:00:00.000Z", "search", Some("s1"), None, Some((5000, 1000)), Some(5)).await;
        insert(&pool, "g", "2026-07-08T12:00:20.000Z", "grep", Some("s1"), Some("a"), Some((3000, 800)), Some(7)).await;
        assert!(!had_followup(&pool, "a").await);
    }

    #[tokio::test]
    async fn unlinked_external_followup_counts_via_legacy_window() {
        let pool = fresh_pool().await;
        setup(&pool).await;
        insert(&pool, "a", "2026-07-08T12:00:00.000Z", "search", Some("s1"), None, Some((5000, 1000)), Some(5)).await;
        insert(&pool, "f", "2026-07-08T12:00:30.000Z", "followup", Some("s1"), None, None, None).await;
        assert!(had_followup(&pool, "a").await);
        // ...but not outside the 60s lexical window.
        insert(&pool, "b", "2026-07-08T13:00:00.000Z", "search", Some("s1"), None, Some((5000, 1000)), Some(5)).await;
        insert(&pool, "f2", "2026-07-08T13:01:30.000Z", "followup", Some("s1"), None, None, None).await;
        assert!(!had_followup(&pool, "b").await);
    }

    #[tokio::test]
    async fn escalation_requires_delivered_results() {
        let pool = fresh_pool().await;
        setup(&pool).await;
        insert(&pool, "a", "2026-07-08T12:00:00.000Z", "search", Some("s1"), None, Some((5000, 1000)), Some(5)).await;
        // Linked retrieve that was refused / delivered nothing.
        insert(&pool, "r0", "2026-07-08T12:00:30.000Z", "retrieve", Some("s1"), Some("a"), Some((0, 0)), Some(0)).await;
        assert!(!had_escalation(&pool, "a").await);
        // Linked retrieve that delivered a document.
        insert(&pool, "r1", "2026-07-08T12:00:40.000Z", "retrieve", Some("s1"), Some("a"), Some((9000, 9000)), Some(1)).await;
        assert!(had_escalation(&pool, "a").await);
    }

    #[tokio::test]
    async fn search_behavior_fallback_requires_cli_predecessor() {
        let pool = fresh_pool().await;
        setup(&pool).await;
        // mcp -> mcp seconds apart: NOT a fallback (the v14 defect).
        insert(&pool, "m1", "2026-07-08T12:00:00.000Z", "search", Some("s1"), None, None, None).await;
        insert(&pool, "m2", "2026-07-08T12:00:10.000Z", "search", Some("s1"), None, None, None).await;
        let behavior: String = sqlx::query_scalar(
            "SELECT behavior FROM search_behavior WHERE ts = '2026-07-08T12:00:10.000Z'",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_ne!(behavior, "fallback");
    }
}
