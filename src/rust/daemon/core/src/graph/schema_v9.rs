//! graph.db v9: each extraction records the extractor that wrote it.

use sqlx::SqlitePool;
use tracing::info;

use super::GraphDbResult;

/// Version of what extraction writes for unchanged bytes: nodes, edges and
/// the hints on them. Bump it whenever a change to the extractor (or to the
/// metadata the resolver reads) makes an existing generation's rows stale —
/// every generation recorded with an older version then counts as not
/// extracted, and the idle backfill and the dedup heal rebuild it IN PLACE:
/// its old rows keep answering until its new ones replace them, which a
/// schema wipe (v7, v8) could not offer.
///
/// - 1: receiver hints read the whole definition (every fragment of a split
///   one), a class named as the receiver types a static call, an untyped
///   site no longer cancels the typed ones; a split definition is one node
///   spanning its fragments.
pub const GRAPH_EXTRACTOR_VERSION: i64 = 1;

/// v9: `graph_generations.extractor_version`. Existing extractions get 0, so
/// every generation is rebuilt once by the current extractor. Also drops the
/// callee nodes the ingest-time LSP pass used to guess (see
/// `drop_guessed_callee_nodes`).
pub(super) async fn migrate_v9(pool: &SqlitePool) -> GraphDbResult<()> {
    info!("Graph migration v9: extractions record their extractor version (existing ones are rebuilt in place by the idle backfill)");
    sqlx::query(
        "ALTER TABLE graph_generations ADD COLUMN extractor_version INTEGER NOT NULL DEFAULT 0",
    )
    .execute(pool)
    .await?;
    drop_guessed_callee_nodes(pool).await
}

/// The ingest-time LSP pass once minted a `Function` node for every callee
/// in ANOTHER file (a method's real id includes its class, so the guess
/// matched nothing). Stamped into the shared empty generation with a real
/// file path, such a node belongs to no extraction — no rebuild replaces it,
/// and the stub sweep only takes file-less rows — while it poses as a
/// definition to every by-name pick. Nothing writes these rows any more; drop
/// them and the edges into them (their callers' generations are rebuilt by
/// the extractor version anyway).
async fn drop_guessed_callee_nodes(pool: &SqlitePool) -> GraphDbResult<()> {
    let mut tx = pool.begin().await?;
    let edges = sqlx::query(
        "DELETE FROM graph_edges WHERE target_node_id IN (
             SELECT node_id FROM graph_nodes WHERE generation = '' AND file_path <> '')",
    )
    .execute(&mut *tx)
    .await?
    .rows_affected();
    let nodes = sqlx::query("DELETE FROM graph_nodes WHERE generation = '' AND file_path <> ''")
        .execute(&mut *tx)
        .await?
        .rows_affected();
    tx.commit().await?;
    info!(
        "Graph migration v9: dropped {nodes} guessed callee node(s) and {edges} edge(s) into them"
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::graph::{
        EdgeType, GraphEdge, GraphNode, NodeType, SharedGraphStore, SqliteGraphStore,
    };

    #[tokio::test]
    async fn guessed_callee_nodes_go_and_definitions_and_stubs_stay() {
        let pool = sqlx::sqlite::SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .unwrap();
        crate::graph::schema::apply_graph_schema(&pool).await;
        let store = SharedGraphStore::new(SqliteGraphStore::new(pool.clone()));
        let caller = GraphNode::new("t", "a.dart", "run", NodeType::Function);
        let real = GraphNode::member("t", "b.dart", "set", Some("Batch"), NodeType::Method);
        let guessed = GraphNode::new("t", "b.dart", "set", NodeType::Function);
        let stub = GraphNode::stub("t", "print", NodeType::Function);
        let (nodes, edges) = crate::graph::stamp_generation(
            "a.dart",
            "a1",
            &[caller.clone(), guessed.clone(), stub.clone()],
            &[
                GraphEdge::new(
                    "t",
                    &caller.node_id,
                    &guessed.node_id,
                    EdgeType::Calls,
                    "a.dart",
                ),
                GraphEdge::new(
                    "t",
                    &caller.node_id,
                    &stub.node_id,
                    EdgeType::Calls,
                    "a.dart",
                ),
            ],
        );
        store.upsert_nodes(&nodes).await.unwrap();
        store.insert_edges(&edges).await.unwrap();
        let (real_rows, _) = crate::graph::stamp_generation("b.dart", "b1", &[real], &[]);
        store.upsert_nodes(&real_rows).await.unwrap();

        drop_guessed_callee_nodes(&pool).await.unwrap();

        let left: Vec<(String, String)> =
            sqlx::query_as("SELECT symbol_name, symbol_type FROM graph_nodes ORDER BY 1, 2")
                .fetch_all(&pool)
                .await
                .unwrap();
        assert_eq!(
            left,
            [
                ("print", "function"),
                ("run", "function"),
                ("set", "method")
            ]
            .map(|(n, t)| (n.to_string(), t.to_string()))
        );
        let edges: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM graph_edges")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(edges, 1, "only the edge into the stub is left");
    }
}
