//! Scoped impact analysis: the reverse traversal (who depends on a symbol),
//! through the same [`GraphScope`] filter as the forward traversal.

use std::collections::{HashMap, HashSet};

use tracing::warn;

use super::traversal::{unique_ids, HopEdge, NodeMeta, NODE_BUDGET};
use super::SqliteGraphStore;
use crate::graph::{GraphDbResult, GraphScope, ImpactNode, ImpactReport};

/// When the caller pins a `file_path`, they want the blast radius of ONE
/// definition: drop the R1 ambiguous fan-out (an unresolved call site's 1/N
/// edge to EVERY same-name definition, weight < 0.6) with the same floor
/// cycles and centrality apply. Without a file_path the query is intentionally
/// broad ("everything named X") and keeps every edge.
const AMBIGUOUS_EDGE_CONFIDENCE_FLOOR: f64 = 0.6;

/// One reverse walk from one definition.
#[derive(Default)]
struct ReverseWalk {
    /// Reached callers in BFS order: (node_id, edge_type, distance, confidence).
    hits: Vec<(String, String, u32, f64)>,
    /// Callers an edge below the floor led to, before knowing whether a
    /// stronger edge reaches them too.
    below_floor: HashSet<String>,
    budget_reached: bool,
}

impl SqliteGraphStore {
    pub(super) async fn impact(
        &self,
        tenant_id: &str,
        symbol_name: &str,
        file_path: Option<&str>,
        max_hops: u32,
        scope: &GraphScope,
    ) -> GraphDbResult<ImpactReport> {
        let target_nodes = self
            .find_target_nodes(tenant_id, symbol_name, file_path, scope)
            .await?;
        let min_edge_confidence = if file_path.is_some() {
            AMBIGUOUS_EDGE_CONFIDENCE_FLOOR
        } else {
            0.0
        };

        let mut all_impacted = Vec::new();
        let mut below_floor: HashSet<String> = HashSet::new();
        let mut node_budget_reached = false;
        for target_id in &target_nodes {
            let walk = self
                .reverse_traverse(tenant_id, target_id, max_hops, min_edge_confidence, scope)
                .await?;
            node_budget_reached |= walk.budget_reached;
            below_floor.extend(walk.below_floor);
            let ids = unique_ids(walk.hits.iter().map(|h| h.0.as_str()));
            let meta = self.admitted_node_meta(tenant_id, &ids, scope).await?;
            all_impacted.extend(impact_nodes(walk.hits, &meta));
        }
        all_impacted.sort_by_key(|n| n.distance);
        let mut seen = HashSet::new();
        all_impacted.retain(|n| seen.insert(n.node_id.clone()));

        // A caller skipped on a weak edge but reached through a strong one is
        // impacted, not dropped; and only callers this branch holds count.
        let dropped: Vec<String> = below_floor
            .into_iter()
            .filter(|id| !seen.contains(id) && !target_nodes.contains(id))
            .collect();
        let dropped_below_floor = self
            .admitted_node_meta(tenant_id, &dropped, scope)
            .await?
            .len() as u32;

        let total = all_impacted.len() as u32;
        Ok(ImpactReport {
            symbol_name: symbol_name.to_string(),
            impacted_nodes: all_impacted,
            total_impacted: total,
            max_hops,
            dropped_below_floor,
            node_budget_reached,
        })
    }

