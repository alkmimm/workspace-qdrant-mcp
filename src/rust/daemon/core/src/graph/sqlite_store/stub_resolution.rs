//! By-name resolution of stub edges (tree-sitter's name-only references).

use std::collections::HashSet;

use sqlx::sqlite::SqliteRow;
use sqlx::{Row, Sqlite, Transaction};
use tracing::debug;
use wqm_common::timestamps::now_utc;

use super::candidate_pick::{CandidateIndex, CandidateScope, StubRef};
use super::resolution_tiers::{receiver_hint, resolution_metadata};
use super::writes::INSERT_EDGE_SQL;
use super::SqliteGraphStore;
use crate::graph::lsp_sites::lsp_sites;
use crate::graph::{compute_edge_id, EdgeType, GenerationBranches, GraphDbResult};

/// Position in the tenant/edge type index. The rowid breaks ties within each
/// edge type and lets the scan advance past persistently unresolved stubs.
#[derive(Default)]
pub struct StubResolutionCursor {
    edge_type: String,
    rowid: i64,
}

const BATCH_SIZE: i64 = 1024;

impl SqliteGraphStore {
    pub(super) async fn resolve_stubs(
        &self,
        tenant_id: &str,
        membership: &GenerationBranches,
    ) -> GraphDbResult<u64> {
        let upper = self.stub_sweep_upper_bound().await?;
        let mut cursor = StubResolutionCursor::default();
        let mut repointed = 0;
        loop {
            let (count, next) = self
                .resolve_stub_batch(tenant_id, membership, upper, &cursor)
                .await?;
            repointed += count;
            let Some(next) = next else {
                return Ok(repointed);
            };
            cursor = next;
        }
    }

    /// Bound one sweep even while indexing inserts new graph rows.
    pub(crate) async fn stub_sweep_upper_bound(&self) -> GraphDbResult<i64> {
        Ok(
            sqlx::query_scalar("SELECT COALESCE(MAX(rowid), 0) FROM graph_edges")
                .fetch_one(&self.pool)
                .await?,
        )
    }

    pub(crate) async fn resolve_stub_batch(
        &self,
        tenant_id: &str,
        membership: &GenerationBranches,
        upper_rowid: i64,
        cursor: &StubResolutionCursor,
    ) -> GraphDbResult<(u64, Option<StubResolutionCursor>)> {
        // Two ranges preserve index seeks both inside the current edge type
        // and when crossing to the next type. Inspect only the merged page,
        // using the nodes' primary key to check either endpoint for a stub.
        // With production statistics SQLite otherwise chose idx_nodes_file,
        // scanning the tenant's stub nodes again for each endpoint.
        let rows = sqlx::query(
            "WITH same_type AS MATERIALIZED (
                SELECT rowid, edge_type FROM graph_edges INDEXED BY idx_edges_tenant_type
                WHERE tenant_id = ?1 AND edge_type = ?2 AND rowid > ?3 AND rowid <= ?4
                ORDER BY rowid LIMIT ?5
             ), later_types AS MATERIALIZED (
                SELECT rowid, edge_type FROM graph_edges INDEXED BY idx_edges_tenant_type
                WHERE tenant_id = ?1 AND edge_type > ?2 AND rowid <= ?4
                ORDER BY edge_type, rowid LIMIT ?5
             ), page AS MATERIALIZED (
                SELECT * FROM same_type UNION ALL SELECT * FROM later_types
                ORDER BY edge_type, rowid LIMIT ?5
             )
             SELECT p.rowid AS scan_rowid, e.*,
                 (SELECT n.symbol_name FROM graph_nodes n INDEXED BY sqlite_autoindex_graph_nodes_1
                  WHERE n.node_id = e.target_node_id
                  AND n.tenant_id = ?1 AND n.file_path = '' LIMIT 1) AS target_name,
                 (SELECT n.symbol_name FROM graph_nodes n INDEXED BY sqlite_autoindex_graph_nodes_1
                  WHERE n.node_id = e.source_node_id
                  AND n.tenant_id = ?1 AND n.file_path = '' LIMIT 1) AS source_name
             FROM page p JOIN graph_edges e ON e.rowid = p.rowid
             ORDER BY p.edge_type, p.rowid",
        )
        .bind(tenant_id)
        .bind(&cursor.edge_type)
        .bind(cursor.rowid)
        .bind(upper_rowid)
        .bind(BATCH_SIZE)
        .fetch_all(&self.pool)
        .await?;

