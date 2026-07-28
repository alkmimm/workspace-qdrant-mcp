//! Dependency-cycle detection via iterative Tarjan SCC.
//!
//! A dependency cycle is a strongly-connected component (SCC) of the directed
//! dependency graph with ≥ 2 members (or a single node with a self-loop). The
//! architecturally-interesting case is a **cross-file** cycle (a circular
//! file-level dependency); same-file SCCs are usually benign mutual recursion.
//!
//! The SCCs are found with an **iterative** Tarjan (explicit work stack): a
//! recursive DFS overflows the call stack on deep graphs — the same class of
//! failure the graph traversal fixed by going iterative (#176). Runs over dense
//! integer node indices (like `community`) to avoid per-edge string hashing.
//!
//! **Precision caveat (observed on the live graph):** cross-file 2-cycles
//! through generic method names (`execute`, `new`, …) can be by-name resolution
//! artifacts rather than true dependency cycles. The `weight >= 0.6` gate drops
//! the 1/N ambiguous fan-out, but a uniquely-yet-wrongly-resolved generic call
//! can still close a spurious cycle. The tool surfaces *candidates* ranked
//! cross-file-first; the caller judges. Same-file mutual recursion (the measured
//! majority — 58/62 on this repo, 46/47 on DOC-V2) is reported but flagged
//! `cross_file = false`, so it never buries the rare cross-file smells.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};
use sqlx::SqlitePool;
use tracing::info;

use super::load_adjacency_graph;

/// A member node of a dependency cycle.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CycleMember {
    pub node_id: String,
    pub symbol_name: String,
    pub symbol_type: String,
    pub file_path: String,
}

/// A detected dependency cycle: an SCC of size ≥ 2 (or a self-looping node).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Cycle {
    pub members: Vec<CycleMember>,
    /// Distinct files the cycle spans (sorted, deduped).
    pub files: Vec<String>,
    /// True when the cycle crosses more than one file — the architecturally
    /// interesting signal (a circular file-level dependency), vs same-file
    /// mutual recursion.
    pub cross_file: bool,
}

/// Default dependency edge types considered for cycles. CALLS/IMPORTS plus the
/// type/inheritance relations are the meaningful dependency edges. CONTAINS
/// (structural parent→child) is excluded: it would make every class trivially
/// "cyclic" with its own methods.
const DEFAULT_CYCLE_EDGE_TYPES: &[&str] =
    &["CALLS", "IMPORTS", "USES_TYPE", "EXTENDS", "IMPLEMENTS"];

