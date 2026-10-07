//! graph.db v8: a class member's identity includes its container.

use sqlx::SqlitePool;
use tracing::{info, warn};

use super::GraphDbResult;

/// v8: `graph_nodes.parent_symbol`, and member node ids that include it.
///
/// Until v8 a node was keyed by `(tenant, file, symbol, type)` alone, so two
/// classes in one file that declared a method of the same name produced ONE
/// node, and the last one written won (live 2026-10-07: Finance's
/// `FirestoreFinanceBatch.set` was missing, every call to it landed on
/// `FirestoreFinanceTransaction.set`, and test_gaps reported it untested). See
/// `compute_member_node_id`.
///
/// The collapsed members cannot be recovered in place, so the rows are dropped
/// together with `graph_generations` — every generation then counts as
/// missing — and the idle backfill rebuilds the graph from the files, exactly
/// as v7 did. VACUUM returns the freed pages.
pub(super) async fn migrate_v8(pool: &SqlitePool) -> GraphDbResult<()> {
    info!("Graph migration v8: member identity includes its container (existing rows dropped, rebuilt by the idle backfill)");
    let statements = [
        "DELETE FROM graph_edges",
        "DELETE FROM graph_nodes",
        "DELETE FROM graph_generations",
        "ALTER TABLE graph_nodes ADD COLUMN parent_symbol TEXT",
    ];
    let mut tx = pool.begin().await?;
    for statement in statements {
        sqlx::query(statement).execute(&mut *tx).await?;
    }
    tx.commit().await?;
    if let Err(e) = sqlx::query("VACUUM").execute(pool).await {
        warn!("Graph migration v8: VACUUM failed (freed pages stay in graph.db): {e}");
    }
    Ok(())
}
