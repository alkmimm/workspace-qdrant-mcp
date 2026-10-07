//! By-name resolution of stub edges (tree-sitter's name-only references).

use sqlx::sqlite::SqliteRow;
use sqlx::{Row, Sqlite, Transaction};
use tracing::debug;
use wqm_common::timestamps::now_utc;

use super::candidate_pick::{CandidateIndex, StubRef};
use super::resolution_tiers::{receiver_types, resolution_metadata};
use super::writes::INSERT_EDGE_SQL;
use super::SqliteGraphStore;
use crate::graph::lsp_sites::lsp_sites;
use crate::graph::{compute_edge_id, EdgeType, GenerationBranches, GraphDbResult};

impl SqliteGraphStore {
    pub(super) async fn resolve_stubs(
        &self,
        tenant_id: &str,
        membership: &GenerationBranches,
    ) -> GraphDbResult<u64> {
        // Dangling edges come in two orientations, both keyed on a file-less
        // stub node:
        //   - target-stub: CALLS / IMPORTS / USES_TYPE point at a name-only
        //     callee / module / type whose defining file is unknown.
        //   - source-stub: CONTAINS is authored from a file-less *parent
        //     container* stub — the class/struct node is created file-anchored
        //     from its OWN chunk, so the CONTAINS edge otherwise never lands on
        //     it (this is why `relations(class, filePath)` listed no members).
        // Both are repointed by name to the real project node; the file-less
        // stub is dropped once it has no edges left.
        // Both dangling queries drive from the small file-less-node set, then
        // probe edges by node id, forced with CROSS JOIN (loop order: nodes
        // outer) so the join starts from the file-less nodes instead of
        // full-scanning every edge of the tenant. `file_path = ''` lets the
        // plain composite index idx_nodes_file(tenant_id, file_path) drive the
        // scan with no `INDEXED BY` hint (a hint on a *partial* index failed the
        // whole query with "no query solution" on the daemon's SQLite).
        let target_dangling = sqlx::query(
            "SELECT e.edge_id, e.generation, e.source_node_id, e.edge_type, e.source_file,
                    e.weight, e.metadata_json, t.symbol_name AS peer_name
             FROM graph_nodes t
             CROSS JOIN graph_edges e ON e.target_node_id = t.node_id
             WHERE t.tenant_id = ?1 AND e.tenant_id = ?1 AND t.file_path = ''",
        )
        .bind(tenant_id)
        .fetch_all(&self.pool)
        .await?;

        let source_dangling = sqlx::query(
            "SELECT e.edge_id, e.generation, e.target_node_id, e.edge_type, e.source_file,
                    e.weight, e.metadata_json, s.symbol_name AS peer_name
             FROM graph_nodes s
             CROSS JOIN graph_edges e ON e.source_node_id = s.node_id
             WHERE s.tenant_id = ?1 AND e.tenant_id = ?1 AND s.file_path = ''",
        )
        .bind(tenant_id)
        .fetch_all(&self.pool)
        .await?;

        if target_dangling.is_empty() && source_dangling.is_empty() {
            return Ok(0);
        }

        let index = CandidateIndex::load(&self.pool, tenant_id, membership).await?;
        let now = now_utc();
        let mut tx = self.pool.begin().await?;
        let mut repointed =
            repoint_target_stubs(&mut tx, tenant_id, &target_dangling, &index, &now).await?;
        repointed +=
            repoint_source_stubs(&mut tx, tenant_id, &source_dangling, &index, &now).await?;
        tx.commit().await?;

        let dropped_refs = self.drop_unresolved(tenant_id).await?;
        debug!(
            "Resolved {} stub edges for tenant {} ({} target + {} source dangling examined, \
             {} unresolved REFERENCES dropped)",
            repointed,
            tenant_id,
            target_dangling.len(),
            source_dangling.len(),
            dropped_refs
        );
        Ok(repointed)
    }

    /// Drop REFERENCES edges that never resolved (#369) — an unresolved
    /// REFERENCES edge is almost always a local variable or parameter that
    /// merely looked like a top-level symbol at extraction time — then the stub
    /// nodes left without any edge. Returns the REFERENCES edges dropped.
    async fn drop_unresolved(&self, tenant_id: &str) -> GraphDbResult<u64> {
        let dropped_refs = sqlx::query(
            "DELETE FROM graph_edges
             WHERE tenant_id = ?1 AND edge_type = 'REFERENCES'
               AND target_node_id IN (
                   SELECT node_id FROM graph_nodes
                   WHERE tenant_id = ?1 AND file_path = ''
               )",
        )
        .bind(tenant_id)
        .execute(&self.pool)
        .await?
        .rows_affected();

        sqlx::query(
            "DELETE FROM graph_nodes
             WHERE tenant_id = ?1 AND file_path = ''
               AND node_id NOT IN (
                   SELECT source_node_id FROM graph_edges WHERE tenant_id = ?1
                   UNION
                   SELECT target_node_id FROM graph_edges WHERE tenant_id = ?1
               )",
        )
        .bind(tenant_id)
        .execute(&self.pool)
        .await?;
        Ok(dropped_refs)
    }
}

/// Pass 1 — target-stub edges: repoint the TARGET to the real node(s), in the
/// edge's own generation (it stays owned by the same file version), each
/// stamped with its confidence and resolution provenance.
async fn repoint_target_stubs(
    tx: &mut Transaction<'_, Sqlite>,
    tenant_id: &str,
    dangling: &[SqliteRow],
    index: &CandidateIndex<'_>,
    now: &str,
) -> GraphDbResult<u64> {
    let mut repointed = 0;
    for d in dangling {
        let peer_name: String = d.get("peer_name");
        let source_file: String = d.get("source_file");
        let source_node_id: String = d.get("source_node_id");
        let generation: String = d.get("generation");
        let metadata = d.get::<Option<String>, _>("metadata_json");
        let receiver = receiver_types(metadata.as_deref());
        let lsp = lsp_sites(metadata.as_deref());
        let picked = index.pick_all(&StubRef {
            name: &peer_name,
            own_file: &source_file,
            generation: &generation,
            caller_class: index.container_of(&source_node_id),
            caller_lang: index.language_of(&source_node_id),
            container_only: false,
            receiver: receiver.as_deref(),
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
    dangling: &[SqliteRow],
    index: &CandidateIndex<'_>,
    now: &str,
) -> GraphDbResult<u64> {
    let mut repointed = 0;
    for d in dangling {
        let peer_name: String = d.get("peer_name");
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
