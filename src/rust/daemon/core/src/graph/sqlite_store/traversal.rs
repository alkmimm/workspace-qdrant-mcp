//! Scoped reads: bounded breadth-first traversal, symbol lookup and stats.
//!
//! Every edge row read here is filtered through the caller's [`GraphScope`]
//! (the generations its branch holds) BEFORE it can enter a frontier, and a
//! node is reported only through a row the scope admits. A symbol another
//! branch defines is therefore neither reached nor listed, and an edge into a
//! symbol this branch's version of the target file no longer defines leads
//! nowhere.

use std::collections::{HashMap, HashSet};

use sqlx::Row;
use tracing::warn;

use super::SqliteGraphStore;
use crate::graph::{EdgeType, GraphDbResult, GraphScope, GraphStats, TraversalNode};

/// Reached-node cap shared by both traversal directions.
pub(super) const NODE_BUDGET: usize = 10_000;

/// The admitted metadata of one node.
pub(super) struct NodeMeta {
    pub symbol_name: String,
    pub symbol_type: String,
    pub file_path: String,
    pub parent_symbol: Option<String>,
}

/// An edge row of one BFS hop, before scoping.
pub(super) struct HopEdge {
    pub source: String,
    pub target: String,
    pub edge_type: String,
    pub weight: f64,
}

impl SqliteGraphStore {
    /// Edges of one hop whose `column` (source or target) is in `frontier`,
    /// keeping only rows `scope` admits.
    pub(super) async fn scoped_hop_edges(
        &self,
        tenant_id: &str,
        column: &str,
        frontier: &[&str],
        type_filter: &str,
        scope: &GraphScope,
    ) -> GraphDbResult<Vec<HopEdge>> {
        let placeholders: Vec<String> =
            (0..frontier.len()).map(|i| format!("?{}", i + 2)).collect();
        let query = format!(
            "SELECT source_node_id, target_node_id, edge_type, \
             COALESCE(weight, 1.0) AS w, generation \
             FROM graph_edges \
             WHERE tenant_id = ?1 AND {column} IN ({}){type_filter}",
            placeholders.join(", ")
        );
        let mut qb = sqlx::query(&query).bind(tenant_id);
        for nid in frontier {
            qb = qb.bind(*nid);
        }
        let rows = qb.fetch_all(&self.pool).await?;
        Ok(rows
            .iter()
            .filter(|r| scope.admits(r.get::<String, _>("generation").as_str()))
            .map(|r| HopEdge {
                source: r.get("source_node_id"),
                target: r.get("target_node_id"),
                edge_type: r.get("edge_type"),
                weight: r.get("w"),
            })
            .collect())
    }

    /// The admitted metadata row of each of `ids`. When a node has both the
    /// row its own file version wrote and a generation-less reference row,
    /// the file version's row wins (it carries the real definition).
    pub(super) async fn admitted_node_meta(
        &self,
        tenant_id: &str,
        ids: &[String],
        scope: &GraphScope,
    ) -> GraphDbResult<HashMap<String, NodeMeta>> {
        let mut meta: HashMap<String, (bool, NodeMeta)> = HashMap::with_capacity(ids.len());
        if ids.is_empty() {
            return Ok(HashMap::new());
        }
        let ph: Vec<String> = (0..ids.len()).map(|i| format!("?{}", i + 2)).collect();
        let query = format!(
            "SELECT node_id, generation, symbol_name, symbol_type, file_path, parent_symbol \
             FROM graph_nodes WHERE tenant_id = ?1 AND node_id IN ({})",
            ph.join(", ")
        );
        let mut qb = sqlx::query(&query).bind(tenant_id);
        for id in ids {
            qb = qb.bind(id);
        }
        for r in qb.fetch_all(&self.pool).await? {
            let generation: String = r.get("generation");
            if !scope.admits(&generation) {
                continue;
            }
            let owned = !generation.is_empty();
            let id: String = r.get("node_id");
            if meta
                .get(&id)
                .is_some_and(|(have_owned, _)| *have_owned || !owned)
            {
                continue;
            }
            meta.insert(
                id,
                (
                    owned,
                    NodeMeta {
                        symbol_name: r.get("symbol_name"),
                        symbol_type: r.get("symbol_type"),
                        file_path: r.get("file_path"),
                        parent_symbol: r.get("parent_symbol"),
                    },
                ),
            );
        }
        Ok(meta.into_iter().map(|(id, (_, m))| (id, m)).collect())
    }

