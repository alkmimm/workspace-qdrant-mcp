//! graph.db v7: rows keyed by content generation (the branch-aware graph).
//!
//! Its own module because it replaces the graph tables wholesale; the
//! linear migration chain in `schema.rs` calls it like any other step.

use sqlx::SqlitePool;
use tracing::{info, warn};

use super::GraphDbResult;

/// v7: rows keyed by content GENERATION — the branch-aware graph.
///
/// Until v6 a node was keyed by `(tenant, file, symbol, type)` alone, so the
/// graph held ONE version of each file: whichever branch's copy was
/// extracted last. A worktree branch that rewrote a file replaced the
/// trunk's symbols for everyone, and a file deleted on a branch stayed in
/// that branch's answers. Every row now carries the generation that
/// produced it (the tracked file's `base_point`), the primary keys include
/// it, and `graph_generations` records each extraction so a generation
/// with no symbols is not mistaken for a missing one.
///
/// The existing rows cannot be carried over: nothing recorded which version
/// of a file produced them. They are dropped and the graph is rebuilt from
/// the files on disk by the idle backfill (tree-sitter only, no embedding).
/// The foreign keys go too: `node_id` alone is no longer unique, and the
/// generation-scoped delete never leaves an edge pointing into its own
/// generation's deleted nodes. VACUUM returns the old tables' pages to the
/// filesystem (the live graph.db was 4.3 GB).
pub(super) async fn migrate_v7(pool: &SqlitePool) -> GraphDbResult<()> {
    info!("Graph migration v7: per-generation graph rows (existing rows dropped, rebuilt by the idle backfill)");
    let now = "(strftime('%Y-%m-%dT%H:%M:%fZ', 'now'))";
    let statements = [
        "DROP TABLE IF EXISTS graph_edges".to_string(),
        "DROP TABLE IF EXISTS graph_nodes".to_string(),
        format!(
            "CREATE TABLE graph_nodes (
                node_id TEXT NOT NULL,
                generation TEXT NOT NULL DEFAULT '',
                tenant_id TEXT NOT NULL,
                symbol_name TEXT NOT NULL,
                symbol_type TEXT NOT NULL,
                file_path TEXT NOT NULL,
                start_line INTEGER,
                end_line INTEGER,
                signature TEXT,
                language TEXT,
                is_test_symbol INTEGER NOT NULL DEFAULT 0,
                created_at TEXT NOT NULL DEFAULT {now},
                updated_at TEXT NOT NULL DEFAULT {now},
                PRIMARY KEY (node_id, generation)
            )"
        ),
        "CREATE INDEX idx_nodes_tenant ON graph_nodes(tenant_id)".to_string(),
        "CREATE INDEX idx_nodes_file ON graph_nodes(tenant_id, file_path)".to_string(),
        "CREATE INDEX idx_nodes_symbol ON graph_nodes(tenant_id, symbol_name)".to_string(),
        "CREATE INDEX idx_nodes_generation ON graph_nodes(tenant_id, generation)".to_string(),
        format!(
            "CREATE TABLE graph_edges (
                edge_id TEXT NOT NULL,
                generation TEXT NOT NULL DEFAULT '',
                tenant_id TEXT NOT NULL,
                source_node_id TEXT NOT NULL,
                target_node_id TEXT NOT NULL,
                edge_type TEXT NOT NULL,
                source_file TEXT NOT NULL,
                weight REAL DEFAULT 1.0,
                metadata_json TEXT,
                created_at TEXT NOT NULL DEFAULT {now},
                PRIMARY KEY (edge_id, generation)
            )"
        ),
        "CREATE INDEX idx_edges_tenant_source ON graph_edges(tenant_id, source_node_id)"
            .to_string(),
        "CREATE INDEX idx_edges_tenant_target ON graph_edges(tenant_id, target_node_id)"
            .to_string(),
        "CREATE INDEX idx_edges_tenant_type ON graph_edges(tenant_id, edge_type)".to_string(),
        "CREATE INDEX idx_edges_source_file ON graph_edges(tenant_id, source_file)".to_string(),
        "CREATE INDEX idx_edges_generation ON graph_edges(tenant_id, generation)".to_string(),
        "CREATE TABLE IF NOT EXISTS graph_generations (
            tenant_id TEXT NOT NULL,
            generation TEXT NOT NULL,
            file_path TEXT NOT NULL,
            node_count INTEGER NOT NULL DEFAULT 0,
            edge_count INTEGER NOT NULL DEFAULT 0,
            extracted_at TEXT NOT NULL,
            PRIMARY KEY (tenant_id, generation)
        )"
        .to_string(),
    ];
    let mut tx = pool.begin().await?;
    for statement in &statements {
        sqlx::query(statement).execute(&mut *tx).await?;
    }
    tx.commit().await?;
    // Outside the transaction (VACUUM cannot run inside one). A failure only
    // leaves the freed pages in the file; the schema is already in place.
    if let Err(e) = sqlx::query("VACUUM").execute(pool).await {
        warn!("Graph migration v7: VACUUM failed (freed pages stay in graph.db): {e}");
    }
    Ok(())
}
