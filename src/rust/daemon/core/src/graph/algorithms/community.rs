/// Community detection using label propagation algorithm.
use std::collections::HashMap;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use sqlx::SqlitePool;
use tracing::{debug, info, warn};

use super::load_adjacency_graph;

/// A detected community (cluster) of nodes.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Community {
    pub community_id: u32,
    pub members: Vec<CommunityMember>,
}

/// A node within a community.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CommunityMember {
    pub node_id: String,
    pub symbol_name: String,
    pub symbol_type: String,
    pub file_path: String,
}

/// Configuration for community detection.
#[derive(Debug, Clone)]
pub struct CommunityConfig {
    /// Maximum iterations for label propagation.
    pub max_iterations: usize,
    /// Minimum community size to include in results.
    pub min_community_size: usize,
}

impl Default for CommunityConfig {
    fn default() -> Self {
        Self {
            max_iterations: 50,
            min_community_size: 2,
        }
    }
}

/// Wall-clock safety budget for label propagation. The index-based pass is
/// sub-second even on tens of thousands of nodes, but a pathological graph
/// (dense, oscillating) must never hang the gRPC call until the client cancels
/// it — past this budget we stop and return the current (partial) labelling.
const LP_TIME_BUDGET: Duration = Duration::from_secs(20);

/// Detect communities using label propagation algorithm.
///
/// Each node starts with a unique label. In each iteration, each node adopts the
/// most frequent label among its neighbors. Converges when no labels change.
///
/// Treats edges as undirected. The pass runs over DENSE INTEGER node indices
/// (`Vec<u32>` labels + `Vec<Vec<usize>>` adjacency), not the node-id strings:
/// the previous `HashMap<&str, _>` form hashed a ~32-char id on every neighbor
/// visit, so at ~50k nodes the non-converging 50 iterations blew past the call
/// timeout (CANCELLED). Interning to indices removes that per-op string hashing.
pub async fn detect_communities(
    pool: &SqlitePool,
    tenant_id: &str,
    config: &CommunityConfig,
    edge_types: Option<&[&str]>,
) -> Result<Vec<Community>, sqlx::Error> {
    let graph = load_adjacency_graph(pool, tenant_id, edge_types).await?;

    if graph.nodes.is_empty() {
        return Ok(Vec::new());
    }

    // Intern node ids → dense indices. Sorted so the labelling is deterministic
    // (the old HashMap-key iteration order was not).
    let mut idx_to_id: Vec<&str> = graph.nodes.keys().map(|s| s.as_str()).collect();
    idx_to_id.sort_unstable();
    let id_to_idx: HashMap<&str, usize> = idx_to_id
        .iter()
        .enumerate()
        .map(|(i, &id)| (id, i))
        .collect();

    let neighbors = build_index_neighbors(&graph.outgoing, &id_to_idx);
    let labels = run_label_propagation(&neighbors, config.max_iterations, tenant_id);
    let communities =
        assemble_communities(&labels, &idx_to_id, &graph.nodes, config.min_community_size);

    info!(
        tenant_id,
        communities = communities.len(),
        "Community detection complete"
    );

    Ok(communities)
}

/// Build an undirected adjacency list over dense node indices from the directed
/// `outgoing` map. Edges whose endpoint is absent from `id_to_idx` (e.g. an
/// unresolved stub dropped at load time) are skipped; self-loops are dropped;
/// each neighbor list is sorted+deduped so a multi-edge counts a neighbor once.
fn build_index_neighbors(
    outgoing: &HashMap<String, Vec<String>>,
    id_to_idx: &HashMap<&str, usize>,
) -> Vec<Vec<usize>> {
    let mut neighbors: Vec<Vec<usize>> = vec![Vec::new(); id_to_idx.len()];
    for (src, targets) in outgoing {
        let Some(&s) = id_to_idx.get(src.as_str()) else {
            continue;
        };
        for tgt in targets {
            let Some(&t) = id_to_idx.get(tgt.as_str()) else {
                continue;
            };
            if s == t {
                continue;
            }
            neighbors[s].push(t);
            neighbors[t].push(s);
        }
    }
    for nbrs in &mut neighbors {
        nbrs.sort_unstable();
        nbrs.dedup();
    }
    neighbors
}

/// Run label-propagation over integer-indexed adjacency until convergence,
/// `max_iterations`, or [`LP_TIME_BUDGET`] (whichever comes first). Returns the
/// label assigned to each node index.
fn run_label_propagation(neighbors: &[Vec<usize>], max_iterations: usize, tenant_id: &str) -> Vec<u32> {
    let n = neighbors.len();
    let mut labels: Vec<u32> = (0..n as u32).collect();
    let start = Instant::now();
    // Reused across nodes/iterations to avoid a per-node allocation.
    let mut counts: HashMap<u32, usize> = HashMap::new();

    for iteration in 0..max_iterations {
        if start.elapsed() >= LP_TIME_BUDGET {
            warn!(
                tenant_id,
                iterations = iteration,
                "Label propagation hit the time budget; returning partial labelling"
            );
            break;
        }
        let mut changed = false;
        for v in 0..n {
            let nbrs = &neighbors[v];
            if nbrs.is_empty() {
                continue;
            }
            counts.clear();
            for &u in nbrs {
                *counts.entry(labels[u]).or_default() += 1;
            }
            // Most frequent neighbor label; tie broken toward the higher label
            // id (deterministic), matching the prior behaviour.
            let best = counts
                .iter()
                .max_by(|a, b| a.1.cmp(b.1).then(b.0.cmp(a.0)))
                .map(|(&label, _)| label)
                .unwrap();
            if labels[v] != best {
                labels[v] = best;
                changed = true;
            }
        }
        if !changed {
            debug!(
                tenant_id,
                iterations = iteration + 1,
                "Label propagation converged"
            );
            break;
        }
    }
    labels
}