    /// Bounded breadth-first REVERSE traversal (callers of `target_id`, up to
    /// `max_hops` hops), same shape as `traverse_forward`. `min_edge_confidence`
    /// prunes low-weight incoming edges before they enter the frontier (0.0 =
    /// keep all, the unanchored default).
    async fn reverse_traverse(
        &self,
        tenant_id: &str,
        target_id: &str,
        max_hops: u32,
        min_edge_confidence: f64,
        scope: &GraphScope,
    ) -> GraphDbResult<ReverseWalk> {
        let mut walk = ReverseWalk::default();
        let mut visited: HashSet<String> = HashSet::new();
        visited.insert(target_id.to_string());
        let mut frontier: Vec<(String, f64)> = vec![(target_id.to_string(), 1.0)];

        let mut distance = 0u32;
        while distance < max_hops && !frontier.is_empty() {
            distance += 1;
            let ids: Vec<&str> = frontier.iter().map(|(n, _)| n.as_str()).collect();
            let edges = self
                .scoped_hop_edges(tenant_id, "target_node_id", &ids, "", scope)
                .await?;
            let parent: HashMap<&str, f64> =
                frontier.iter().map(|(n, c)| (n.as_str(), *c)).collect();
            let ranked = best_arrivals(
                edges,
                &visited,
                &parent,
                min_edge_confidence,
                &mut walk.below_floor,
            );
            let mut next: Vec<(String, f64)> = Vec::new();
            for (src, (edge_type, confidence)) in ranked {
                if !visited.insert(src.clone()) {
                    continue;
                }
                walk.hits
                    .push((src.clone(), edge_type, distance, confidence));
                next.push((src, confidence));
                if visited.len() >= NODE_BUDGET {
                    walk.budget_reached = true;
                    break;
                }
            }
            frontier = next;
            if walk.budget_reached {
                break;
            }
        }

        if walk.budget_reached {
            warn!(
                "graph reverse_traverse: node budget {} reached from target {} — impact truncated",
                NODE_BUDGET, target_id
            );
        }
        Ok(walk)
    }
}

/// The best arrival per caller within one hop, highest confidence first with
/// node id as the total tiebreak (#367: a downstream `take(top_k)` decides the
/// ANSWER from this order). Gated per-edge (not on the cumulative product) so
/// deep high-confidence chains survive; a caller only a below-floor edge leads
/// to is recorded in `below_floor` instead.
fn best_arrivals(
    edges: Vec<HopEdge>,
    visited: &HashSet<String>,
    parent: &HashMap<&str, f64>,
    min_edge_confidence: f64,
    below_floor: &mut HashSet<String>,
) -> Vec<(String, (String, f64))> {
    let mut best: HashMap<String, (String, f64)> = HashMap::new();
    for e in edges {
        if visited.contains(&e.source) {
            continue;
        }
        if e.weight < min_edge_confidence {
            below_floor.insert(e.source);
            continue;
        }
        let pconf = parent.get(e.target.as_str()).copied().unwrap_or(1.0);
        let confidence = pconf * e.weight;
        let entry = best
            .entry(e.source)
            .or_insert((e.edge_type.clone(), f64::MIN));
        if confidence > entry.1 {
            *entry = (e.edge_type, confidence);
        }
    }
    let mut ranked: Vec<(String, (String, f64))> = best.into_iter().collect();
    ranked.sort_by(|(a_id, a_val), (b_id, b_val)| {
        b_val
            .1
            .partial_cmp(&a_val.1)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a_id.cmp(b_id))
    });
    ranked
}

/// The reached callers this branch holds, typed by how they depend on the
/// changed symbol.
fn impact_nodes(
    hits: Vec<(String, String, u32, f64)>,
    meta: &HashMap<String, NodeMeta>,
) -> Vec<ImpactNode> {
    hits.into_iter()
        .filter_map(|(id, edge_type, distance, confidence)| {
            meta.get(&id).map(|m| {
                let impact_type = match (distance, edge_type.as_str()) {
                    (1, "CALLS") => "direct_caller",
                    (1, "USES_TYPE") => "type_user",
                    (1, _) => "direct_reference",
                    (_, "CALLS") => "indirect_caller",
                    _ => "indirect_reference",
                };
                ImpactNode {
                    node_id: id.clone(),
                    symbol_name: m.symbol_name.clone(),
                    parent_symbol: m.parent_symbol.clone(),
                    file_path: m.file_path.clone(),
                    impact_type: impact_type.to_string(),
                    distance,
                    confidence,
                }
            })
        })
        .collect()
}