/// Detect dependency cycles for a tenant.
///
/// Builds the directed, high-confidence (weight ≥ 0.6) dependency graph WITHOUT
/// the centrality genericity filters — a real cycle may legitimately pass
/// through a high-in-degree node, so those precision-for-ranking filters would
/// hide cycles. The weight ≥ 0.6 gate is kept: it drops the 1/N ambiguous
/// fan-out that would otherwise fabricate spurious cycles.
///
/// Returns every SCC with at least `min_cycle_size` members (a self-loop counts
/// as a size-1 cycle when `min_cycle_size <= 1`), cross-file cycles first, then
/// larger first.
pub async fn detect_cycles(
    pool: &SqlitePool,
    tenant_id: &str,
    edge_types: Option<&[&str]>,
    min_cycle_size: usize,
) -> Result<Vec<Cycle>, sqlx::Error> {
    let types = edge_types.unwrap_or(DEFAULT_CYCLE_EDGE_TYPES);
    // apply_genericity_filters = false: keep the raw resolved dependency graph.
    let graph = load_adjacency_graph(pool, tenant_id, Some(types), false).await?;
    if graph.nodes.is_empty() {
        return Ok(Vec::new());
    }

    // Intern node ids → dense indices, sorted so output is deterministic.
    let mut idx_to_id: Vec<&str> = graph.nodes.keys().map(|s| s.as_str()).collect();
    idx_to_id.sort_unstable();
    let id_to_idx: HashMap<&str, usize> = idx_to_id
        .iter()
        .enumerate()
        .map(|(i, &id)| (id, i))
        .collect();
    let n = idx_to_id.len();

    // Directed adjacency over dense indices; self-loops tracked separately (a
    // self-loop is a size-1 SCC that Tarjan won't otherwise flag as a cycle).
    let mut adj: Vec<Vec<usize>> = vec![Vec::new(); n];
    let mut has_self_loop = vec![false; n];
    for (src, targets) in &graph.outgoing {
        let Some(&s) = id_to_idx.get(src.as_str()) else {
            continue;
        };
        for tgt in targets {
            let Some(&t) = id_to_idx.get(tgt.as_str()) else {
                continue;
            };
            if s == t {
                has_self_loop[s] = true;
                continue;
            }
            adj[s].push(t);
        }
    }
    for a in &mut adj {
        a.sort_unstable();
        a.dedup();
    }

    let sccs = tarjan_scc(&adj);

    let min = min_cycle_size.max(1);
    let mut cycles: Vec<Cycle> = Vec::new();
    for comp in sccs {
        let is_cycle = comp.len() >= 2 || (comp.len() == 1 && has_self_loop[comp[0]]);
        if !is_cycle || comp.len() < min {
            continue;
        }
        let mut members: Vec<CycleMember> = comp
            .iter()
            .filter_map(|&i| {
                let id = idx_to_id[i];
                graph.nodes.get(id).map(|info| CycleMember {
                    node_id: id.to_string(),
                    symbol_name: info.symbol_name.clone(),
                    symbol_type: info.symbol_type.clone(),
                    file_path: info.file_path.clone(),
                })
            })
            .collect();
        members.sort_by(|a, b| {
            a.file_path
                .cmp(&b.file_path)
                .then_with(|| a.symbol_name.cmp(&b.symbol_name))
        });
        let mut files: Vec<String> = members.iter().map(|m| m.file_path.clone()).collect();
        files.sort_unstable();
        files.dedup();
        let cross_file = files.len() > 1;
        cycles.push(Cycle {
            members,
            files,
            cross_file,
        });
    }

    // Cross-file cycles first (the valuable ones), then larger first.
    cycles.sort_by(|a, b| {
        b.cross_file
            .cmp(&a.cross_file)
            .then_with(|| b.members.len().cmp(&a.members.len()))
    });

    info!(
        tenant_id,
        cycles = cycles.len(),
        "Dependency cycle detection complete"
    );
    Ok(cycles)
}

/// Iterative Tarjan strongly-connected-components over dense integer adjacency.
///
/// Returns each SCC as a list of node indices. Uses an explicit work stack of
/// `(node, neighbor-cursor)` frames instead of recursion, so a deep graph
/// cannot overflow the call stack.
fn tarjan_scc(adj: &[Vec<usize>]) -> Vec<Vec<usize>> {
    let n = adj.len();
    const UNVISITED: i64 = -1;
    let mut index = vec![UNVISITED; n]; // DFS discovery order
    let mut lowlink = vec![0i64; n];
    let mut on_stack = vec![false; n];
    let mut scc_stack: Vec<usize> = Vec::new(); // Tarjan's node stack
    let mut next_index: i64 = 0;
    let mut sccs: Vec<Vec<usize>> = Vec::new();

    for start in 0..n {
        if index[start] != UNVISITED {
            continue;
        }
        // Explicit DFS work stack: (node, next-neighbor-cursor).
        let mut work: Vec<(usize, usize)> = vec![(start, 0)];
        while let Some(&(v, ci)) = work.last() {
            if ci == 0 {
                // First entry into v.
                index[v] = next_index;
                lowlink[v] = next_index;
                next_index += 1;
                scc_stack.push(v);
                on_stack[v] = true;
            }
            if ci < adj[v].len() {
                let w = adj[v][ci];
                work.last_mut().unwrap().1 = ci + 1; // advance v's cursor
                if index[w] == UNVISITED {
                    work.push((w, 0)); // descend into w
                } else if on_stack[w] {
                    // Back/cross edge to a node still on the SCC stack.
                    lowlink[v] = lowlink[v].min(index[w]);
                }
            } else {
                // All of v's neighbors processed. If v is an SCC root, pop it.
                if lowlink[v] == index[v] {
                    let mut comp = Vec::new();
                    loop {
                        let w = scc_stack.pop().unwrap();
                        on_stack[w] = false;
                        comp.push(w);
                        if w == v {
                            break;
                        }
                    }
                    sccs.push(comp);
                }
                work.pop();
                // Propagate v's lowlink up to its DFS parent.
                if let Some(&(parent, _)) = work.last() {
                    lowlink[parent] = lowlink[parent].min(lowlink[v]);
                }
            }
        }
    }
    sccs
}

