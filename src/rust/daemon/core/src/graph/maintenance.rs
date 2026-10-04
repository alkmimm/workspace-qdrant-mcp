//! Graph maintenance that needs the index authority (`state.db`).
//!
//! A generation's graph rows are deleted by the delete path the moment its
//! tracked row disappears; this sweep catches the ones that path never saw —
//! a graph written for an ingest that then failed before its tracked row
//! committed, a delete that errored, a generation dropped while the daemon
//! was down. It replaces the per-path ghost sweep (#245), which could not tell
//! one version of a path from another.

use std::collections::HashSet;
use std::time::Duration;

use chrono::{DateTime, Utc};
use sqlx::SqlitePool;
use tracing::{info, warn};

use super::branch_scope::tracked_generations;
use super::{SharedGraphStore, SqliteGraphStore};

/// How long an extracted generation may go without a tracked row before it is
/// retired. The ingest pipeline writes the graph (phase 4) BEFORE the tracked
/// row (phase 6), so a fresh generation is briefly unknown to the authority.
pub const ORPHAN_GENERATION_GRACE: Duration = Duration::from_secs(600);

/// What one sweep removed.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct OrphanSweep {
    pub generations: usize,
    pub rows: u64,
}

/// Delete every extracted generation no tracked file references any more,
/// once it is older than `grace`.
///
/// DESTRUCTIVE, so it refuses to act on doubt: a tenant whose authority query
/// fails is skipped, and so is one whose authority is EMPTY while the graph is
/// not — an empty walk over a non-empty index is a failure, not truth (a
/// tenant that is really gone is removed whole by the tenant delete).
pub async fn sweep_orphan_generations(
    graph: &SharedGraphStore<SqliteGraphStore>,
    state_pool: &SqlitePool,
    now: DateTime<Utc>,
    grace: Duration,
) -> OrphanSweep {
    let mut sweep = OrphanSweep::default();
    let tenants: Vec<String> = {
        let guard = graph.read().await;
        sqlx::query_scalar("SELECT DISTINCT tenant_id FROM graph_generations")
            .fetch_all(guard.pool())
            .await
            .unwrap_or_default()
    };
    for tenant in tenants {
        let tracked: HashSet<String> = match tracked_generations(state_pool, &tenant).await {
            Ok(t) if !t.is_empty() => t,
            Ok(_) => continue,
            Err(e) => {
                warn!(tenant = %tenant, error = %e, "graph generation sweep: tracked_files query failed — skipping tenant");
                continue;
            }
        };
        let extracted = match graph.extracted_generations(&tenant).await {
            Ok(x) => x,
            Err(e) => {
                warn!(tenant = %tenant, error = %e, "graph generation sweep: listing generations failed — skipping tenant");
                continue;
            }
        };
        let mut removed = 0usize;
        for g in extracted {
            if tracked.contains(&g.generation) || !older_than(&g.extracted_at, now, grace) {
                continue;
            }
            match graph.delete_generation(&tenant, &g.generation).await {
                Ok(rows) => {
                    removed += 1;
                    sweep.rows += rows;
                }
                Err(e) => warn!(tenant = %tenant, generation = %g.generation, error = %e,
                    "graph generation sweep: delete failed"),
            }
        }
        if removed > 0 {
            info!(tenant = %tenant, generations = removed,
                "Graph generation sweep retired generations no tracked file references");
        }
        sweep.generations += removed;
    }
    sweep
}

/// Whether an ISO-8601 stamp is more than `grace` before `now`. An unparsable
/// stamp is treated as fresh: never delete on a value we cannot read.
fn older_than(stamp: &str, now: DateTime<Utc>, grace: Duration) -> bool {
    match DateTime::parse_from_rfc3339(stamp) {
        Ok(at) => now
            .signed_duration_since(at.with_timezone(&Utc))
            .to_std()
            .is_ok_and(|age| age > grace),
        Err(_) => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::graph::{GraphNode, NodeType};
    use sqlx::sqlite::SqlitePoolOptions;

    async fn memory_pool() -> SqlitePool {
        SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .unwrap()
    }

    async fn state_with(generations: &[&str]) -> SqlitePool {
        let pool = memory_pool().await;
        sqlx::query("CREATE TABLE watch_folders (watch_id TEXT PRIMARY KEY, tenant_id TEXT)")
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query(
            "CREATE TABLE tracked_files (watch_folder_id TEXT, relative_path TEXT,
                base_point TEXT, branches TEXT)",
        )
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query("INSERT INTO watch_folders VALUES ('w', 't')")
            .execute(&pool)
            .await
            .unwrap();
        for g in generations {
            sqlx::query(r#"INSERT INTO tracked_files VALUES ('w', ?1, ?1, '["main"]')"#)
                .bind(g)
                .execute(&pool)
                .await
                .unwrap();
        }
        pool
    }

    async fn graph_with(generations: &[&str]) -> SharedGraphStore<SqliteGraphStore> {
        let pool = memory_pool().await;
        crate::graph::schema::apply_graph_schema(&pool).await;
        let graph = SharedGraphStore::new(SqliteGraphStore::new(pool));
        for g in generations {
            let file = format!("{g}.rs");
            let node = GraphNode::new("t", &file, "f", NodeType::Function);
            graph
                .reingest_file("t", &file, g, &[node], &[])
                .await
                .unwrap();
        }
        graph
    }

    fn later(minutes: i64) -> DateTime<Utc> {
        Utc::now() + chrono::Duration::minutes(minutes)
    }

    #[tokio::test]
    async fn retires_only_untracked_generations_past_the_grace() {
        let graph = graph_with(&["kept", "gone"]).await;
        let state = state_with(&["kept"]).await;

        // Within the grace window nothing goes: the tracked row may still be
        // on its way (graph is written before tracked_files).
        let early =
            sweep_orphan_generations(&graph, &state, Utc::now(), ORPHAN_GENERATION_GRACE).await;
        assert_eq!(early.generations, 0);

        let swept =
            sweep_orphan_generations(&graph, &state, later(11), ORPHAN_GENERATION_GRACE).await;
        assert_eq!(
            swept,
            OrphanSweep {
                generations: 1,
                rows: 1
            }
        );
        assert!(graph.generation_extracted("t", "kept").await.unwrap());
        assert!(!graph.generation_extracted("t", "gone").await.unwrap());
    }

    #[tokio::test]
    async fn an_empty_authority_retires_nothing() {
        let graph = graph_with(&["a", "b"]).await;
        let state = state_with(&[]).await;
        let swept =
            sweep_orphan_generations(&graph, &state, later(60), ORPHAN_GENERATION_GRACE).await;
        assert_eq!(
            swept.generations, 0,
            "an empty walk over a non-empty graph is a failure"
        );
        assert!(graph.generation_extracted("t", "a").await.unwrap());
    }

    #[test]
    fn an_unreadable_stamp_counts_as_fresh() {
        assert!(!older_than("not a time", Utc::now(), Duration::ZERO));
        assert!(older_than(
            "2020-01-01T00:00:00.000Z",
            Utc::now(),
            Duration::from_secs(1)
        ));
    }
}
