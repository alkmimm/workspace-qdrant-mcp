//! What test code reaches: confidently, over the edges the coverage walk
//! follows, and only through an ambiguous same-name guess — reported, never
//! counted as covered.

use std::collections::{HashMap, HashSet, VecDeque};

use sqlx::{Row, SqlitePool};

use super::super::AdjacencyGraph;
use crate::graph::GraphScope;

/// The confidence gate the adjacency loader applies (`weight >= 0.6`): an edge
/// below it is one of N same-name candidates of an unresolved call.
const AMBIGUITY_GATE: f64 = 0.6;

/// Ids bound per `IN (...)` query, well under SQLite's variable limit.
const ID_CHUNK: usize = 400;

/// Every node a test reaches transitively over `outgoing`. The visited set
/// bounds the walk (each node enqueued once) — the graph is finite and
/// de-duplicated, so no separate node budget is needed.
pub(super) fn reached_from_tests<'g>(
    graph: &'g AdjacencyGraph,
    test_set: &HashSet<&'g str>,
) -> HashSet<&'g str> {
    let mut reached: HashSet<&str> = HashSet::new();
    let mut queue: VecDeque<&str> = VecDeque::new();
    for &t in test_set {
        if reached.insert(t) {
            queue.push_back(t);
        }
    }
    while let Some(cur) = queue.pop_front() {
        if let Some(targets) = graph.outgoing.get(cur) {
            for tgt in targets {
                // Only follow into nodes present in the graph — stub/excluded
                // endpoints are absent (same convention cycles/centrality use).
                if graph.nodes.contains_key(tgt) && reached.insert(tgt.as_str()) {
                    queue.push_back(tgt.as_str());
                }
            }
        }
    }
    reached
}

/// For each gap, how many test nodes call it only through an edge below the
/// ambiguity gate. Such a gap may well be tested: the test's call matched
/// several same-named definitions and the resolver could not tell which. The
/// walk above cannot follow that edge without fabricating coverage, so the
/// count is reported instead of silently dropped.
pub(super) async fn ambiguous_test_callers(
    pool: &SqlitePool,
    tenant_id: &str,
    scope: &GraphScope,
    edge_types: &[&str],
    test_ids: &[&str],
    gaps: &HashSet<&str>,
) -> Result<HashMap<String, u32>, sqlx::Error> {
    let mut pairs: HashSet<(String, String)> = HashSet::new();
    if gaps.is_empty() || edge_types.is_empty() {
        return Ok(HashMap::new());
    }
    let types: Vec<String> = (0..edge_types.len())
        .map(|i| format!("?{}", i + 3))
        .collect();
    for chunk in test_ids.chunks(ID_CHUNK) {
        let first = edge_types.len() + 3;
        let ids: Vec<String> = (0..chunk.len())
            .map(|i| format!("?{}", first + i))
            .collect();
        let sql = format!(
            "SELECT source_node_id, target_node_id, generation FROM graph_edges
             WHERE tenant_id = ?1 AND weight < ?2 AND edge_type IN ({})
               AND source_node_id IN ({})",
            types.join(", "),
            ids.join(", ")
        );
        let mut query = sqlx::query(&sql).bind(tenant_id).bind(AMBIGUITY_GATE);
        for t in edge_types {
            query = query.bind(*t);
        }
        for id in chunk {
            query = query.bind(*id);
        }
        for row in query.fetch_all(pool).await? {
            let target: String = row.get("target_node_id");
            if gaps.contains(target.as_str())
                && scope.admits(row.get::<String, _>("generation").as_str())
            {
                pairs.insert((row.get("source_node_id"), target));
            }
        }
    }
    let mut callers: HashMap<String, u32> = HashMap::new();
    for (_, target) in pairs {
        *callers.entry(target).or_default() += 1;
    }
    Ok(callers)
}

/// The class each of `ids` is a member of, for the nodes that have one: two
/// homonymous gaps in one file read as `Batch.set` and `Transaction.set`.
pub(super) async fn parent_symbols(
    pool: &SqlitePool,
    tenant_id: &str,
    ids: &[String],
) -> Result<HashMap<String, String>, sqlx::Error> {
    let mut parents: HashMap<String, String> = HashMap::new();
    for chunk in ids.chunks(ID_CHUNK) {
        let ph: Vec<String> = (0..chunk.len()).map(|i| format!("?{}", i + 2)).collect();
        let sql = format!(
            "SELECT DISTINCT node_id, parent_symbol FROM graph_nodes
             WHERE tenant_id = ?1 AND parent_symbol IS NOT NULL AND node_id IN ({})",
            ph.join(", ")
        );
        let mut query = sqlx::query(&sql).bind(tenant_id);
        for id in chunk {
            query = query.bind(id);
        }
        for row in query.fetch_all(pool).await? {
            parents.insert(row.get("node_id"), row.get("parent_symbol"));
        }
    }
    Ok(parents)
}
