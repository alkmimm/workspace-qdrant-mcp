//! Migration v46: defeat the planner's index mischoice in the v45
//! `token_savings` legacy-followup arm.
//!
//! Post-deploy verification of v45 on the LIVE database showed the full-view
//! scan at ~15s for 8k rows despite the scratch-DB validation running the
//! same shape in 0.03s. EXPLAIN QUERY PLAN on the live DB: the planner
//! served the legacy arm's `nxt.parent_event_id IS NULL` predicate from
//! `idx_search_events_parent_event_id` — and with ~99% of live rows carrying
//! a NULL parent (nothing wrote parents before the effectiveness-signals
//! change), that "index search" scans nearly the whole index PER OUTER ROW
//! (~66M visits). The stats-free scratch DB happened to pick the right
//! index; the live one did not — the recurring fresh-copy-vs-live planner
//! gotcha.
//!
//! Fix: the classic SQLite unary-`+` hint (`+nxt.parent_event_id IS NULL`)
//! disqualifies that column from index selection, forcing the planner onto
//! `idx_search_events_session_tool_ts (session_id=? AND tool=? AND ts>? AND
//! ts<?)` — the index built for exactly this probe. Live-verified before
//! shipping: 15s → 0.005s on the same data. Everything else is identical to
//! v45.

use async_trait::async_trait;
use sqlx::SqlitePool;
use tracing::info;

use super::migration::Migration;
use super::SchemaError;

pub struct V46Migration;

pub const DROP_TOKEN_SAVINGS_VIEW_SQL: &str = "DROP VIEW IF EXISTS token_savings";

pub const CREATE_TOKEN_SAVINGS_VIEW_V46_SQL: &str = r#"
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
              AND +nxt.parent_event_id IS NULL
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

#[async_trait]
impl Migration for V46Migration {
    async fn up(&self, pool: &SqlitePool) -> Result<(), SchemaError> {
        info!("Migration v46: unary-+ hint on the token_savings legacy-followup arm (live planner picked the NULL-heavy parent index)");

        let mut tx = pool.begin().await?;
        sqlx::query(DROP_TOKEN_SAVINGS_VIEW_SQL).execute(&mut *tx).await?;
        sqlx::query(CREATE_TOKEN_SAVINGS_VIEW_V46_SQL)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;

        info!("Migration v46 complete");
        Ok(())
    }

    fn version(&self) -> i32 {
        46
    }

    fn description(&self) -> &'static str {
        "Recreate token_savings with a unary-+ planner hint so the legacy-followup arm uses \
         idx_search_events_session_tool_ts instead of scanning the NULL-heavy parent index"
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

    /// The unary-+ must not change semantics — re-run the v45 behavioral
    /// matrix against the v46 view.
    #[tokio::test]
    async fn v46_view_preserves_v45_semantics() {
        use crate::search_events_schema::{
            CREATE_SEARCH_EVENTS_INDEXES_SQL, CREATE_SEARCH_EVENTS_SQL,
        };
        let pool = fresh_pool().await;
        sqlx::query(CREATE_SEARCH_EVENTS_SQL)
            .execute(&pool)
            .await
            .unwrap();
        for index_sql in CREATE_SEARCH_EVENTS_INDEXES_SQL {
            sqlx::query(index_sql).execute(&pool).await.unwrap();
        }
        crate::schema_version::v38::V38Migration
            .up(&pool)
            .await
            .unwrap();
        V46Migration.up(&pool).await.unwrap();
        // Idempotent re-run.
        V46Migration.up(&pool).await.unwrap();

        let ins = |id: &str, ts: &str, op: &str, session: Option<&str>, parent: Option<&str>, bytes: bool, rc: Option<i32>| {
            let pool = pool.clone();
            let (id, ts, op) = (id.to_string(), ts.to_string(), op.to_string());
            let session = session.map(|s| s.to_string());
            let parent = parent.map(|s| s.to_string());
            async move {
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
                .bind(if bytes { Some(5000i64) } else { None })
                .bind(if bytes { Some(1000i64) } else { None })
                .bind(rc)
                .execute(&pool)
                .await
                .unwrap();
            }
        };

        // parent-linked search (op preserved) → followup on origin
        ins("a", "2026-07-08T12:00:00.000Z", "search", Some("s1"), None, true, Some(5)).await;
        ins("b", "2026-07-08T12:00:30.000Z", "search", Some("s1"), Some("a"), true, Some(3)).await;
        // lone linked followup row must not self-match
        ins("self", "2026-07-08T12:10:00.000Z", "followup", Some("s1"), Some("x"), true, Some(3)).await;
        // unlinked external followup counts via the (hinted) legacy window
        ins("l0", "2026-07-08T12:40:00.000Z", "search", Some("s4"), None, true, Some(5)).await;
        ins("lf", "2026-07-08T12:40:30.000Z", "followup", Some("s4"), None, false, None).await;
        // refused retrieve does not escalate; delivered one does
        ins("e0", "2026-07-08T12:50:00.000Z", "search", Some("s5"), None, true, Some(5)).await;
        ins("r0", "2026-07-08T12:50:30.000Z", "retrieve", Some("s5"), Some("e0"), true, Some(0)).await;
        ins("r1", "2026-07-08T12:50:40.000Z", "retrieve", Some("s5"), Some("e0"), true, Some(1)).await;

        let probe = |id: &str| {
            let pool = pool.clone();
            let id = id.to_string();
            async move {
                sqlx::query_as::<_, (bool, bool)>(
                    "SELECT had_followup, had_escalation FROM token_savings WHERE id = ?1",
                )
                .bind(id)
                .fetch_one(&pool)
                .await
                .unwrap()
            }
        };
        assert_eq!(probe("a").await, (true, false));
        assert_eq!(probe("self").await, (false, false));
        assert_eq!(probe("l0").await, (true, false));
        assert_eq!(probe("e0").await, (false, true));
    }
}
