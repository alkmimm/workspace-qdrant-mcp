//! SQLite-backed graph store with bounded breadth-first traversal.

use std::collections::{HashMap, HashSet};

use async_trait::async_trait;
use sqlx::{Row, SqlitePool};
use tracing::{debug, warn};
use wqm_common::timestamps::now_utc;

use super::{
    compute_edge_id, EdgeType, GraphDbResult, GraphEdge, GraphNode, GraphStats, GraphStore,
    ImpactNode, ImpactReport, TraversalNode,
};

/// SQLite-backed implementation of `GraphStore`.
///
/// Uses a dedicated `graph.db` with WAL mode. Multi-hop traversal is a bounded
/// breadth-first walk (one index-seeking query per hop, visited-set dedup, node
/// budget) — no graph database engine required.
#[derive(Clone)]
pub struct SqliteGraphStore {
    pool: SqlitePool,
}

impl SqliteGraphStore {
    /// Create a new store from an existing connection pool.
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }

    /// Whether a `symbol_type` string names a *container* kind — one that can
    /// hold members via a CONTAINS edge (class, struct, interface, trait, impl,
    /// module, enum). Used when resolving a CONTAINS parent stub so a same-named
    /// constructor/method never wins over the enclosing type.
    fn is_container_node_type(symbol_type: &str) -> bool {
        matches!(
            symbol_type,
            "class" | "struct" | "interface" | "trait" | "impl" | "module" | "enum"
        )
    }

    /// Get a reference to the pool (for advanced queries in tests).
    pub fn pool(&self) -> &SqlitePool {
        &self.pool
    }
}