        let next = rows
            .last()
            .filter(|_| rows.len() as i64 == BATCH_SIZE)
            .map(|last| StubResolutionCursor {
                edge_type: last.get("edge_type"),
                rowid: last.get("scan_rowid"),
            });
        let (mut targets, mut sources) = (Vec::new(), Vec::new());
        let mut scope = CandidateScope::default();
        let mut stub_ids = HashSet::new();
        for row in &rows {
            if let Some(name) = row.get::<Option<String>, _>("target_name") {
                scope.names.insert(name);
                scope.caller_ids.insert(row.get("source_node_id"));
                scope.source_files.insert(row.get("source_file"));
                if let Some(hint) = receiver_hint(row.get::<Option<&str>, _>("metadata_json")) {
                    scope.names.extend(hint.types);
                    scope.names.extend(hint.static_types);
                }
                stub_ids.insert(row.get::<String, _>("target_node_id"));
                targets.push(row);
            }
            if let Some(name) = row.get::<Option<String>, _>("source_name") {
                scope.names.insert(name);
                scope.caller_ids.insert(row.get("target_node_id"));
                scope.source_files.insert(row.get("source_file"));
                stub_ids.insert(row.get::<String, _>("source_node_id"));
                sources.push(row);
            }
        }
        if targets.is_empty() && sources.is_empty() {
            return Ok((0, next));
        }

        let index = CandidateIndex::load(&self.pool, tenant_id, membership, &scope).await?;
        let now = now_utc();
        let mut tx = self.pool.begin().await?;
        let mut repointed =
            repoint_target_stubs(&mut tx, tenant_id, &targets, &index, &now).await?;
        repointed += repoint_source_stubs(&mut tx, tenant_id, &sources, &index, &now).await?;
        // As before, unresolved REFERENCES are usually local variables that
        // merely resembled a top-level symbol. Drop only rows from this page.
        for row in targets
            .iter()
            .filter(|r| r.get::<&str, _>("edge_type") == "REFERENCES")
        {
            delete_edge_row(
                &mut tx,
                tenant_id,
                row.get("edge_id"),
                row.get("generation"),
            )
            .await?;
        }
        sqlx::query(
            "DELETE FROM graph_nodes AS n INDEXED BY sqlite_autoindex_graph_nodes_1
             WHERE n.tenant_id = ?1 AND n.file_path = ''
               AND n.node_id IN (SELECT value FROM json_each(?2))
               AND NOT EXISTS (SELECT 1 FROM graph_edges e
                               WHERE e.tenant_id = ?1 AND e.source_node_id = n.node_id)
               AND NOT EXISTS (SELECT 1 FROM graph_edges e
                               WHERE e.tenant_id = ?1 AND e.target_node_id = n.node_id)",
        )
        .bind(tenant_id)
        .bind(sqlx::types::Json(&stub_ids))
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        debug!(
            tenant_id,
            repointed,
            targets = targets.len(),
            sources = sources.len(),
            "Resolved stub edge page"
        );
        Ok((repointed, next))
    }
}

