//! Test-gap detection: production symbols no test reaches over the call graph.
//!
//! A production definition is a **gap** when NO test node reaches it — directly
//! or transitively — by following call/type-use edges forward from test code.
//! This relates production symbols to their tests structurally, instead of
//! grepping for a `test_<name>` by hand.
//!
//! **Coverage caveat (important — state it in every surface).** This is
//! CALL-GRAPH REACHABILITY from test code, an *approximation* of test coverage,
//! NOT execution coverage: a symbol reached by a test that never asserts on it
//! still counts as covered, and a symbol whose only resolving call edge is below
//! the graph's `weight >= 0.6` ambiguity gate reads as a gap. It complements —
//! does not replace — real coverage tools, and needs no test run, just the index.
//!
//! **Test detection.** A node counts as a test when its FILE is a test file
//! (`is_test_file`: `*.test.ts`, `*.spec.ts`, `*_test.rs`, files under `tests/`)
//! OR the extractor tagged the SYMBOL as an inline test (`is_test_symbol`). The
//! symbol flag closes the Rust blind spot: `#[cfg(test)] mod tests { … }` and
//! `#[test]`-family functions live in the SAME production `.rs` file, so a path
//! check alone would leave the production symbols they exercise reading as gaps.
//! The extractor tags those symbols (`#[cfg(test)]` modules and `#[test]` /
//! `#[tokio::test]` / `#[rstest]` / `#[test_case]` attributes) at index time, so
//! inline unit tests now seed coverage like any other test — a tenant must be
//! (re)indexed after the schema bump for the flag to populate.

use std::collections::{HashSet, VecDeque};
use std::path::Path;

use serde::{Deserialize, Serialize};
use sqlx::SqlitePool;
use tracing::info;

use super::load_adjacency_graph;
use crate::file_classification::is_test_file;

/// A production definition that no test reaches over the call graph.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TestGap {
    pub node_id: String,
    pub symbol_name: String,
    pub symbol_type: String,
    pub file_path: String,
    /// How many PRODUCTION nodes depend on this symbol (incoming edges whose
    /// source is a non-test node). High = important untested code: many callers
    /// rely on something no test exercises. Drives the ranking.
    pub production_dependents: u32,
}

/// Coverage-by-reachability summary + ranked gaps for a tenant.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TestGapsReport {
    /// Production definition nodes considered (testable symbol types, on
    /// non-test, non-excluded files).
    pub total_production: u32,
    /// Of those, how many a test reaches over the call graph.
    pub covered: u32,
    /// Ranked gaps (production_dependents desc, then name). Truncated to `top_k`;
    /// `gap_count` stays the true total.
    pub gaps: Vec<TestGap>,
    pub gap_count: u32,
}

/// Edge types that mean "exercised by": a test that CALLS production code, or
/// USES_TYPE of a production type, exercises it. IMPORTS is deliberately absent —
/// importing a symbol is not testing it.
const DEFAULT_TEST_GAP_EDGE_TYPES: &[&str] = &["CALLS", "USES_TYPE"];

/// Symbol types that are meaningful test-gap CANDIDATES — the things one writes
/// tests against. Excludes modules, imports, variables, constants, fields, which
/// would only inflate the gap count with un-testable noise.
fn is_testable_symbol_type(symbol_type: &str) -> bool {
    matches!(
        symbol_type,
        "function" | "method" | "class" | "struct" | "interface" | "trait" | "enum"
    )
}