#[async_trait]
impl GraphStore for SqliteGraphStore {
    async fn upsert_node(&self, node: &GraphNode) -> GraphDbResult<()> {
        let now = now_utc();
        sqlx::query(
            "INSERT INTO graph_nodes (node_id, tenant_id, symbol_name, symbol_type,
                file_path, start_line, end_line, signature, language,
                created_at, updated_at)
            VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?10)
            ON CONFLICT(node_id) DO UPDATE SET
                symbol_name = excluded.symbol_name,
                symbol_type = excluded.symbol_type,
                file_path = CASE WHEN excluded.file_path = '' THEN graph_nodes.file_path
                                 ELSE excluded.file_path END,
                start_line = COALESCE(excluded.start_line, graph_nodes.start_line),
                end_line = COALESCE(excluded.end_line, graph_nodes.end_line),
                signature = COALESCE(excluded.signature, graph_nodes.signature),
                language = COALESCE(excluded.language, graph_nodes.language),
                updated_at = ?10",
        )
        .bind(&node.node_id)
        .bind(&node.tenant_id)
        .bind(&node.symbol_name)
        .bind(node.symbol_type.as_str())
        .bind(&node.file_path)
        .bind(node.start_line.map(|v| v as i64))
        .bind(node.end_line.map(|v| v as i64))
        .bind(&node.signature)
        .bind(&node.language)
        .bind(&now)
        .execute(&self.pool)
        .await?;

        Ok(())
    }

    async fn upsert_nodes(&self, nodes: &[GraphNode]) -> GraphDbResult<()> {
        if nodes.is_empty() {
            return Ok(());
        }
        let now = now_utc();
        let mut tx = self.pool.begin().await?;

        for node in nodes {
            sqlx::query(
                "INSERT INTO graph_nodes (node_id, tenant_id, symbol_name, symbol_type,
                    file_path, start_line, end_line, signature, language,
                    created_at, updated_at)
                VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?10)
                ON CONFLICT(node_id) DO UPDATE SET
                    symbol_name = excluded.symbol_name,
                    symbol_type = excluded.symbol_type,
                    file_path = CASE WHEN excluded.file_path = '' THEN graph_nodes.file_path
                                     ELSE excluded.file_path END,
                    start_line = COALESCE(excluded.start_line, graph_nodes.start_line),
                    end_line = COALESCE(excluded.end_line, graph_nodes.end_line),
                    signature = COALESCE(excluded.signature, graph_nodes.signature),
                    language = COALESCE(excluded.language, graph_nodes.language),
                    updated_at = ?10",
            )
            .bind(&node.node_id)
            .bind(&node.tenant_id)
            .bind(&node.symbol_name)
            .bind(node.symbol_type.as_str())
            .bind(&node.file_path)
            .bind(node.start_line.map(|v| v as i64))
            .bind(node.end_line.map(|v| v as i64))
            .bind(&node.signature)
            .bind(&node.language)
            .bind(&now)
            .execute(&mut *tx)
            .await?;
        }

        tx.commit().await?;
        debug!("Upserted {} graph nodes", nodes.len());
        Ok(())
    }

    async fn insert_edge(&self, edge: &GraphEdge) -> GraphDbResult<()> {
        let now = now_utc();
        sqlx::query(
            "INSERT OR IGNORE INTO graph_edges
                (edge_id, tenant_id, source_node_id, target_node_id, edge_type,
                 source_file, weight, metadata_json, created_at)
            VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
        )
        .bind(&edge.edge_id)
        .bind(&edge.tenant_id)
        .bind(&edge.source_node_id)
        .bind(&edge.target_node_id)
        .bind(edge.edge_type.as_str())
        .bind(&edge.source_file)
        .bind(edge.weight)
        .bind(&edge.metadata_json)
        .bind(&now)
        .execute(&self.pool)
        .await?;

        Ok(())
    }

    async fn insert_edges(&self, edges: &[GraphEdge]) -> GraphDbResult<()> {
        if edges.is_empty() {
            return Ok(());
        }
        let now = now_utc();
        let mut tx = self.pool.begin().await?;

        for edge in edges {
            sqlx::query(
                "INSERT OR IGNORE INTO graph_edges
                    (edge_id, tenant_id, source_node_id, target_node_id, edge_type,
                     source_file, weight, metadata_json, created_at)
                VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
            )
            .bind(&edge.edge_id)
            .bind(&edge.tenant_id)
            .bind(&edge.source_node_id)
            .bind(&edge.target_node_id)
            .bind(edge.edge_type.as_str())
            .bind(&edge.source_file)
            .bind(edge.weight)
            .bind(&edge.metadata_json)
            .bind(&now)
            .execute(&mut *tx)
            .await?;
        }

        tx.commit().await?;
        debug!("Inserted {} graph edges", edges.len());
        Ok(())
    }

    async fn delete_edges_by_file(&self, tenant_id: &str, file_path: &str) -> GraphDbResult<u64> {
        let result =
            sqlx::query("DELETE FROM graph_edges WHERE tenant_id = ?1 AND source_file = ?2")
                .bind(tenant_id)
                .bind(file_path)
                .execute(&self.pool)
                .await?;

        let count = result.rows_affected();
        debug!(
            "Deleted {} graph edges for file {} in tenant {}",
            count, file_path, tenant_id
        );
        Ok(count)
    }

    async fn delete_tenant(&self, tenant_id: &str) -> GraphDbResult<u64> {
        let mut tx = self.pool.begin().await?;

        let edge_result = sqlx::query("DELETE FROM graph_edges WHERE tenant_id = ?1")
            .bind(tenant_id)
            .execute(&mut *tx)
            .await?;

        let node_result = sqlx::query("DELETE FROM graph_nodes WHERE tenant_id = ?1")
            .bind(tenant_id)
            .execute(&mut *tx)
            .await?;

        tx.commit().await?;

        let total = edge_result.rows_affected() + node_result.rows_affected();
        debug!(
            "Deleted {} items (edges + nodes) for tenant {}",
            total, tenant_id
        );
        Ok(total)
    }

    async fn query_related(
        &self,
        tenant_id: &str,
        node_id: &str,
        max_hops: u32,
        edge_types: Option<&[EdgeType]>,
    ) -> GraphDbResult<Vec<TraversalNode>> {
        if max_hops == 0 {
            return Ok(Vec::new());
        }

        // Bounded breadth-first traversal. Replaces a recursive `UNION ALL` CTE
        // that re-expanded every node via every path — on a re-convergent /
        // hub-heavy call graph that is exponential (a single live 1-hop relation
        // on the 686k-edge tenant measured ~60s before this change). BFS visits
        // each node once at its minimum depth, issues ONE index-seeking query per
        // hop over the whole frontier (`source_node_id IN (...)` → idx_edges_source),
        // and hard-caps the reached-node count.
        const NODE_BUDGET: usize = 10_000;

        // Edge-type filter appended to each per-hop query (single table, no alias).
        let type_filter = match edge_types {
            Some(types) if !types.is_empty() => {
                let placeholders: Vec<String> =
                    types.iter().map(|t| format!("'{}'", t.as_str())).collect();
                format!(" AND edge_type IN ({})", placeholders.join(", "))
            }
            _ => String::new(),
        };

        let mut visited: HashSet<String> = HashSet::new();
        visited.insert(node_id.to_string());
        // (node_id, path, confidence) for the current frontier.
        let mut frontier: Vec<(String, String, f64)> =
            vec![(node_id.to_string(), node_id.to_string(), 1.0)];
        // Reached nodes in BFS order: (node_id, edge_type, depth, path, confidence).
        let mut hits: Vec<(String, String, u32, String, f64)> = Vec::new();
        let mut truncated = false;

        let mut depth = 0u32;
        while depth < max_hops && !frontier.is_empty() {
            depth += 1;

            let placeholders: Vec<String> =
                (0..frontier.len()).map(|i| format!("?{}", i + 2)).collect();
            let query = format!(
                "SELECT source_node_id, target_node_id, edge_type, \
                 COALESCE(weight, 1.0) AS w \
                 FROM graph_edges \
                 WHERE tenant_id = ?1 AND source_node_id IN ({}){}",
                placeholders.join(", "),
                type_filter
            );
            let mut qb = sqlx::query(&query).bind(tenant_id);
            for (nid, _, _) in &frontier {
                qb = qb.bind(nid);
            }
            let rows = qb.fetch_all(&self.pool).await?;

            let mut next: Vec<(String, String, f64)> = Vec::new();
            {
                let parent: HashMap<&str, (&str, f64)> = frontier
                    .iter()
                    .map(|(n, p, c)| (n.as_str(), (p.as_str(), *c)))
                    .collect();
                for row in &rows {
                    let tgt: String = row.get("target_node_id");
                    if !visited.insert(tgt.clone()) {
                        continue; // already reached at an equal-or-shallower depth
                    }
                    let src: String = row.get("source_node_id");
                    let edge_type: String = row.get("edge_type");
                    let weight: f64 = row.get("w");
                    let (ppath, pconf) =
                        parent.get(src.as_str()).copied().unwrap_or((node_id, 1.0));
                    let path = format!("{ppath} -> {tgt}");
                    let confidence = pconf * weight;
                    hits.push((tgt.clone(), edge_type, depth, path.clone(), confidence));
                    next.push((tgt, path, confidence));
                    if visited.len() >= NODE_BUDGET {
                        truncated = true;
                        break;
                    }
                }
            }
            frontier = next;
            if truncated {
                break;
            }
        }

        if truncated {
            warn!(
                "graph query_related: node budget {} reached from source {} — results truncated",
                NODE_BUDGET, node_id
            );
        }
        if hits.is_empty() {
            return Ok(Vec::new());
        }

        // Resolve symbol metadata for reached nodes that ARE graph nodes (the old
        // INNER JOIN dropped edge targets with no node row, e.g. unresolved stubs).
        let uniq_ids: Vec<String> = {
            let mut seen: HashSet<&str> = HashSet::new();
            hits.iter()
                .filter(|h| seen.insert(h.0.as_str()))
                .map(|h| h.0.clone())
                .collect()
        };
        let node_ph: Vec<String> = (0..uniq_ids.len()).map(|i| format!("?{}", i + 1)).collect();
        let node_query = format!(
            "SELECT node_id, symbol_name, symbol_type, file_path \
             FROM graph_nodes WHERE node_id IN ({})",
            node_ph.join(", ")
        );
        let mut nqb = sqlx::query(&node_query);
        for id in &uniq_ids {
            nqb = nqb.bind(id);
        }
        let node_rows = nqb.fetch_all(&self.pool).await?;
        let mut meta: HashMap<String, (String, String, String)> =
            HashMap::with_capacity(node_rows.len());
        for r in &node_rows {
            let id: String = r.get("node_id");
            meta.insert(
                id,
                (r.get("symbol_name"), r.get("symbol_type"), r.get("file_path")),
            );
        }

        let mut results: Vec<TraversalNode> = hits
            .into_iter()
            .filter_map(|(id, edge_type, depth, path, confidence)| {
                meta.get(&id)
                    .map(|(symbol_name, symbol_type, file_path)| TraversalNode {
                        node_id: id.clone(),
                        symbol_name: symbol_name.clone(),
                        symbol_type: symbol_type.clone(),
                        file_path: file_path.clone(),
                        edge_type,
                        depth,
                        path,
                        confidence,
                    })
            })
            .collect();
        results
            .sort_by(|a, b| a.depth.cmp(&b.depth).then_with(|| a.symbol_name.cmp(&b.symbol_name)));
        Ok(results)
    }

    async fn query_related_by_symbol(
        &self,
        tenant_id: &str,
        symbol_name: &str,
        file_path: Option<&str>,
        max_hops: u32,
        edge_types: Option<&[EdgeType]>,
    ) -> GraphDbResult<Vec<TraversalNode>> {
        // Resolve the source node(s) by name. Prefer the file_path-narrowed
        // match; if that finds nothing (the file_path form can differ from what
        // the extractor stored — the same mismatch that defeats client-side
        // node_id computation), fall back to a name-only match, exactly how
        // impact_analysis stays robust.
        let mut targets = self
            .find_target_nodes(tenant_id, symbol_name, file_path)
            .await?;
        if targets.is_empty() && file_path.is_some() {
            targets = self.find_target_nodes(tenant_id, symbol_name, None).await?;
        }
        if targets.is_empty() {
            return Ok(Vec::new());
        }

        // Traverse forward from every matched node, merging the results. Dedup on
        // (node_id, edge_type, path) — the same granularity query_related emits —
        // so a node reached from two source nodes is not double-listed; sort by
        // depth then name to match query_related's ordering.
        let mut seen: std::collections::HashSet<(String, String, String)> =
            std::collections::HashSet::new();
        let mut out: Vec<TraversalNode> = Vec::new();
        for nid in &targets {
            for n in self
                .query_related(tenant_id, nid, max_hops, edge_types)
                .await?
            {
                if seen.insert((n.node_id.clone(), n.edge_type.clone(), n.path.clone())) {
                    out.push(n);
                }
            }
        }
        out.sort_by(|a, b| a.depth.cmp(&b.depth).then_with(|| a.symbol_name.cmp(&b.symbol_name)));
        Ok(out)
    }

    async fn impact_analysis(
        &self,
        tenant_id: &str,
        symbol_name: &str,
        file_path: Option<&str>,
    ) -> GraphDbResult<ImpactReport> {
        let target_nodes = self
            .find_target_nodes(tenant_id, symbol_name, file_path)
            .await?;

        if target_nodes.is_empty() {
            return Ok(ImpactReport {
                symbol_name: symbol_name.to_string(),
                impacted_nodes: vec![],
                total_impacted: 0,
            });
        }

        let mut all_impacted = Vec::new();
        for target_id in &target_nodes {
            let impacted = self.reverse_traverse(tenant_id, target_id).await?;
            all_impacted.extend(impacted);
        }

        all_impacted.sort_by_key(|n| n.distance);
        let mut seen = std::collections::HashSet::new();
        all_impacted.retain(|n| seen.insert(n.node_id.clone()));

        let total = all_impacted.len() as u32;
        Ok(ImpactReport {
            symbol_name: symbol_name.to_string(),
            impacted_nodes: all_impacted,
            total_impacted: total,
        })
    }

    async fn stats(&self, tenant_id: Option<&str>) -> GraphDbResult<GraphStats> {
        let (node_rows, edge_rows) = match tenant_id {
            Some(tid) => {
                let nodes = sqlx::query(
                    "SELECT symbol_type, COUNT(*) as cnt FROM graph_nodes
                     WHERE tenant_id = ?1 GROUP BY symbol_type",
                )
                .bind(tid)
                .fetch_all(&self.pool)
                .await?;
                let edges = sqlx::query(
                    "SELECT edge_type, COUNT(*) as cnt FROM graph_edges
                     WHERE tenant_id = ?1 GROUP BY edge_type",
                )
                .bind(tid)
                .fetch_all(&self.pool)
                .await?;
                (nodes, edges)
            }
            None => {
                let nodes = sqlx::query(
                    "SELECT symbol_type, COUNT(*) as cnt FROM graph_nodes
                     GROUP BY symbol_type",
                )
                .fetch_all(&self.pool)
                .await?;
                let edges = sqlx::query(
                    "SELECT edge_type, COUNT(*) as cnt FROM graph_edges
                     GROUP BY edge_type",
                )
                .fetch_all(&self.pool)
                .await?;
                (nodes, edges)
            }
        };

        let mut stats = GraphStats::default();
        for row in &node_rows {
            let stype: String = row.get("symbol_type");
            let cnt: i64 = row.get("cnt");
            stats.total_nodes += cnt as u64;
            stats.nodes_by_type.insert(stype, cnt as u64);
        }
        for row in &edge_rows {
            let etype: String = row.get("edge_type");
            let cnt: i64 = row.get("cnt");
            stats.total_edges += cnt as u64;
            stats.edges_by_type.insert(etype, cnt as u64);
        }

        Ok(stats)
    }

    async fn prune_orphans(&self, tenant_id: &str) -> GraphDbResult<u64> {
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

    async fn resolve_stub_edges(&self, tenant_id: &str) -> GraphDbResult<u64> {
        use std::collections::HashMap;

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
        let target_dangling = sqlx::query(
            "SELECT e.edge_id, e.source_node_id, e.edge_type, e.source_file,
                    e.weight, e.metadata_json, t.symbol_name AS peer_name
             FROM graph_edges e
             JOIN graph_nodes t ON e.target_node_id = t.node_id
             WHERE e.tenant_id = ?1 AND (t.file_path IS NULL OR t.file_path = '')",
        )
        .bind(tenant_id)
        .fetch_all(&self.pool)
        .await?;

        let source_dangling = sqlx::query(
            "SELECT e.edge_id, e.target_node_id, e.edge_type, e.source_file,
                    e.weight, e.metadata_json, s.symbol_name AS peer_name
             FROM graph_edges e
             JOIN graph_nodes s ON e.source_node_id = s.node_id
             WHERE e.tenant_id = ?1 AND (s.file_path IS NULL OR s.file_path = '')",
        )
        .bind(tenant_id)
        .fetch_all(&self.pool)
        .await?;

        if target_dangling.is_empty() && source_dangling.is_empty() {
            return Ok(0);
        }

        // Real candidate nodes (resolved file_path, not file-typed), indexed by
        // symbol_name -> [(node_id, file_path, symbol_type)].
        let real_rows = sqlx::query(
            "SELECT node_id, symbol_name, file_path, symbol_type, language FROM graph_nodes
             WHERE tenant_id = ?1 AND file_path <> '' AND symbol_type <> 'file'",
        )
        .bind(tenant_id)
        .fetch_all(&self.pool)
        .await?;

        let mut by_name: HashMap<String, Vec<(String, String, String)>> = HashMap::new();
        // node_id -> language, stamped at extraction by the dynamic language registry
        // (graph/extractor/mod.rs). Used to scope call/type resolution to the
        // caller's own language: a TypeScript `.filter()` must not repoint onto a
        // Rust `filter` method of the same bare name (R3 — cross-language false
        // CALLS were a top source of `relations` noise). Only known, non-empty
        // languages are recorded, so an unclassified node never causes an over-drop.
        let mut node_lang: HashMap<String, String> = HashMap::new();
        for r in &real_rows {
            let name: String = r.get("symbol_name");
            let nid: String = r.get("node_id");
            let fp: String = r.get("file_path");
            let ty: String = r.get("symbol_type");
            let lang: Option<String> = r.get("language");
            if let Some(l) = lang {
                if !l.is_empty() {
                    node_lang.insert(nid.clone(), l);
                }
            }
            by_name.entry(name).or_default().push((nid, fp, ty));
        }

        // R2 scope map: member node_id -> its enclosing class/container node_id, from
        // CONTAINS edges. Lets the resolver prefer a same-named callee defined in the
        // CALLER's own class (cross-file methods of one class) over a tenant-wide
        // collision — the precision tier above keep-all-candidates. (R2)
        let contained_by: HashMap<String, String> = sqlx::query(
            "SELECT source_node_id AS class_id, target_node_id AS member_id
             FROM graph_edges
             WHERE tenant_id = ?1 AND edge_type = 'CONTAINS'",
        )
        .bind(tenant_id)
        .fetch_all(&self.pool)
        .await?
        .iter()
        .map(|r| (r.get::<String, _>("member_id"), r.get::<String, _>("class_id")))
        .collect();

        // Resolve a stub name to real definition node(s), each with a CONFIDENCE
        // weight (see docs/plans/2026-06-24-code-graph-resolution-roadmap.md, R1/R2):
        //   own-file definition       -> [(node, 1.0)]       precise
        //   caller's class (R2 scope) -> [(node, 0.95)]      scoped
        //   unique tenant-wide        -> [(node, 0.7)]       likely
        //   ambiguous (N>1)           -> [(c, 1/N) for each] KEEP ALL (recall>precision)
        //   external / no match       -> []                  leave it a stub
        // Keeping ambiguous edges (instead of dropping them) restores impact/usages
        // recall when same-named callees (build/of/toString/domain methods) collide
        // across files; the scope tier then recovers PRECISION for the common
        // intra-class case. `container_only` restricts the pool to container kinds.
        let pick_all = |name: &str,
                        own_file: &str,
                        caller_class: Option<&str>,
                        caller_lang: Option<&str>,
                        container_only: bool|
         -> Vec<(String, f64)> {
            let Some(candidates) = by_name.get(name) else {
                return Vec::new();
            };
            let pool: Vec<(&str, &str)> = candidates
                .iter()
                .filter(|(_, _, ty)| !container_only || Self::is_container_node_type(ty))
                // Language scope: drop a candidate only when BOTH the caller's and
                // the candidate's languages are known AND differ (a cross-language
                // false positive). Unknown on either side → keep, so a node the
                // registry didn't classify is never over-dropped.
                .filter(|(nid, _, _)| match (caller_lang, node_lang.get(nid.as_str())) {
                    (Some(cl), Some(tl)) => cl == tl.as_str(),
                    _ => true,
                })
                .map(|(nid, fp, _)| (nid.as_str(), fp.as_str()))
                .collect();
            if pool.is_empty() {
                return Vec::new();
            }
            // Prefer a definition in the edge's own file (precise).
            if let Some((nid, _)) = pool.iter().find(|(_, fp)| *fp == own_file) {
                return vec![((*nid).to_string(), 1.0)];
            }
            // R2: prefer a candidate in the CALLER's own enclosing class (e.g. a
            // sibling/inherited method defined in another file) over a tenant-wide
            // collision — the scope-aware precision tier.
            if let Some(cc) = caller_class {
                if let Some((nid, _)) = pool
                    .iter()
                    .find(|(nid, _)| contained_by.get(*nid).map(String::as_str) == Some(cc))
                {
                    return vec![((*nid).to_string(), 0.95)];
                }
            }
            // A unique tenant-wide name.
            if pool.len() == 1 {
                return vec![(pool[0].0.to_string(), 0.7)];
            }
            // Ambiguous: keep EVERY candidate, confidence 1/N. The fan-out is
            // normalized so centrality (which consumes only high-confidence edges,
            // weight >= 0.6) is not inflated, while impact/usages see all candidates.
            let conf = 1.0 / pool.len() as f64;
            pool.iter()
                .map(|(nid, _)| ((*nid).to_string(), conf))
                .collect()
        };
        // Compact resolution provenance stamped onto each repointed edge's metadata.
        let resolution_metadata = |confidence: f64, n: usize| -> String {
            let tier = if confidence >= 0.99 {
                "in_file"
            } else if confidence >= 0.9 {
                "scoped"
            } else if n == 1 {
                "tenant_unique"
            } else {
                "ambiguous"
            };
            format!(
                "{{\"resolution\":\"{}\",\"confidence\":{:.4},\"candidates\":{}}}",
                tier, confidence, n
            )
        };

        let now = now_utc();
        let mut repointed: u64 = 0;
        let mut tx = self.pool.begin().await?;

        // Pass 1 — target-stub edges: repoint the TARGET to the real node(s).
        // Ambiguous names fan out to every candidate (confidence 1/N) instead of
        // being dropped; external/unresolved names yield no candidate and stay stubs.
        for d in &target_dangling {
            let peer_name: String = d.get("peer_name");
            let source_file: String = d.get("source_file");
            let source_node_id: String = d.get("source_node_id");
            // R2: the caller's enclosing class, used to prefer a same-class callee.
            let caller_class = contained_by.get(&source_node_id).map(String::as_str);
            // Caller's language, used to scope out cross-language collisions.
            let caller_lang = node_lang.get(&source_node_id).map(String::as_str);
            let candidates = pick_all(&peer_name, &source_file, caller_class, caller_lang, false);
            if candidates.is_empty() {
                continue; // external/stdlib or unresolved — leave it a stub.
            }
            let edge_type_str: String = d.get("edge_type");
            let Some(edge_type) = EdgeType::from_str(&edge_type_str) else {
                continue;
            };
            let old_edge_id: String = d.get("edge_id");
            let mut emitted = false;
            for (new_target, confidence) in &candidates {
                // Skip self-loops (e.g. direct recursion) — no signal, skews centrality.
                if &source_node_id == new_target {
                    continue;
                }
                let new_edge_id = compute_edge_id(&source_node_id, new_target, edge_type);
                let meta = resolution_metadata(*confidence, candidates.len());
                sqlx::query(
                    "INSERT OR IGNORE INTO graph_edges
                        (edge_id, tenant_id, source_node_id, target_node_id, edge_type,
                         source_file, weight, metadata_json, created_at)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
                )
                .bind(&new_edge_id)
                .bind(tenant_id)
                .bind(&source_node_id)
                .bind(new_target)
                .bind(edge_type.as_str())
                .bind(&source_file)
                .bind(*confidence)
                .bind(&meta)
                .bind(&now)
                .execute(&mut *tx)
                .await?;
                emitted = true;
            }
            if emitted {
                sqlx::query("DELETE FROM graph_edges WHERE edge_id = ?1 AND tenant_id = ?2")
                    .bind(&old_edge_id)
                    .bind(tenant_id)
                    .execute(&mut *tx)
                    .await?;
                repointed += 1;
            }
        }

        // Pass 2 — source-stub edges (CONTAINS from a file-less container stub):
        // repoint the SOURCE to the real container node of the same name.
        for d in &source_dangling {
            let peer_name: String = d.get("peer_name");
            let source_file: String = d.get("source_file");
            let target_node_id: String = d.get("target_node_id");
            // Reference language = the real member's language; a container and its
            // member share a language, so this scopes out cross-language collisions.
            let ref_lang = node_lang.get(&target_node_id).map(String::as_str);
            // Containment is structural (one owner): keep ONLY a confident match
            // (own-file or tenant-unique), never fan out an ambiguous container name.
            let Some(new_source) = pick_all(&peer_name, &source_file, None, ref_lang, true)
                .into_iter()
                .find(|(_, c)| *c >= 0.7)
                .map(|(nid, _)| nid)
            else {
                continue;
            };
            if target_node_id == new_source {
                continue;
            }
            let edge_type_str: String = d.get("edge_type");
            let Some(edge_type) = EdgeType::from_str(&edge_type_str) else {
                continue;
            };
            let old_edge_id: String = d.get("edge_id");
            let weight: f64 = d.get("weight");
            let metadata_json: Option<String> = d.get("metadata_json");
            let new_edge_id = compute_edge_id(&new_source, &target_node_id, edge_type);

            sqlx::query(
                "INSERT OR IGNORE INTO graph_edges
                    (edge_id, tenant_id, source_node_id, target_node_id, edge_type,
                     source_file, weight, metadata_json, created_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
            )
            .bind(&new_edge_id)
            .bind(tenant_id)
            .bind(&new_source)
            .bind(&target_node_id)
            .bind(edge_type.as_str())
            .bind(&source_file)
            .bind(weight)
            .bind(&metadata_json)
            .bind(&now)
            .execute(&mut *tx)
            .await?;

            sqlx::query("DELETE FROM graph_edges WHERE edge_id = ?1 AND tenant_id = ?2")
                .bind(&old_edge_id)
                .bind(tenant_id)
                .execute(&mut *tx)
                .await?;
            repointed += 1;
        }

        tx.commit().await?;

        // Drop stub nodes that no longer have any edges.
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

        debug!(
            "Resolved {} stub edges for tenant {} ({} target + {} source dangling examined)",
            repointed,
            tenant_id,
            target_dangling.len(),
            source_dangling.len()
        );
        Ok(repointed)
    }
}

impl SqliteGraphStore {
    async fn find_target_nodes(
        &self,
        tenant_id: &str,
        symbol_name: &str,
        file_path: Option<&str>,
    ) -> GraphDbResult<Vec<String>> {
        if let Some(fp) = file_path {
            let rows = sqlx::query(
                "SELECT node_id FROM graph_nodes
                 WHERE tenant_id = ?1 AND symbol_name = ?2 AND file_path = ?3",
            )
            .bind(tenant_id)
            .bind(symbol_name)
            .bind(fp)
            .fetch_all(&self.pool)
            .await?;
            Ok(rows.iter().map(|r| r.get("node_id")).collect())
        } else {
            let rows = sqlx::query(
                "SELECT node_id FROM graph_nodes
                 WHERE tenant_id = ?1 AND symbol_name = ?2",
            )
            .bind(tenant_id)
            .bind(symbol_name)
            .fetch_all(&self.pool)
            .await?;
            Ok(rows.iter().map(|r| r.get("node_id")).collect())
        }
    }

    async fn reverse_traverse(
        &self,
        tenant_id: &str,
        target_id: &str,
    ) -> GraphDbResult<Vec<ImpactNode>> {
        // Bounded breadth-first REVERSE traversal (callers of `target_id`, up to 3
        // hops). Same rationale as `query_related`: the previous recursive
        // `UNION ALL` CTE re-expanded nodes via every path and could blow up on a
        // hub-heavy graph. One index-seeking query per hop (`target_node_id IN (...)`
        // → idx_edges_target), visited-set dedup, node budget.
        const MAX_DISTANCE: u32 = 3;
        const NODE_BUDGET: usize = 10_000;

        let mut visited: HashSet<String> = HashSet::new();
        visited.insert(target_id.to_string());
        // (node_id, confidence) for the current reverse frontier.
        let mut frontier: Vec<(String, f64)> = vec![(target_id.to_string(), 1.0)];
        // Reached callers in BFS order: (node_id, edge_type, distance, confidence).
        let mut hits: Vec<(String, String, u32, f64)> = Vec::new();
        let mut truncated = false;

        let mut distance = 0u32;
        while distance < MAX_DISTANCE && !frontier.is_empty() {
            distance += 1;

            let placeholders: Vec<String> =
                (0..frontier.len()).map(|i| format!("?{}", i + 2)).collect();
            let query = format!(
                "SELECT source_node_id, target_node_id, edge_type, \
                 COALESCE(weight, 1.0) AS w \
                 FROM graph_edges \
                 WHERE tenant_id = ?1 AND target_node_id IN ({})",
                placeholders.join(", ")
            );
            let mut qb = sqlx::query(&query).bind(tenant_id);
            for (nid, _) in &frontier {
                qb = qb.bind(nid);
            }
            let rows = qb.fetch_all(&self.pool).await?;

            let mut next: Vec<(String, f64)> = Vec::new();
            {
                let parent: HashMap<&str, f64> =
                    frontier.iter().map(|(n, c)| (n.as_str(), *c)).collect();
                for row in &rows {
                    let src: String = row.get("source_node_id"); // the caller
                    if !visited.insert(src.clone()) {
                        continue;
                    }
                    let tgt: String = row.get("target_node_id");
                    let edge_type: String = row.get("edge_type");
                    let weight: f64 = row.get("w");
                    let pconf = parent.get(tgt.as_str()).copied().unwrap_or(1.0);
                    let confidence = pconf * weight;
                    hits.push((src.clone(), edge_type, distance, confidence));
                    next.push((src, confidence));
                    if visited.len() >= NODE_BUDGET {
                        truncated = true;
                        break;
                    }
                }
            }
            frontier = next;
            if truncated {
                break;
            }
        }

        if truncated {
            warn!(
                "graph reverse_traverse: node budget {} reached from target {} — impact truncated",
                NODE_BUDGET, target_id
            );
        }
        if hits.is_empty() {
            return Ok(Vec::new());
        }

        let uniq_ids: Vec<String> = {
            let mut seen: HashSet<&str> = HashSet::new();
            hits.iter()
                .filter(|h| seen.insert(h.0.as_str()))
                .map(|h| h.0.clone())
                .collect()
        };
        let node_ph: Vec<String> = (0..uniq_ids.len()).map(|i| format!("?{}", i + 1)).collect();
        let node_query = format!(
            "SELECT node_id, symbol_name, file_path \
             FROM graph_nodes WHERE node_id IN ({})",
            node_ph.join(", ")
        );
        let mut nqb = sqlx::query(&node_query);
        for id in &uniq_ids {
            nqb = nqb.bind(id);
        }
        let node_rows = nqb.fetch_all(&self.pool).await?;
        let mut meta: HashMap<String, (String, String)> = HashMap::with_capacity(node_rows.len());
        for r in &node_rows {
            let id: String = r.get("node_id");
            meta.insert(id, (r.get("symbol_name"), r.get("file_path")));
        }

        Ok(hits
            .into_iter()
            .filter_map(|(id, edge_type, distance, confidence)| {
                meta.get(&id).map(|(symbol_name, file_path)| {
                    let impact_type = match (distance, edge_type.as_str()) {
                        (1, "CALLS") => "direct_caller",
                        (1, "USES_TYPE") => "type_user",
                        (1, _) => "direct_reference",
                        (_, "CALLS") => "indirect_caller",
                        _ => "indirect_reference",
                    };
                    ImpactNode {
                        node_id: id.clone(),
                        symbol_name: symbol_name.clone(),
                        file_path: file_path.clone(),
                        impact_type: impact_type.to_string(),
                        distance,
                        confidence,
                    }
                })
            })
            .collect())
    }
}
