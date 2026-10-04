//! Generation-keyed writes: one file version's rows are replaced, and later
//! deleted, together — never another version's rows of the same path.

use sqlx::{Row, SqliteConnection};
use tracing::debug;
use wqm_common::timestamps::now_utc;

use super::SqliteGraphStore;
use crate::graph::{
    EdgeType, ExtractedGeneration, GraphDbError, GraphDbResult, GraphEdge, GraphNode,
};

/// Upsert one node row. A generation-less row (a stub, or a node another file
/// only referred to) is shared by every version that refers to it, so a later
/// writer only fills what it knows (`COALESCE`) and never blanks a file path.
/// `is_test_symbol` is a straight replace: the symbol's own file extraction is
/// authoritative and self-healing.
const UPSERT_NODE_SQL: &str = "INSERT INTO graph_nodes (node_id, generation, tenant_id,
        symbol_name, symbol_type, file_path, start_line, end_line, signature, language,
        is_test_symbol, created_at, updated_at)
    VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?12)
    ON CONFLICT(node_id, generation) DO UPDATE SET
        symbol_name = excluded.symbol_name,
        symbol_type = excluded.symbol_type,
        file_path = CASE WHEN excluded.file_path = '' THEN graph_nodes.file_path
                         ELSE excluded.file_path END,
        start_line = COALESCE(excluded.start_line, graph_nodes.start_line),
        end_line = COALESCE(excluded.end_line, graph_nodes.end_line),
        signature = COALESCE(excluded.signature, graph_nodes.signature),
        language = COALESCE(excluded.language, graph_nodes.language),
        is_test_symbol = excluded.is_test_symbol,
        updated_at = ?12";

pub(super) const INSERT_EDGE_SQL: &str = "INSERT OR IGNORE INTO graph_edges
        (edge_id, generation, tenant_id, source_node_id, target_node_id, edge_type,
         source_file, weight, metadata_json, created_at)
    VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)";

async fn write_node(conn: &mut SqliteConnection, node: &GraphNode, now: &str) -> sqlx::Result<()> {
    sqlx::query(UPSERT_NODE_SQL)
        .bind(&node.node_id)
        .bind(&node.generation)
        .bind(&node.tenant_id)
        .bind(&node.symbol_name)
        .bind(node.symbol_type.as_str())
        .bind(&node.file_path)
        .bind(node.start_line.map(|v| v as i64))
        .bind(node.end_line.map(|v| v as i64))
        .bind(&node.signature)
        .bind(&node.language)
        .bind(node.is_test_symbol as i64)
        .bind(now)
        .execute(conn)
        .await?;
    Ok(())
}

async fn write_edge(conn: &mut SqliteConnection, edge: &GraphEdge, now: &str) -> sqlx::Result<()> {
    sqlx::query(INSERT_EDGE_SQL)
        .bind(&edge.edge_id)
        .bind(&edge.generation)
        .bind(&edge.tenant_id)
        .bind(&edge.source_node_id)
        .bind(&edge.target_node_id)
        .bind(edge.edge_type.as_str())
        .bind(&edge.source_file)
        .bind(edge.weight)
        .bind(&edge.metadata_json)
        .bind(now)
        .execute(conn)
        .await?;
    Ok(())
}

impl SqliteGraphStore {
    pub(super) async fn write_nodes(&self, nodes: &[GraphNode]) -> GraphDbResult<()> {
        if nodes.is_empty() {
            return Ok(());
        }
        let now = now_utc();
        let mut tx = self.pool.begin().await?;
        for node in nodes {
            write_node(&mut tx, node, &now).await?;
        }
        tx.commit().await?;
        debug!("Upserted {} graph nodes", nodes.len());
        Ok(())
    }

    pub(super) async fn write_edges(&self, edges: &[GraphEdge]) -> GraphDbResult<()> {
        if edges.is_empty() {
            return Ok(());
        }
        let now = now_utc();
        let mut tx = self.pool.begin().await?;
        for edge in edges {
            write_edge(&mut tx, edge, &now).await?;
        }
        tx.commit().await?;
        debug!("Inserted {} graph edges", edges.len());
        Ok(())
    }