/// Pass 1 — target-stub edges: repoint the TARGET to the real node(s), in the
/// edge's own generation (it stays owned by the same file version), each
/// stamped with its confidence and resolution provenance.
async fn repoint_target_stubs(
    tx: &mut Transaction<'_, Sqlite>,
    tenant_id: &str,
    dangling: &[&SqliteRow],
    index: &CandidateIndex<'_>,
    now: &str,
) -> GraphDbResult<u64> {
    let mut repointed = 0;
    for d in dangling {
        let peer_name: String = d.get("target_name");
        let source_file: String = d.get("source_file");
        let source_node_id: String = d.get("source_node_id");
        let generation: String = d.get("generation");
        let metadata = d.get::<Option<String>, _>("metadata_json");
        let receiver = receiver_hint(metadata.as_deref());
        let lsp = lsp_sites(metadata.as_deref());
        let picked = index.pick_all(&StubRef {
            name: &peer_name,
            own_file: &source_file,
            generation: &generation,
            caller_class: index.container_of(&source_node_id),
            caller_lang: index.language_of(&source_node_id),
            container_only: false,
            receiver: receiver.as_ref(),
            lsp: lsp.as_ref(),
        });
        let candidates = picked.targets;
        if candidates.is_empty() {
            continue; // external/stdlib or unresolved — leave it a stub.
        }
        let Some(edge_type) = EdgeType::from_str(&d.get::<String, _>("edge_type")) else {
            continue;
        };
        let mut emitted = false;
        for (new_target, confidence) in &candidates {
            // Skip self-loops (e.g. direct recursion) — no signal.
            if &source_node_id == new_target {
                continue;
            }
            sqlx::query(INSERT_EDGE_SQL)
                .bind(compute_edge_id(&source_node_id, new_target, edge_type))
                .bind(&generation)
                .bind(tenant_id)
                .bind(&source_node_id)
                .bind(new_target)
                .bind(edge_type.as_str())
                .bind(&source_file)
                .bind(*confidence)
                .bind(resolution_metadata(
                    *confidence,
                    candidates.len(),
                    picked.located,
                ))
                .bind(now)
                .execute(&mut **tx)
                .await?;
            emitted = true;
        }
        if emitted {
            delete_edge_row(tx, tenant_id, &d.get::<String, _>("edge_id"), &generation).await?;
            repointed += 1;
        }
    }
    Ok(repointed)
}

/// Pass 2 — source-stub edges (CONTAINS from a file-less container stub):
/// repoint the SOURCE to the real container node of the same name.
async fn repoint_source_stubs(
    tx: &mut Transaction<'_, Sqlite>,
    tenant_id: &str,
    dangling: &[&SqliteRow],
    index: &CandidateIndex<'_>,
    now: &str,
) -> GraphDbResult<u64> {
    let mut repointed = 0;
    for d in dangling {
        let peer_name: String = d.get("source_name");
        let source_file: String = d.get("source_file");
        let target_node_id: String = d.get("target_node_id");
        let generation: String = d.get("generation");
        // Containment is structural (one owner): keep ONLY a confident match
        // (own-file or unique), never fan out an ambiguous container name. A
        // container and its member share a language.
        let Some(new_source) = index
            .pick_all(&StubRef {
                name: &peer_name,
                own_file: &source_file,
                generation: &generation,
                caller_class: None,
                caller_lang: index.language_of(&target_node_id),
                container_only: true,
                receiver: None,
                lsp: None,
            })
            .targets
            .into_iter()
            .find(|(_, c)| *c >= 0.7)
            .map(|(nid, _)| nid)
        else {
            continue;
        };
        if target_node_id == new_source {
            continue;
        }
        let Some(edge_type) = EdgeType::from_str(&d.get::<String, _>("edge_type")) else {
            continue;
        };
        sqlx::query(INSERT_EDGE_SQL)
            .bind(compute_edge_id(&new_source, &target_node_id, edge_type))
            .bind(&generation)
            .bind(tenant_id)
            .bind(&new_source)
            .bind(&target_node_id)
            .bind(edge_type.as_str())
            .bind(&source_file)
            .bind(d.get::<f64, _>("weight"))
            .bind(d.get::<Option<String>, _>("metadata_json"))
            .bind(now)
            .execute(&mut **tx)
            .await?;
        delete_edge_row(tx, tenant_id, &d.get::<String, _>("edge_id"), &generation).await?;
        repointed += 1;
    }
    Ok(repointed)
}

async fn delete_edge_row(
    tx: &mut Transaction<'_, Sqlite>,
    tenant_id: &str,
    edge_id: &str,
    generation: &str,
) -> sqlx::Result<()> {
    sqlx::query(
        "DELETE FROM graph_edges WHERE edge_id = ?1 AND generation = ?2 AND tenant_id = ?3",
    )
    .bind(edge_id)
    .bind(generation)
    .bind(tenant_id)
    .execute(&mut **tx)
    .await?;
    Ok(())
}