/// Group labeled node indices into Community values and sort by size descending.
fn assemble_communities(
    labels: &[u32],
    idx_to_id: &[&str],
    nodes: &HashMap<String, super::NodeInfo>,
    min_size: usize,
) -> Vec<Community> {
    let mut groups: HashMap<u32, Vec<CommunityMember>> = HashMap::new();
    for (idx, &label) in labels.iter().enumerate() {
        let id = idx_to_id[idx];
        if let Some(info) = nodes.get(id) {
            groups.entry(label).or_default().push(CommunityMember {
                node_id: id.to_string(),
                symbol_name: info.symbol_name.clone(),
                symbol_type: info.symbol_type.clone(),
                file_path: info.file_path.clone(),
            });
        }
    }

    let mut communities: Vec<Community> = groups
        .into_values()
        .filter(|m| m.len() >= min_size)
        .map(|mut m| {
            m.sort_by(|a, b| a.symbol_name.cmp(&b.symbol_name));
            Community {
                community_id: 0,
                members: m,
            }
        })
        .collect();

    communities.sort_by_key(|c| std::cmp::Reverse(c.members.len()));
    for (i, c) in communities.iter_mut().enumerate() {
        c.community_id = i as u32;
    }
    communities
}

#[cfg(test)]
mod tests {
    use super::*;

    fn nbrs_from_edges(n: usize, edges: &[(usize, usize)]) -> Vec<Vec<usize>> {
        let mut nb = vec![Vec::new(); n];
        for &(a, b) in edges {
            nb[a].push(b);
            nb[b].push(a);
        }
        for v in &mut nb {
            v.sort_unstable();
            v.dedup();
        }
        nb
    }

    #[test]
    fn build_index_neighbors_dedups_multiedge_and_is_undirected() {
        let mut outgoing: HashMap<String, Vec<String>> = HashMap::new();
        outgoing.insert("a".into(), vec!["b".into(), "b".into()]); // multi-edge
        let id_to_idx: HashMap<&str, usize> = [("a", 0usize), ("b", 1)].into_iter().collect();
        let nb = build_index_neighbors(&outgoing, &id_to_idx);
        assert_eq!(nb[0], vec![1], "multi-edge deduped");
        assert_eq!(nb[1], vec![0], "undirected back-edge added");
    }

    #[test]
    fn build_index_neighbors_skips_unresolved_endpoints() {
        let mut outgoing: HashMap<String, Vec<String>> = HashMap::new();
        // 'b' is not in id_to_idx (e.g. a dropped stub) → that edge is skipped.
        outgoing.insert("a".into(), vec!["b".into()]);
        let id_to_idx: HashMap<&str, usize> = [("a", 0usize)].into_iter().collect();
        let nb = build_index_neighbors(&outgoing, &id_to_idx);
        assert!(nb[0].is_empty());
    }

    #[test]
    fn lp_separates_two_disjoint_triangles() {
        let nb = nbrs_from_edges(6, &[(0, 1), (1, 2), (0, 2), (3, 4), (4, 5), (3, 5)]);
        let labels = run_label_propagation(&nb, 50, "t");
        assert_eq!(labels[0], labels[1]);
        assert_eq!(labels[1], labels[2]);
        assert_eq!(labels[3], labels[4]);
        assert_eq!(labels[4], labels[5]);
        assert_ne!(labels[0], labels[3], "disjoint triangles get distinct labels");
    }

    /// Scaling regression guard: with the old `HashMap<&str,_>` form this size
    /// blew past the gRPC call timeout (the `graph modules` CANCELLED report).
    /// Index-based it completes in milliseconds. 20k disjoint triangles =
    /// 60k nodes / 60k edges → exactly one community per triangle.
    #[test]
    fn lp_scales_to_60k_nodes_without_hanging() {
        const TRIS: usize = 20_000;
        let mut edges = Vec::with_capacity(TRIS * 3);
        for t in 0..TRIS {
            let a = t * 3;
            edges.push((a, a + 1));
            edges.push((a + 1, a + 2));
            edges.push((a, a + 2));
        }
        let nb = nbrs_from_edges(TRIS * 3, &edges);
        let labels = run_label_propagation(&nb, 50, "t");
        assert_eq!(labels[0], labels[2], "triangle members share a label");
        assert_ne!(labels[0], labels[3], "distinct triangles differ");
        let distinct: std::collections::HashSet<u32> = labels.iter().copied().collect();
        assert_eq!(distinct.len(), TRIS, "one community per triangle");
    }
}