    /// Replace one generation's rows and record the extraction, in one
    /// transaction. The first statement is a write, so the transaction takes
    /// the write lock up front (no read-then-write upgrade to fail with BUSY).
    pub(super) async fn replace_generation_rows(
        &self,
        tenant_id: &str,
        file_path: &str,
        generation: &str,
        nodes: &[GraphNode],
        edges: &[GraphEdge],
    ) -> GraphDbResult<()> {
        // The empty generation is the shared namespace of stub rows; replacing
        // it would delete every stub of the tenant.
        if generation.is_empty() {
            return Err(GraphDbError::InvalidInput(format!(
                "replace_generation for {file_path}: empty generation"
            )));
        }
        let now = now_utc();
        let mut tx = self.pool.begin().await?;
        sqlx::query("DELETE FROM graph_edges WHERE tenant_id = ?1 AND generation = ?2")
            .bind(tenant_id)
            .bind(generation)
            .execute(&mut *tx)
            .await?;
        sqlx::query("DELETE FROM graph_nodes WHERE tenant_id = ?1 AND generation = ?2")
            .bind(tenant_id)
            .bind(generation)
            .execute(&mut *tx)
            .await?;
        for node in nodes {
            write_node(&mut tx, node, &now).await?;
        }
        for edge in edges {
            write_edge(&mut tx, edge, &now).await?;
        }
        let own_nodes = nodes.iter().filter(|n| n.generation == generation).count();
        sqlx::query(
            "INSERT INTO graph_generations
                (tenant_id, generation, file_path, node_count, edge_count, extracted_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)
             ON CONFLICT(tenant_id, generation) DO UPDATE SET
                file_path = excluded.file_path,
                node_count = excluded.node_count,
                edge_count = excluded.edge_count,
                extracted_at = excluded.extracted_at",
        )
        .bind(tenant_id)
        .bind(generation)
        .bind(file_path)
        .bind(own_nodes as i64)
        .bind(edges.len() as i64)
        .bind(&now)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        debug!(
            "Replaced graph generation {} of {} ({} nodes, {} edges)",
            generation,
            file_path,
            own_nodes,
            edges.len()
        );
        Ok(())
    }

    pub(super) async fn delete_generation_rows(
        &self,
        tenant_id: &str,
        generation: &str,
    ) -> GraphDbResult<u64> {
        if generation.is_empty() {
            return Ok(0);
        }
        let mut tx = self.pool.begin().await?;
        let edges = sqlx::query("DELETE FROM graph_edges WHERE tenant_id = ?1 AND generation = ?2")
            .bind(tenant_id)
            .bind(generation)
            .execute(&mut *tx)
            .await?
            .rows_affected();
        let nodes = sqlx::query("DELETE FROM graph_nodes WHERE tenant_id = ?1 AND generation = ?2")
            .bind(tenant_id)
            .bind(generation)
            .execute(&mut *tx)
            .await?
            .rows_affected();
        sqlx::query("DELETE FROM graph_generations WHERE tenant_id = ?1 AND generation = ?2")
            .bind(tenant_id)
            .bind(generation)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        debug!(
            "Deleted graph generation {} of tenant {} ({} nodes, {} edges)",
            generation, tenant_id, nodes, edges
        );
        Ok(nodes + edges)
    }