#[cfg(test)]
mod tests {
    use super::*;
    use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};

    // ── Pure Tarjan SCC ────────────────────────────────────────────────

    /// Sort each SCC and the list of SCCs for order-independent comparison.
    fn normalize(mut sccs: Vec<Vec<usize>>) -> Vec<Vec<usize>> {
        for c in &mut sccs {
            c.sort_unstable();
        }
        sccs.sort_unstable();
        sccs
    }

    #[test]
    fn tarjan_single_cycle() {
        // 0 → 1 → 2 → 0
        let adj = vec![vec![1], vec![2], vec![0]];
        assert_eq!(normalize(tarjan_scc(&adj)), vec![vec![0, 1, 2]]);
    }

    #[test]
    fn tarjan_acyclic_chain_all_singletons() {
        // 0 → 1 → 2 (no back edge)
        let adj = vec![vec![1], vec![2], vec![]];
        assert_eq!(normalize(tarjan_scc(&adj)), vec![vec![0], vec![1], vec![2]]);
    }

    #[test]
    fn tarjan_two_disjoint_cycles() {
        // 0↔1 and 2↔3
        let adj = vec![vec![1], vec![0], vec![3], vec![2]];
        assert_eq!(normalize(tarjan_scc(&adj)), vec![vec![0, 1], vec![2, 3]]);
    }

    #[test]
    fn tarjan_cycle_with_dangling_tail() {
        // 0 → 1 → 2 → 0, and 2 → 3 (3 is a sink, not in the cycle)
        let adj = vec![vec![1], vec![2], vec![0, 3], vec![]];
        assert_eq!(
            normalize(tarjan_scc(&adj)),
            vec![vec![0, 1, 2], vec![3]]
        );
    }

    /// A deep chain closed into ONE giant SCC. Depth 20k would blow a recursive
    /// DFS's call stack; the iterative form must return a single SCC of all
    /// nodes. Guards the #176-class overflow.
    #[test]
    fn tarjan_deep_chain_is_one_scc_no_overflow() {
        const N: usize = 20_000;
        let mut adj = vec![Vec::new(); N];
        for (i, item) in adj.iter_mut().enumerate().take(N - 1) {
            item.push(i + 1);
        }
        adj[N - 1].push(0); // close the loop
        let sccs = tarjan_scc(&adj);
        assert_eq!(sccs.len(), 1, "the closed chain is a single SCC");
        assert_eq!(sccs[0].len(), N);
    }

    // ── End-to-end detect_cycles over SQLite ───────────────────────────

    async fn mem_pool() -> SqlitePool {
        let opts = SqliteConnectOptions::new()
            .filename(":memory:")
            .create_if_missing(true);
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect_with(opts)
            .await
            .unwrap();
        // Minimal schema: only the columns load_adjacency_graph reads.
        sqlx::query(
            "CREATE TABLE graph_nodes (node_id TEXT PRIMARY KEY, tenant_id TEXT, \
             symbol_name TEXT, symbol_type TEXT, file_path TEXT, \
             is_test_symbol INTEGER NOT NULL DEFAULT 0)",
        )
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query(
            "CREATE TABLE graph_edges (tenant_id TEXT, source_node_id TEXT, \
             target_node_id TEXT, edge_type TEXT, weight REAL)",
        )
        .execute(&pool)
        .await
        .unwrap();
        pool
    }

    async fn node(pool: &SqlitePool, id: &str, name: &str, file: &str) {
        sqlx::query(
            "INSERT INTO graph_nodes (node_id, tenant_id, symbol_name, symbol_type, file_path) \
             VALUES (?1, 't', ?2, 'function', ?3)",
        )
        .bind(id)
        .bind(name)
        .bind(file)
        .execute(pool)
        .await
        .unwrap();
    }

    async fn edge(pool: &SqlitePool, src: &str, tgt: &str, weight: f64) {
        sqlx::query(
            "INSERT INTO graph_edges (tenant_id, source_node_id, target_node_id, edge_type, weight) \
             VALUES ('t', ?1, ?2, 'CALLS', ?3)",
        )
        .bind(src)
        .bind(tgt)
        .bind(weight)
        .execute(pool)
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn detect_cross_file_cycle() {
        let pool = mem_pool().await;
        node(&pool, "a", "a", "a.rs").await;
        node(&pool, "b", "b", "b.rs").await;
        node(&pool, "c", "c", "c.rs").await;
        // a → b → c → a, all high-confidence.
        edge(&pool, "a", "b", 1.0).await;
        edge(&pool, "b", "c", 1.0).await;
        edge(&pool, "c", "a", 1.0).await;

        let cycles = detect_cycles(&pool, "t", None, 2).await.unwrap();
        assert_eq!(cycles.len(), 1, "one cycle");
        let c = &cycles[0];
        assert_eq!(c.members.len(), 3);
        assert!(c.cross_file, "spans 3 files → cross-file");
        assert_eq!(c.files, vec!["a.rs", "b.rs", "c.rs"]);
    }

    #[tokio::test]
    async fn acyclic_graph_has_no_cycles() {
        let pool = mem_pool().await;
        node(&pool, "a", "a", "a.rs").await;
        node(&pool, "b", "b", "b.rs").await;
        node(&pool, "c", "c", "c.rs").await;
        edge(&pool, "a", "b", 1.0).await;
        edge(&pool, "b", "c", 1.0).await;
        assert!(detect_cycles(&pool, "t", None, 2).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn low_confidence_edge_does_not_fabricate_a_cycle() {
        let pool = mem_pool().await;
        node(&pool, "a", "a", "a.rs").await;
        node(&pool, "b", "b", "b.rs").await;
        edge(&pool, "a", "b", 1.0).await;
        // The back-edge that would close the cycle is a 1/N ambiguous fan-out
        // (weight 0.2 < the 0.6 gate) → must be ignored → no cycle.
        edge(&pool, "b", "a", 0.2).await;
        assert!(
            detect_cycles(&pool, "t", None, 2).await.unwrap().is_empty(),
            "sub-0.6 back-edge must not create a cycle"
        );
    }

    #[tokio::test]
    async fn same_file_mutual_recursion_is_not_cross_file() {
        let pool = mem_pool().await;
        node(&pool, "a", "a", "same.rs").await;
        node(&pool, "b", "b", "same.rs").await;
        edge(&pool, "a", "b", 1.0).await;
        edge(&pool, "b", "a", 1.0).await;
        let cycles = detect_cycles(&pool, "t", None, 2).await.unwrap();
        assert_eq!(cycles.len(), 1);
        assert!(!cycles[0].cross_file, "single-file recursion is not cross-file");
    }

    // ── Scenarios grounded in real graph data ──────────────────────────
    // Mined from the live graph (weight ≥ 0.6, dependency edges):
    //   workspace-qdrant-mcp: 62 two-cycles — 58 same-file, 4 cross-file
    //   DOC-V2:               47 two-cycles — 46 same-file, 1 cross-file
    //   both: 0 self-loops. The 58:4 ratio makes cross-file-first ordering
    //   load-bearing, and the same-file majority is benign mutual recursion.

    /// Real pattern (DOC-V2): a repository and a service that call each other
    /// across files — `getResourceViewUrl`(resources_repository.dart) ↔
    /// `getViewUrl`(resources_service.dart). A 2-node cross-file cycle is the
    /// canonical layering smell the detector exists to surface.
    #[tokio::test]
    async fn cross_file_two_node_repository_service_cycle() {
        let pool = mem_pool().await;
        node(&pool, "repo", "getResourceViewUrl", "resources_repository.dart").await;
        node(&pool, "svc", "getViewUrl", "resources_service.dart").await;
        edge(&pool, "repo", "svc", 1.0).await;
        edge(&pool, "svc", "repo", 1.0).await;
        let cycles = detect_cycles(&pool, "t", None, 2).await.unwrap();
        assert_eq!(cycles.len(), 1);
        assert_eq!(cycles[0].members.len(), 2);
        assert!(cycles[0].cross_file);
        assert_eq!(cycles[0].files.len(), 2);
    }

    /// INVARIANT (grounded in the measured 58:4 same:cross ratio): cross-file
    /// cycles must sort BEFORE same-file ones, so `--top-k` surfaces the rare
    /// architectural smells instead of burying them under benign same-file
    /// mutual recursion.
    #[tokio::test]
    async fn cross_file_cycles_sort_before_same_file() {
        let pool = mem_pool().await;
        // Three same-file 2-cycles (benign mutual recursion — the majority case).
        for i in 0..3 {
            let (a, b, f) = (format!("a{i}"), format!("b{i}"), format!("same{i}.rs"));
            node(&pool, &a, &a, &f).await;
            node(&pool, &b, &b, &f).await;
            edge(&pool, &a, &b, 1.0).await;
            edge(&pool, &b, &a, 1.0).await;
        }
        // One cross-file 2-cycle (the smell).
        node(&pool, "x", "x", "x.rs").await;
        node(&pool, "y", "y", "y.rs").await;
        edge(&pool, "x", "y", 1.0).await;
        edge(&pool, "y", "x", 1.0).await;

        let cycles = detect_cycles(&pool, "t", None, 2).await.unwrap();
        assert_eq!(cycles.len(), 4, "3 same-file + 1 cross-file");
        assert!(cycles[0].cross_file, "the cross-file cycle sorts first");
        assert!(
            cycles[1..].iter().all(|c| !c.cross_file),
            "the same-file cycles follow"
        );
    }

    /// A larger SCC (a→b→c→d→a) across files is one cycle of four.
    #[tokio::test]
    async fn larger_cross_file_scc() {
        let pool = mem_pool().await;
        for (id, f) in [("a", "a.rs"), ("b", "b.rs"), ("c", "c.rs"), ("d", "d.rs")] {
            node(&pool, id, id, f).await;
        }
        edge(&pool, "a", "b", 1.0).await;
        edge(&pool, "b", "c", 1.0).await;
        edge(&pool, "c", "d", 1.0).await;
        edge(&pool, "d", "a", 1.0).await;
        let cycles = detect_cycles(&pool, "t", None, 2).await.unwrap();
        assert_eq!(cycles.len(), 1);
        assert_eq!(cycles[0].members.len(), 4);
        assert!(cycles[0].cross_file);
    }

    /// A diamond DAG (a→b, a→c, b→d, c→d) is acyclic — a common shape the
    /// detector must NOT report (no back edge closes it).
    #[tokio::test]
    async fn diamond_dag_is_not_a_cycle() {
        let pool = mem_pool().await;
        for id in ["a", "b", "c", "d"] {
            node(&pool, id, id, &format!("{id}.rs")).await;
        }
        edge(&pool, "a", "b", 1.0).await;
        edge(&pool, "a", "c", 1.0).await;
        edge(&pool, "b", "d", 1.0).await;
        edge(&pool, "c", "d", 1.0).await;
        assert!(detect_cycles(&pool, "t", None, 2).await.unwrap().is_empty());
    }

    /// Two disjoint cycles plus an acyclic tail → exactly two cycles.
    #[tokio::test]
    async fn multiple_independent_cycles() {
        let pool = mem_pool().await;
        for id in ["a", "b", "c", "d", "e"] {
            node(&pool, id, id, &format!("{id}.rs")).await;
        }
        edge(&pool, "a", "b", 1.0).await;
        edge(&pool, "b", "a", 1.0).await; // cycle 1
        edge(&pool, "c", "d", 1.0).await;
        edge(&pool, "d", "c", 1.0).await; // cycle 2
        edge(&pool, "a", "e", 1.0).await; // acyclic tail
        let cycles = detect_cycles(&pool, "t", None, 2).await.unwrap();
        assert_eq!(cycles.len(), 2);
        assert!(cycles.iter().all(|c| c.members.len() == 2));
    }

    /// RULE: a self-loop (direct recursion) is a size-1 cycle — excluded by the
    /// default `min_cycle_size = 2`, included only when it is ≤ 1. (Measured:
    /// 0 such self-edges at weight ≥ 0.6 in the live graph, so the default hides
    /// nothing real while keeping the common case quiet.)
    #[tokio::test]
    async fn self_loop_gated_by_min_size() {
        let pool = mem_pool().await;
        node(&pool, "r", "recurse", "r.rs").await;
        edge(&pool, "r", "r", 1.0).await;
        assert!(
            detect_cycles(&pool, "t", None, 2).await.unwrap().is_empty(),
            "min 2 skips self-loops"
        );
        let with1 = detect_cycles(&pool, "t", None, 1).await.unwrap();
        assert_eq!(with1.len(), 1, "min 1 reports the self-loop");
        assert_eq!(with1[0].members.len(), 1);
        assert!(!with1[0].cross_file);
    }
}