    pub(super) async fn traverse_forward(
        &self,
        tenant_id: &str,
        node_id: &str,
        max_hops: u32,
        edge_types: Option<&[EdgeType]>,
        scope: &GraphScope,
    ) -> GraphDbResult<Vec<TraversalNode>> {
        if max_hops == 0 {
            return Ok(Vec::new());
        }

        // Bounded breadth-first traversal: each node is visited once at its
        // minimum depth, ONE index-seeking query per hop over the whole frontier
        // (`tenant_id, source_node_id` → idx_edges_tenant_source), and the
        // reached-node count is hard-capped. (A recursive UNION ALL CTE that
        // re-expanded every path measured ~60s for one hop on a hub-heavy graph.)
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
            let ids: Vec<&str> = frontier.iter().map(|(n, _, _)| n.as_str()).collect();
            let edges = self
                .scoped_hop_edges(tenant_id, "source_node_id", &ids, &type_filter, scope)
                .await?;

            let parent: HashMap<&str, (&str, f64)> = frontier
                .iter()
                .map(|(n, p, c)| (n.as_str(), (p.as_str(), *c)))
                .collect();
            // Aggregate this hop's arrivals per target, keeping the
            // HIGHEST-confidence edge: row order is arbitrary, and first-wins
            // would understate `confidence` (the documented best-path product)
            // and wrongly drop nodes under a `min_confidence` filter. BFS
            // min-depth still wins ACROSS hops.
            let mut best: HashMap<String, (String, String, f64)> = HashMap::new();
            for e in edges {
                if visited.contains(&e.target) {
                    continue; // already reached at a shallower depth
                }
                let (ppath, pconf) = parent
                    .get(e.source.as_str())
                    .copied()
                    .unwrap_or((node_id, 1.0));
                let confidence = pconf * e.weight;
                let entry = best.entry(e.target).or_insert((
                    e.edge_type.clone(),
                    ppath.to_string(),
                    f64::MIN,
                ));
                if confidence > entry.2 {
                    *entry = (e.edge_type, ppath.to_string(), confidence);
                }
            }
            // Deterministic emission (#367): highest confidence first, node id
            // as the total tiebreak — a downstream `take(top_k)` decides the
            // ANSWER from this order.
            let mut ranked: Vec<(String, (String, String, f64))> = best.into_iter().collect();
            ranked.sort_by(|(a_id, a_val), (b_id, b_val)| {
                b_val
                    .2
                    .partial_cmp(&a_val.2)
                    .unwrap_or(std::cmp::Ordering::Equal)
                    .then_with(|| a_id.cmp(b_id))
            });
            let mut next: Vec<(String, String, f64)> = Vec::new();
            for (tgt, (edge_type, ppath, confidence)) in ranked {
                if !visited.insert(tgt.clone()) {
                    continue;
                }
                let path = format!("{ppath} -> {tgt}");
                hits.push((tgt.clone(), edge_type, depth, path.clone(), confidence));
                next.push((tgt, path, confidence));
                if visited.len() >= NODE_BUDGET {
                    truncated = true;
                    break;
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

        // Report only nodes with an admitted row (an edge target with none is
        // an unresolved reference or a symbol this branch does not define).
        let uniq_ids = unique_ids(hits.iter().map(|h| h.0.as_str()));
        let meta = self.admitted_node_meta(tenant_id, &uniq_ids, scope).await?;
        let mut results: Vec<TraversalNode> = hits
            .into_iter()
            .filter_map(|(id, edge_type, depth, path, confidence)| {
                meta.get(&id).map(|m| TraversalNode {
                    node_id: id.clone(),
                    symbol_name: m.symbol_name.clone(),
                    symbol_type: m.symbol_type.clone(),
                    file_path: m.file_path.clone(),
                    edge_type,
                    depth,
                    path,
                    confidence,
                    parent_symbol: m.parent_symbol.clone(),
                })
            })
            .collect();
        results.sort_by(|a, b| {
            a.depth
                .cmp(&b.depth)
                .then_with(|| a.symbol_name.cmp(&b.symbol_name))
        });
        Ok(results)
    }

    pub(super) async fn traverse_forward_by_symbol(
        &self,
        tenant_id: &str,
        symbol_name: &str,
        file_path: Option<&str>,
        max_hops: u32,
        edge_types: Option<&[EdgeType]>,
        scope: &GraphScope,
    ) -> GraphDbResult<Vec<TraversalNode>> {
        // Prefer the file_path-narrowed match; if that finds nothing (the
        // file_path form can differ from what the extractor stored — the same
        // mismatch that defeats client-side node_id computation), fall back to
        // a name-only match, exactly how impact_analysis stays robust.
        let mut targets = self
            .find_target_nodes(tenant_id, symbol_name, file_path, scope)
            .await?;
        if targets.is_empty() && file_path.is_some() {
            targets = self
                .find_target_nodes(tenant_id, symbol_name, None, scope)
                .await?;
        }
        // Dedup on (node_id, edge_type, path) — the granularity traverse_forward
        // emits — so a node reached from two sources is not double-listed.
        let mut seen: HashSet<(String, String, String)> = HashSet::new();
        let mut out: Vec<TraversalNode> = Vec::new();
        for nid in &targets {
            for n in self
                .traverse_forward(tenant_id, nid, max_hops, edge_types, scope)
                .await?
            {
                if seen.insert((n.node_id.clone(), n.edge_type.clone(), n.path.clone())) {
                    out.push(n);
                }
            }
        }
        out.sort_by(|a, b| {
            a.depth
                .cmp(&b.depth)
                .then_with(|| a.symbol_name.cmp(&b.symbol_name))
        });
        Ok(out)
    }

    /// Node ids named `symbol_name` (optionally in `file_path`) that `scope`
    /// admits, sorted so every traversal seeded from them is reproducible.
    pub(super) async fn find_target_nodes(
        &self,
        tenant_id: &str,
        symbol_name: &str,
        file_path: Option<&str>,
        scope: &GraphScope,
    ) -> GraphDbResult<Vec<String>> {
        let rows = match file_path {
            Some(fp) => {
                sqlx::query(
                    "SELECT node_id, generation FROM graph_nodes
                     WHERE tenant_id = ?1 AND symbol_name = ?2 AND file_path = ?3",
                )
                .bind(tenant_id)
                .bind(symbol_name)
                .bind(fp)
                .fetch_all(&self.pool)
                .await?
            }
            None => {
                sqlx::query(
                    "SELECT node_id, generation FROM graph_nodes
                     WHERE tenant_id = ?1 AND symbol_name = ?2",
                )
                .bind(tenant_id)
                .bind(symbol_name)
                .fetch_all(&self.pool)
                .await?
            }
        };
        let mut ids: Vec<String> = rows
            .iter()
            .filter(|r| scope.admits(r.get::<String, _>("generation").as_str()))
            .map(|r| r.get("node_id"))
            .collect();
        ids.sort();
        ids.dedup();
        Ok(ids)
    }

    pub(super) async fn scoped_stats(
        &self,
        tenant_id: Option<&str>,
        scope: &GraphScope,
    ) -> GraphDbResult<GraphStats> {
        // Grouped by generation too, so a branch's counts are a sum over the
        // versions it holds. Unscoped calls skip that grouping.
        let by_generation = if scope.is_scoped() {
            ", generation"
        } else {
            ""
        };
        let tenant_filter = if tenant_id.is_some() {
            " WHERE tenant_id = ?1"
        } else {
            ""
        };
        let mut stats = GraphStats::default();
        for (table, column) in [("graph_nodes", "symbol_type"), ("graph_edges", "edge_type")] {
            let generation_col = if scope.is_scoped() {
                "generation"
            } else {
                "'' AS generation"
            };
            let sql = format!(
                "SELECT {column} AS kind, {generation_col}, COUNT(*) AS cnt FROM {table}\
                 {tenant_filter} GROUP BY {column}{by_generation}"
            );
            let mut q = sqlx::query(&sql);
            if let Some(tid) = tenant_id {
                q = q.bind(tid);
            }
            for row in q.fetch_all(&self.pool).await? {
                let generation: String = row.get("generation");
                // A branch's counts are what its file versions define: the
                // generation-less stub rows are unresolved names shared by the
                // whole tenant, not symbols of this branch.
                if !scope.admits(&generation) || (scope.is_scoped() && generation.is_empty()) {
                    continue;
                }
                let kind: String = row.get("kind");
                let cnt = row.get::<i64, _>("cnt").max(0) as u64;
                let (total, by_kind) = if table == "graph_nodes" {
                    (&mut stats.total_nodes, &mut stats.nodes_by_type)
                } else {
                    (&mut stats.total_edges, &mut stats.edges_by_type)
                };
                *total += cnt;
                *by_kind.entry(kind).or_default() += cnt;
            }
        }
        Ok(stats)
    }
}

/// `ids` in first-seen order without repeats.
pub(super) fn unique_ids<'a>(ids: impl Iterator<Item = &'a str>) -> Vec<String> {
    let mut seen: HashSet<&str> = HashSet::new();
    ids.filter(|id| seen.insert(id))
        .map(str::to_string)
        .collect()
}