    pub(super) async fn is_generation_extracted(
        &self,
        tenant_id: &str,
        generation: &str,
    ) -> GraphDbResult<bool> {
        let row: Option<i64> = sqlx::query_scalar(
            "SELECT 1 FROM graph_generations WHERE tenant_id = ?1 AND generation = ?2",
        )
        .bind(tenant_id)
        .bind(generation)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.is_some())
    }

    pub(super) async fn list_extracted_generations(
        &self,
        tenant_id: &str,
    ) -> GraphDbResult<Vec<ExtractedGeneration>> {
        let rows = sqlx::query(
            "SELECT generation, file_path, extracted_at FROM graph_generations
             WHERE tenant_id = ?1",
        )
        .bind(tenant_id)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows
            .iter()
            .map(|r| ExtractedGeneration {
                generation: r.get("generation"),
                file_path: r.get("file_path"),
                extracted_at: r.get("extracted_at"),
            })
            .collect())
    }

    pub(super) async fn delete_tenant_rows(&self, tenant_id: &str) -> GraphDbResult<u64> {
        let mut tx = self.pool.begin().await?;
        let mut total = 0;
        for table in ["graph_edges", "graph_nodes", "graph_generations"] {
            total += sqlx::query(&format!("DELETE FROM {table} WHERE tenant_id = ?1"))
                .bind(tenant_id)
                .execute(&mut *tx)
                .await?
                .rows_affected();
        }
        tx.commit().await?;
        debug!("Deleted {} graph rows for tenant {}", total, tenant_id);
        Ok(total)
    }

    pub(super) async fn delete_orphan_nodes(&self, tenant_id: &str) -> GraphDbResult<u64> {
        let result = sqlx::query(
            "DELETE FROM graph_nodes
             WHERE tenant_id = ?1
               AND node_id NOT IN (
                   SELECT source_node_id FROM graph_edges WHERE tenant_id = ?1
                   UNION
                   SELECT target_node_id FROM graph_edges WHERE tenant_id = ?1
               )",
        )
        .bind(tenant_id)
        .execute(&self.pool)
        .await?;
        let count = result.rows_affected();
        debug!("Pruned {} orphaned nodes for tenant {}", count, tenant_id);
        Ok(count)
    }

    pub(super) async fn supersede_fuzzy_calls(
        &self,
        tenant_id: &str,
        caller_id: &str,
        source_file: &str,
        generation: &str,
        resolved_names: &[String],
        precise_targets: &[String],
    ) -> GraphDbResult<u64> {
        if resolved_names.is_empty() {
            return Ok(0);
        }
        let now = now_utc();
        let mut tx = self.pool.begin().await?;

        // Drop the caller's fuzzy CALLS in THIS version of its file to ANY node
        // named one of resolved_names (the by-name fan-out — including a
        // pre-existing edge to the precise target, which is re-inserted below at
        // full confidence). Another version of the file is another branch's
        // answer and keeps its own edges.
        let name_ph: Vec<String> = (0..resolved_names.len())
            .map(|i| format!("?{}", i + 4))
            .collect();
        let del_sql = format!(
            "DELETE FROM graph_edges
             WHERE tenant_id = ?1 AND edge_type = 'CALLS' AND source_node_id = ?2
               AND generation = ?3
               AND target_node_id IN (
                   SELECT node_id FROM graph_nodes
                   WHERE tenant_id = ?1 AND symbol_name IN ({})
               )",
            name_ph.join(", ")
        );
        let mut dq = sqlx::query(&del_sql)
            .bind(tenant_id)
            .bind(caller_id)
            .bind(generation);
        for n in resolved_names {
            dq = dq.bind(n);
        }
        let deleted = dq.execute(&mut *tx).await?.rows_affected();

        for target in precise_targets {
            if target == caller_id {
                continue; // no self-loops
            }
            let mut edge =
                GraphEdge::new(tenant_id, caller_id, target, EdgeType::Calls, source_file);
            edge.generation = generation.to_string();
            edge.metadata_json = Some("{\"resolution\":\"lsp\"}".to_string());
            write_edge(&mut tx, &edge, &now).await?;
        }

        tx.commit().await?;
        debug!(
            "make_calls_authoritative: caller {} dropped {} fuzzy, added {} precise",
            caller_id,
            deleted,
            precise_targets.len()
        );
        Ok(deleted)
    }
}
