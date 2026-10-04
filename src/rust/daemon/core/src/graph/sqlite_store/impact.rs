//! Scoped impact analysis: the reverse traversal (who depends on a symbol),
//! through the same [`GraphScope`] filter as the forward traversal.

use std::collections::{HashMap, HashSet};

use tracing::warn;

use super::traversal::{unique_ids, NODE_BUDGET};
use super::SqliteGraphStore;
use crate::graph::{GraphDbResult, GraphScope, ImpactNode, ImpactReport};

impl SqliteGraphStore {
    pub(super) async fn impact(
        &self,
        tenant_id: &str,
        symbol_name: &str,
        file_path: Option<&str>,
        scope: &GraphScope,
    ) -> GraphDbResult<ImpactReport> {
        let target_nodes = self
            .find_target_nodes(tenant_id, symbol_name, file_path, scope)
            .await?;

        // When the caller pins a `file_path`, they want the blast radius of ONE
        // definition: drop the R1 ambiguous fan-out (an unresolved call site's
        // 1/N edge to EVERY same-name definition, weight < 0.6) with the same
        // floor cycles and centrality apply. Without a file_path the query is
        // intentionally broad ("everything named X") and keeps every edge.
        const AMBIGUOUS_EDGE_CONFIDENCE_FLOOR: f64 = 0.6;
        let min_edge_confidence = if file_path.is_some() {
            AMBIGUOUS_EDGE_CONFIDENCE_FLOOR
        } else {
            0.0
        };

        let mut all_impacted = Vec::new();
        for target_id in &target_nodes {
            all_impacted.extend(
                self.reverse_traverse(tenant_id, target_id, min_edge_confidence, scope)
                    .await?,
            );
        }
        all_impacted.sort_by_key(|n| n.distance);
        let mut seen = HashSet::new();
        all_impacted.retain(|n| seen.insert(n.node_id.clone()));

        let total = all_impacted.len() as u32;
        Ok(ImpactReport {
            symbol_name: symbol_name.to_string(),
            impacted_nodes: all_impacted,
            total_impacted: total,
        })
    }

    async fn reverse_traverse(
        &self,
        tenant_id: &str,
        target_id: &str,
        min_edge_confidence: f64,
        scope: &GraphScope,
    ) -> GraphDbResult<Vec<ImpactNode>> {
        // Bounded breadth-first REVERSE traversal (callers of `target_id`, up to
        // 3 hops), same shape as `traverse_forward`. `min_edge_confidence`
        // prunes low-weight incoming edges before they enter the frontier
        // (0.0 = keep all, the unanchored default).
        const MAX_DISTANCE: u32 = 3;

        let mut visited: HashSet<String> = HashSet::new();
        visited.insert(target_id.to_string());
        let mut frontier: Vec<(String, f64)> = vec![(target_id.to_string(), 1.0)];
        // Reached callers in BFS order: (node_id, edge_type, distance, confidence).
        let mut hits: Vec<(String, String, u32, f64)> = Vec::new();
        let mut truncated = false;

        let mut distance = 0u32;
        while distance < MAX_DISTANCE && !frontier.is_empty() {
            distance += 1;
            let ids: Vec<&str> = frontier.iter().map(|(n, _)| n.as_str()).collect();
            let edges = self
                .scoped_hop_edges(tenant_id, "target_node_id", &ids, "", scope)
                .await?;

            let parent: HashMap<&str, f64> =
                frontier.iter().map(|(n, c)| (n.as_str(), *c)).collect();
            // Best arrival per caller within this hop (see traverse_forward).
            let mut best: HashMap<String, (String, f64)> = HashMap::new();
            for e in edges {
                if visited.contains(&e.source) {
                    continue;
                }
                // Gated per-edge (not on the cumulative product) so deep
                // high-confidence chains survive; 0.0 keeps every edge.
                if e.weight < min_edge_confidence {
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
            // Deterministic emission (#367).
            let mut ranked: Vec<(String, (String, f64))> = best.into_iter().collect();
            ranked.sort_by(|(a_id, a_val), (b_id, b_val)| {
                b_val
                    .1
                    .partial_cmp(&a_val.1)
                    .unwrap_or(std::cmp::Ordering::Equal)
                    .then_with(|| a_id.cmp(b_id))
            });
            let mut next: Vec<(String, f64)> = Vec::new();
            for (src, (edge_type, confidence)) in ranked {
                if !visited.insert(src.clone()) {
                    continue;
                }
                hits.push((src.clone(), edge_type, distance, confidence));
                next.push((src, confidence));
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
                "graph reverse_traverse: node budget {} reached from target {} — impact truncated",
                NODE_BUDGET, target_id
            );
        }
        if hits.is_empty() {
            return Ok(Vec::new());
        }

        let uniq_ids = unique_ids(hits.iter().map(|h| h.0.as_str()));
        let meta = self.admitted_node_meta(tenant_id, &uniq_ids, scope).await?;
        Ok(hits
            .into_iter()
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
                        file_path: m.file_path.clone(),
                        impact_type: impact_type.to_string(),
                        distance,
                        confidence,
                    }
                })
            })
            .collect())
    }
}