/// Detect production symbols not reached by any test over the call graph.
///
/// `top_k` caps the returned `gaps` (0/absent = all); `gap_count` stays the true
/// total. Gaps are ranked by production in-degree (most-depended-upon first),
/// so the first entries are the highest-leverage untested code. See the module
/// docs for the coverage-approximation caveat.
pub async fn detect_test_gaps(
    pool: &SqlitePool,
    tenant_id: &str,
    edge_types: Option<&[&str]>,
    top_k: usize,
) -> Result<TestGapsReport, sqlx::Error> {
    let types = edge_types.unwrap_or(DEFAULT_TEST_GAP_EDGE_TYPES);
    // apply_genericity_filters = false: keep the raw resolved graph — a heavily
    // used production symbol must still be judged tested-or-not, not filtered
    // away for being "generic". The loader already drops stub nodes (empty
    // file_path), sub-0.6 ambiguous edges, and WQM_GRAPH_EXCLUDE paths, so
    // generated/legacy trees are out of the coverage picture too.
    let graph = load_adjacency_graph(pool, tenant_id, Some(types), false).await?;
    if graph.nodes.is_empty() {
        return Ok(TestGapsReport {
            total_production: 0,
            covered: 0,
            gaps: Vec::new(),
            gap_count: 0,
        });
    }

    // Classify each node once (file-path parsing is not free at graph scale):
    // a node is TEST if its FILE is a test file (`is_test_file`: `*.test.ts`,
    // `tests/` dirs, …) OR the extractor tagged the SYMBOL as an inline test
    // (`is_test_symbol`: a Rust `#[cfg(test)]` / `#[test]`-family symbol that
    // shares a production `.rs` file, which the path check alone cannot see).
    let test_set: HashSet<&str> = graph
        .nodes
        .iter()
        .filter(|(_, info)| info.is_test_symbol || is_test_file(Path::new(&info.file_path)))
        .map(|(id, _)| id.as_str())
        .collect();

    // Forward-BFS from ALL test nodes over `outgoing`: every node a test reaches
    // transitively. The visited set bounds it (each node enqueued once) — the
    // graph is finite and de-duplicated, so no separate node budget is needed.
    let mut reached: HashSet<&str> = HashSet::new();
    let mut queue: VecDeque<&str> = VecDeque::new();
    for &t in &test_set {
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

    // Production candidates = testable-typed nodes on non-test files. A candidate
    // not in `reached` is a gap, ranked by how many PRODUCTION nodes call it.
    let mut total_production = 0u32;
    let mut covered = 0u32;
    let mut gaps: Vec<TestGap> = Vec::new();
    for (id, info) in &graph.nodes {
        if test_set.contains(id.as_str()) || !is_testable_symbol_type(&info.symbol_type) {
            continue;
        }
        total_production += 1;
        if reached.contains(id.as_str()) {
            covered += 1;
            continue;
        }
        let production_dependents = graph
            .incoming
            .get(id)
            .map(|srcs| {
                srcs.iter()
                    .filter(|s| !test_set.contains(s.as_str()))
                    .count() as u32
            })
            .unwrap_or(0);
        gaps.push(TestGap {
            node_id: id.clone(),
            symbol_name: info.symbol_name.clone(),
            symbol_type: info.symbol_type.clone(),
            file_path: info.file_path.clone(),
            production_dependents,
        });
    }

    let gap_count = gaps.len() as u32;

    // Most-depended-upon untested code first; deterministic tie-break by name
    // then node_id.
    gaps.sort_by(|a, b| {
        b.production_dependents
            .cmp(&a.production_dependents)
            .then_with(|| a.symbol_name.cmp(&b.symbol_name))
            .then_with(|| a.node_id.cmp(&b.node_id))
    });
    if top_k > 0 && top_k < gaps.len() {
        gaps.truncate(top_k);
    }

    info!(
        "GraphService test-gaps: tenant={} production={} covered={} gaps={}",
        tenant_id, total_production, covered, gap_count
    );

    Ok(TestGapsReport {
        total_production,
        covered,
        gaps,
        gap_count,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use sqlx::sqlite::SqlitePoolOptions;

    const T: &str = "t1";

    async fn pool() -> SqlitePool {
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .unwrap();
        sqlx::query(
            "CREATE TABLE graph_nodes (
                node_id TEXT PRIMARY KEY, tenant_id TEXT NOT NULL,
                symbol_name TEXT NOT NULL, symbol_type TEXT NOT NULL,
                file_path TEXT NOT NULL, start_line INTEGER, end_line INTEGER,
                signature TEXT, language TEXT,
                is_test_symbol INTEGER NOT NULL DEFAULT 0,
                created_at TEXT NOT NULL DEFAULT '', updated_at TEXT NOT NULL DEFAULT '')",
        )
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query(
            "CREATE TABLE graph_edges (
                edge_id TEXT PRIMARY KEY, tenant_id TEXT NOT NULL,
                source_node_id TEXT NOT NULL, target_node_id TEXT NOT NULL,
                edge_type TEXT NOT NULL, source_file TEXT NOT NULL,
                weight REAL DEFAULT 1.0, metadata_json TEXT,
                created_at TEXT NOT NULL DEFAULT '')",
        )
        .execute(&pool)
        .await
        .unwrap();
        pool
    }

    async fn node(pool: &SqlitePool, id: &str, name: &str, stype: &str, file_path: &str) {
        sqlx::query(
            "INSERT INTO graph_nodes (node_id, tenant_id, symbol_name, symbol_type, file_path)
             VALUES (?, ?, ?, ?, ?)",
        )
        .bind(id)
        .bind(T)
        .bind(name)
        .bind(stype)
        .bind(file_path)
        .execute(pool)
        .await
        .unwrap();
    }

    /// A node tagged `is_test_symbol = 1` on a PRODUCTION file path — a Rust
    /// inline unit test (`#[cfg(test)]`), which `is_test_file` cannot detect.
    async fn inline_test_node(pool: &SqlitePool, id: &str, name: &str, file_path: &str) {
        sqlx::query(
            "INSERT INTO graph_nodes
                (node_id, tenant_id, symbol_name, symbol_type, file_path, is_test_symbol)
             VALUES (?, ?, ?, 'function', ?, 1)",
        )
        .bind(id)
        .bind(T)
        .bind(name)
        .bind(file_path)
        .execute(pool)
        .await
        .unwrap();
    }

    async fn edge(pool: &SqlitePool, src: &str, tgt: &str) {
        sqlx::query(
            "INSERT INTO graph_edges
                (edge_id, tenant_id, source_node_id, target_node_id, edge_type, source_file)
             VALUES (?, ?, ?, ?, 'CALLS', 's.rs')",
        )
        .bind(format!("{src}->{tgt}"))
        .bind(T)
        .bind(src)
        .bind(tgt)
        .execute(pool)
        .await
        .unwrap();
    }

    /// Covered (direct + transitive), an untested cluster ranked by prod
    /// in-degree, a non-testable type excluded, and the summary counts.
    #[tokio::test]
    async fn detects_gaps_covered_and_ranking() {
        let p = pool().await;
        // Covered branch: test → handler → service.
        node(&p, "tm", "test_main", "function", "main_test.rs").await;
        node(&p, "h", "handler", "function", "handler.rs").await;
        node(&p, "s", "service", "function", "service.rs").await;
        // Untested cluster: orphan_p → orphan_q ← orphan_r (q has 2 prod deps).
        node(&p, "op", "orphan_p", "function", "orphan.rs").await;
        node(&p, "oq", "orphan_q", "function", "helpers.rs").await;
        node(&p, "orr", "orphan_r", "function", "worker.rs").await;
        // Non-testable type on a non-test file → must NOT be a candidate.
        node(&p, "cfg", "MAX", "constant", "config.rs").await;
        edge(&p, "tm", "h").await;
        edge(&p, "h", "s").await;
        edge(&p, "op", "oq").await;
        edge(&p, "orr", "oq").await;

        let r = detect_test_gaps(&p, T, None, 0).await.unwrap();

        // handler, service, orphan_p/q/r are the 5 production candidates (MAX excluded).
        assert_eq!(r.total_production, 5, "constant MAX excluded from candidates");
        // handler + service reached transitively from test_main.
        assert_eq!(r.covered, 2);
        assert_eq!(r.gap_count, 3);
        let names: Vec<&str> = r.gaps.iter().map(|g| g.symbol_name.as_str()).collect();
        assert!(!names.contains(&"handler") && !names.contains(&"service"), "covered not gaps");
        assert!(!names.contains(&"MAX"), "non-testable type not a gap");
        // Ranked by production_dependents: orphan_q (2) first, then p, r (0) by name.
        assert_eq!(r.gaps[0].symbol_name, "orphan_q");
        assert_eq!(r.gaps[0].production_dependents, 2);
        assert_eq!(names, vec!["orphan_q", "orphan_p", "orphan_r"]);
    }

    /// A Rust inline unit test on a PRODUCTION path (`is_test_symbol = 1`, not a
    /// test file) seeds coverage: the production symbol it calls is covered, not
    /// a gap, and the inline test itself is never a production candidate. This is
    /// the follow-up "B" fix — without the symbol flag, `inline_test` would read
    /// as production and `prod_target` as an untested gap.
    #[tokio::test]
    async fn inline_test_symbol_seeds_coverage() {
        let p = pool().await;
        // Inline test lives in a production .rs file (not `*_test.rs`, no tests/).
        inline_test_node(&p, "it", "detects_cycles", "graph/algorithms/cycles.rs").await;
        // Production symbol the inline test exercises, same production file.
        node(&p, "pt", "detect_cycles", "function", "graph/algorithms/cycles.rs").await;
        // An unrelated, genuinely untested production symbol.
        node(&p, "orph", "orphan", "function", "graph/other.rs").await;
        edge(&p, "it", "pt").await;

        let r = detect_test_gaps(&p, T, None, 0).await.unwrap();

        // Candidates: detect_cycles + orphan (the inline test is NOT a candidate).
        assert_eq!(r.total_production, 2, "inline test excluded from candidates");
        assert_eq!(r.covered, 1, "detect_cycles reached from the inline test");
        assert_eq!(r.gap_count, 1);
        let names: Vec<&str> = r.gaps.iter().map(|g| g.symbol_name.as_str()).collect();
        assert_eq!(names, vec!["orphan"], "only the truly untested symbol is a gap");
        assert!(!names.contains(&"detect_cycles"), "inline-tested symbol is covered");
        assert!(!names.contains(&"detects_cycles"), "the inline test itself is not a gap");
    }

    /// `top_k` truncates the returned list but not the true `gap_count`.
    #[tokio::test]
    async fn top_k_truncates_but_keeps_true_count() {
        let p = pool().await;
        node(&p, "a", "a", "function", "a.rs").await;
        node(&p, "b", "b", "function", "b.rs").await;
        node(&p, "c", "c", "function", "c.rs").await;
        let r = detect_test_gaps(&p, T, None, 2).await.unwrap();
        assert_eq!(r.total_production, 3);
        assert_eq!(r.covered, 0, "no test files → nothing covered");
        assert_eq!(r.gap_count, 3, "true total survives truncation");
        assert_eq!(r.gaps.len(), 2, "returned list capped at top_k");
    }

    /// Empty graph is a clean zero, not an error.
    #[tokio::test]
    async fn empty_graph_is_zero() {
        let p = pool().await;
        let r = detect_test_gaps(&p, T, None, 0).await.unwrap();
        assert_eq!(r.total_production, 0);
        assert_eq!(r.gap_count, 0);
        assert!(r.gaps.is_empty());
    }
}
