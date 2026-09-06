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
//!
//! **Reliability guard.** Because coverage here depends entirely on edges the
//! extractor managed to resolve, a repo whose tests reach their subjects
//! indirectly (DI container, path-aliased imports, dynamic dispatch) yields a
//! near-zero ratio that says nothing about its tests. The report therefore
//! carries `test_nodes` and, when tests exist but reach almost nothing, a
//! `reliability_warning` telling the caller to disregard the ranking — see
//! [`IMPLAUSIBLE_COVERAGE_RATIO`]. Reporting "0.6% covered" as a finding is a
//! worse failure than reporting nothing at all.

use std::collections::{BTreeMap, HashSet, VecDeque};
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
    /// Graph nodes classified as test — the seeds of the reachability walk.
    /// Reported so a caller can tell "this repo has no tests" (an honest 0%)
    /// apart from "this repo's tests produced no resolvable edges" (a broken
    /// measurement); see [`reliability_warning`](Self::reliability_warning).
    pub test_nodes: u32,
    /// Set when the measurement is not trustworthy: tests exist in the graph
    /// yet reach almost nothing, which means the test→production edges failed
    /// to resolve rather than that the code is untested. `None` when the
    /// coverage figure is plausible (or when there is genuinely no test code).
    pub reliability_warning: Option<String>,
    /// Candidates dropped from `total_production` as TOOLING (see
    /// [`NON_PRODUCTION_PATH_SEGMENTS`]). Reported rather than silently
    /// subtracted: a caller comparing two runs must be able to see that the
    /// denominator moved because of the filter, not because of the code.
    pub excluded_non_production: u32,
    /// Coverage split by file extension, largest language first.
    ///
    /// The single most useful number for judging whether a report is REAL: a
    /// global ratio hides a per-language extraction failure completely. Measured
    /// 2026-09-06 — DOC-V2 (whose top-25 was full of demonstrably tested Flutter
    /// primitives) reported 27.7% overall, and this repo's own healthy graph
    /// reports 28.3%. The global figure carried no signal at all; a per-language
    /// split does.
    pub coverage_by_language: Vec<LanguageCoverage>,
}

/// Per-language slice of the coverage summary.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LanguageCoverage {
    /// Lowercased file extension including the dot, e.g. `.dart`.
    pub extension: String,
    pub production: u32,
    pub covered: u32,
    /// Test symbols written in this language — the seeds available to cover it.
    /// A language with many test symbols and near-zero coverage is an extractor
    /// blind spot, not an untested module.
    pub test_nodes: u32,
}

/// Edge types that mean "exercised by": a test that CALLS production code, or
/// USES_TYPE of a production type, exercises it. IMPORTS is deliberately absent —
/// importing a symbol is not testing it.
const DEFAULT_TEST_GAP_EDGE_TYPES: &[&str] = &["CALLS", "USES_TYPE"];

/// Coverage ratio below which a report that HAS test nodes is treated as a
/// failed measurement rather than as a finding.
///
/// Field feedback (v0-bws-training audit, 2026-08): a Next.js/TS repo with 183
/// Jest test files reported 21 of 3554 symbols covered (0.6%) and ranked
/// demonstrably-tested functions as the top gaps. The tests resolve their
/// subjects through a DI container and import via an `@/` path alias, so no
/// `CALLS` edge test→production was ever extracted. At that ratio the ranking
/// is noise, and presenting it as a finding is worse than presenting nothing.
///
/// 5% is deliberately far below any real-world floor: this repo's own graph
/// measures ~28%, and even a lightly-tested codebase clears 5% once its test
/// edges resolve at all. Anything under it indicates the extractor, not the
/// test suite.
const IMPLAUSIBLE_COVERAGE_RATIO: f64 = 0.05;

/// Build the reliability caveat for a finished report, or `None` when the
/// numbers are trustworthy.
///
/// Deliberately silent when `test_nodes == 0`: a project with no test code
/// genuinely has 0% coverage, and that is a finding, not a malfunction. The
/// warning fires only on the contradiction — tests are present in the graph,
/// yet they reach almost no production symbol.
fn build_reliability_warning(
    total_production: u32,
    covered: u32,
    test_nodes: u32,
    by_language: &[LanguageCoverage],
) -> Option<String> {
    if test_nodes == 0 || total_production == 0 {
        return None;
    }
    let ratio = f64::from(covered) / f64::from(total_production);
    if ratio >= IMPLAUSIBLE_COVERAGE_RATIO {
        // The GLOBAL figure is plausible — but an average hides a language whose
        // edges did not resolve at all. Apply the SAME already-calibrated floor
        // per language rather than inventing a second threshold: "a real test
        // suite does not measure this low" is exactly as true of one language as
        // of a whole repo, and a polyglot repo can look healthy overall while one
        // stack is entirely unmeasured. Requires the language to actually have
        // test symbols, so an untested module stays an honest finding.
        return language_reliability_warning(by_language, ratio);
    }
    Some(format!(
        "UNRELIABLE: {test_nodes} test symbols are indexed, yet only {covered} of \
         {total_production} production symbols ({:.1}%) are reachable from them. A real test \
         suite does not measure this low — the test->production edges almost certainly failed \
         to resolve. Common causes: subjects wired through a DI container, imports via a path \
         alias the extractor does not follow, dynamic dispatch, or mocking that replaces the \
         call entirely. Treat the gap ranking below as noise, not as a list of untested code, \
         and confirm with a real coverage tool.",
        ratio * 100.0
    ))
}

/// Minimum production symbols before a language's ratio is worth judging — below
/// this a handful of unresolved edges swings the percentage wildly.
const LANGUAGE_MIN_PRODUCTION: u32 = 50;

/// Warn when ONE language measures below the implausibility floor while the repo
/// as a whole looks fine. That is the shape a global ratio cannot express: the
/// gaps it produces are concentrated in the blind language, so they dominate the
/// ranking an agent reads, while the healthy languages keep the average up.
fn language_reliability_warning(
    by_language: &[LanguageCoverage],
    overall_ratio: f64,
) -> Option<String> {
    let blind: Vec<&LanguageCoverage> = by_language
        .iter()
        .filter(|lang| {
            lang.production >= LANGUAGE_MIN_PRODUCTION
                && lang.test_nodes > 0
                && f64::from(lang.covered) / f64::from(lang.production) < IMPLAUSIBLE_COVERAGE_RATIO
        })
        .collect();
    if blind.is_empty() {
        return None;
    }
    let detail = blind
        .iter()
        .map(|lang| {
            format!(
                "{} ({} of {} production symbols covered, {} test symbols indexed)",
                lang.extension,
                lang.covered,
                lang.production,
                lang.test_nodes
            )
        })
        .collect::<Vec<_>>()
        .join("; ");
    Some(format!(
        "PARTIALLY UNRELIABLE: overall coverage is {:.1}%, which looks plausible, but these \
         languages measure below {:.0}% while HAVING indexed tests: {detail}. A real test suite \
         does not measure this low — for those languages the test->production edges almost \
         certainly failed to resolve, so their symbols are over-represented in the ranking below. \
         Known blind spot: an idiom that REFERENCES a symbol without invoking it produces no edge \
         (e.g. Flutter's `find.byType(Widget)` asserts on a type without constructing it), so the \
         most-asserted primitives can rank as the most critical gaps. Judge each language on its \
         own row in coverage_by_language, and confirm with a real coverage tool.",
        overall_ratio * 100.0,
        IMPLAUSIBLE_COVERAGE_RATIO * 100.0
    ))
}

/// Path segments whose code is TOOLING, not the product: automation that ships
/// in no artifact and is exercised by running it, not by unit tests. Counting it
/// as production inflates the denominator and — because these scripts are often
/// self-contained and widely self-referential — pushes their symbols high into
/// the gap ranking. DOC-V2 (2026-09-06) had `doc-frontend/scripts/` guardrail
/// checkers at positions 7 and 24 of the top-25.
///
/// `tools/` is deliberately NOT here: this very repo ships
/// `src/rust/tools/registry-updater` as a real component. The bar for adding a
/// segment is that its content cannot plausibly be product code.
const NON_PRODUCTION_PATH_SEGMENTS: &[&str] = &["scripts"];

/// Is this path tooling rather than product code? Matched on whole path
/// SEGMENTS (never substrings) so `src/transcripts/` is untouched — the
/// substring-vs-segment mistake that once made an ignore token swallow whole
/// directories.
fn is_non_production_path(file_path: &str) -> bool {
    file_path
        .split(['/', '\\'])
        .any(|segment| NON_PRODUCTION_PATH_SEGMENTS.contains(&segment))
}

/// The file extension used to group coverage by language, e.g. `.dart`.
/// Returns `None` for an extensionless path.
fn language_key(file_path: &str) -> Option<String> {
    let name = file_path.rsplit(['/', '\\']).next()?;
    let dot = name.rfind('.')?;
    if dot == 0 {
        return None; // a dotfile, not an extension
    }
    Some(name[dot..].to_lowercase())
}

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
            test_nodes: 0,
            reliability_warning: None,
            excluded_non_production: 0,
            coverage_by_language: Vec::new(),
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
    let mut excluded_non_production = 0u32;
    let mut gaps: Vec<TestGap> = Vec::new();
    // (production, covered) per language, keyed by extension. BTreeMap so the
    // pre-sort order is deterministic for equal counts.
    let mut per_language: BTreeMap<String, (u32, u32)> = BTreeMap::new();
    // Test symbols per language, so a blind language can be told apart from one
    // that simply has no tests (see `language_reliability_warning`).
    let mut test_nodes_per_language: BTreeMap<String, u32> = BTreeMap::new();
    for id in &test_set {
        if let Some(info) = graph.nodes.get(*id) {
            if let Some(ext) = language_key(&info.file_path) {
                *test_nodes_per_language.entry(ext).or_insert(0) += 1;
            }
        }
    }
    for (id, info) in &graph.nodes {
        if test_set.contains(id.as_str()) || !is_testable_symbol_type(&info.symbol_type) {
            continue;
        }
        // Tooling is not the product: excluded from the denominator, and
        // counted so the caller can see the filter acted.
        if is_non_production_path(&info.file_path) {
            excluded_non_production += 1;
            continue;
        }
        total_production += 1;
        let language = language_key(&info.file_path);
        if let Some(ext) = language.clone() {
            per_language.entry(ext).or_insert((0, 0)).0 += 1;
        }
        if reached.contains(id.as_str()) {
            covered += 1;
            if let Some(ext) = language {
                per_language.entry(ext).or_insert((0, 0)).1 += 1;
            }
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

    let test_nodes = test_set.len() as u32;

    // Largest language first: the caller reads the top rows, and a stack with
    // few symbols cannot say much about the report either way.
    let mut coverage_by_language: Vec<LanguageCoverage> = per_language
        .into_iter()
        .map(|(extension, (production, covered))| LanguageCoverage {
            test_nodes: test_nodes_per_language
                .get(&extension)
                .copied()
                .unwrap_or(0),
            extension,
            production,
            covered,
        })
        .collect();
    coverage_by_language.sort_by(|a, b| {
        b.production
            .cmp(&a.production)
            .then_with(|| a.extension.cmp(&b.extension))
    });

    let reliability_warning =
        build_reliability_warning(total_production, covered, test_nodes, &coverage_by_language);

    info!(
        "GraphService test-gaps: tenant={} production={} covered={} gaps={} test_nodes={} excluded_tooling={} unreliable={}",
        tenant_id,
        total_production,
        covered,
        gap_count,
        test_nodes,
        excluded_non_production,
        reliability_warning.is_some()
    );

    Ok(TestGapsReport {
        total_production,
        covered,
        gaps,
        gap_count,
        test_nodes,
        reliability_warning,
        excluded_non_production,
        coverage_by_language,
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
        assert_eq!(
            r.total_production, 5,
            "constant MAX excluded from candidates"
        );
        // handler + service reached transitively from test_main.
        assert_eq!(r.covered, 2);
        assert_eq!(r.gap_count, 3);
        let names: Vec<&str> = r.gaps.iter().map(|g| g.symbol_name.as_str()).collect();
        assert!(
            !names.contains(&"handler") && !names.contains(&"service"),
            "covered not gaps"
        );
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
        node(
            &p,
            "pt",
            "detect_cycles",
            "function",
            "graph/algorithms/cycles.rs",
        )
        .await;
        // An unrelated, genuinely untested production symbol.
        node(&p, "orph", "orphan", "function", "graph/other.rs").await;
        edge(&p, "it", "pt").await;

        let r = detect_test_gaps(&p, T, None, 0).await.unwrap();

        // Candidates: detect_cycles + orphan (the inline test is NOT a candidate).
        assert_eq!(
            r.total_production, 2,
            "inline test excluded from candidates"
        );
        assert_eq!(r.covered, 1, "detect_cycles reached from the inline test");
        assert_eq!(r.gap_count, 1);
        let names: Vec<&str> = r.gaps.iter().map(|g| g.symbol_name.as_str()).collect();
        assert_eq!(
            names,
            vec!["orphan"],
            "only the truly untested symbol is a gap"
        );
        assert!(
            !names.contains(&"detect_cycles"),
            "inline-tested symbol is covered"
        );
        assert!(
            !names.contains(&"detects_cycles"),
            "the inline test itself is not a gap"
        );
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
        assert_eq!(r.test_nodes, 0);
        assert!(r.reliability_warning.is_none(), "nothing to warn about");
    }

    /// The v0-bws-training shape: tests ARE indexed, but their subjects resolve
    /// through a DI container / path alias, so almost no test→production edge
    /// was extracted. The ratio is then a measurement failure, not a finding —
    /// the report must say so instead of letting the ranking read as truth.
    #[tokio::test]
    async fn warns_when_indexed_tests_reach_almost_nothing() {
        let p = pool().await;
        node(&p, "tm", "renders_the_page", "function", "page.test.ts").await;
        for i in 0..25 {
            node(
                &p,
                &format!("n{i}"),
                &format!("prod{i}"),
                "function",
                "app/prod.ts",
            )
            .await;
        }
        // The single edge the extractor did manage to resolve: 1/25 = 4%.
        edge(&p, "tm", "n0").await;

        let r = detect_test_gaps(&p, T, None, 0).await.unwrap();

        assert_eq!(r.total_production, 25);
        assert_eq!(r.covered, 1);
        assert_eq!(r.test_nodes, 1, "the test file's symbol seeded the walk");
        let warning = r
            .reliability_warning
            .expect("4% with indexed tests must be flagged as unreliable");
        assert!(
            warning.contains("UNRELIABLE"),
            "leads with the verdict: {warning}"
        );
        assert!(
            warning.contains("4.0%"),
            "states the measured ratio: {warning}"
        );
        // Still returns the gaps — the caller is told to distrust them, not
        // denied the data (a coverage tool may still want the raw list).
        assert_eq!(r.gap_count, 24);
    }

    /// A project with genuinely NO test code is 0% covered and that is a real
    /// finding — the guard must stay silent rather than blaming the extractor.
    #[tokio::test]
    async fn no_warning_when_project_has_no_tests() {
        let p = pool().await;
        for i in 0..25 {
            node(
                &p,
                &format!("n{i}"),
                &format!("prod{i}"),
                "function",
                "app/prod.ts",
            )
            .await;
        }

        let r = detect_test_gaps(&p, T, None, 0).await.unwrap();

        assert_eq!(r.covered, 0);
        assert_eq!(r.test_nodes, 0);
        assert!(
            r.reliability_warning.is_none(),
            "0% with no test code is honest, not a malfunction"
        );
    }

    /// Above the threshold the report is trusted and ships no caveat.
    #[tokio::test]
    async fn no_warning_when_coverage_is_plausible() {
        let p = pool().await;
        node(&p, "tm", "test_main", "function", "main_test.rs").await;
        for i in 0..10 {
            node(
                &p,
                &format!("n{i}"),
                &format!("prod{i}"),
                "function",
                "prod.rs",
            )
            .await;
        }
        for i in 0..3 {
            edge(&p, "tm", &format!("n{i}")).await;
        }

        let r = detect_test_gaps(&p, T, None, 0).await.unwrap();

        assert_eq!(r.covered, 3, "30% — well above the implausibility floor");
        assert!(r.reliability_warning.is_none());
    }

    /// The threshold is a floor on the RATIO, not on the absolute count: the
    /// boundary is inclusive, so exactly 5% is still trusted.
    #[test]
    fn threshold_boundary_is_inclusive() {
        assert!(
            build_reliability_warning(100, 5, 1, &[]).is_none(),
            "5% passes"
        );
        assert!(
            build_reliability_warning(100, 4, 1, &[]).is_some(),
            "4% is flagged"
        );
        assert!(
            build_reliability_warning(0, 0, 7, &[]).is_none(),
            "no production candidates → nothing to judge"
        );
    }

    fn lang(extension: &str, production: u32, covered: u32, test_nodes: u32) -> LanguageCoverage {
        LanguageCoverage {
            extension: extension.to_string(),
            production,
            covered,
            test_nodes,
        }
    }

    /// The defect this guard exists for: a polyglot repo whose GLOBAL ratio is
    /// perfectly normal while one stack resolved no test edges at all. Measured
    /// 2026-09-06 — DOC-V2 reported 27.7% overall with a top-25 full of
    /// demonstrably tested Flutter primitives, and this repo's healthy graph
    /// reports 28.3%. The global number cannot separate them; a per-language
    /// row can.
    #[test]
    fn a_blind_language_is_flagged_even_when_the_global_ratio_is_healthy() {
        let by_language = [
            lang(".java", 1000, 400, 500), // 40% — healthy, carries the average
            lang(".dart", 800, 8, 900),    // 1% with 900 test symbols — impossible
        ];
        let warning = build_reliability_warning(1800, 408, 1400, &by_language)
            .expect("a language below the floor must be flagged");
        assert!(warning.contains("PARTIALLY UNRELIABLE"));
        assert!(warning.contains(".dart"));
        assert!(
            !warning.contains(".java"),
            "the healthy language must not be named as suspect"
        );
    }

    /// A language with no tests at all measures 0% honestly — that is a finding,
    /// not a malfunction, exactly as for a whole repo with no test code.
    #[test]
    fn a_language_without_tests_is_not_flagged() {
        let by_language = [
            lang(".java", 1000, 400, 500),
            lang(".sql", 200, 0, 0), // no tests → an honest 0%
        ];
        assert!(build_reliability_warning(1200, 400, 500, &by_language).is_none());
    }

    /// Small languages swing wildly on a handful of unresolved edges, so they
    /// are not judged.
    #[test]
    fn a_tiny_language_is_not_judged() {
        let by_language = [
            lang(".java", 1000, 400, 500),
            lang(".lua", 10, 0, 3), // under LANGUAGE_MIN_PRODUCTION
        ];
        assert!(build_reliability_warning(1010, 400, 503, &by_language).is_none());
    }

    /// Tooling is not the product. Segment-matched, never substring — a
    /// `transcripts/` directory must survive.
    #[test]
    fn tooling_paths_are_excluded_by_segment() {
        assert!(is_non_production_path("doc-frontend/scripts/check_a11y.dart"));
        assert!(is_non_production_path("scripts/build.ts"));
        assert!(!is_non_production_path("src/transcripts/parser.rs"));
        assert!(!is_non_production_path("src/scriptsupport/loader.rs"));
        // `tools/` is production in this very repo (tools/registry-updater).
        assert!(!is_non_production_path("src/rust/tools/registry-updater/main.rs"));
    }

    #[test]
    fn language_key_reads_the_extension() {
        assert_eq!(language_key("a/b/c.dart").as_deref(), Some(".dart"));
        assert_eq!(language_key("a/b/C.JAVA").as_deref(), Some(".java"));
        assert_eq!(language_key("Makefile"), None);
        assert_eq!(language_key("a/.gitignore"), None, "a dotfile is not an extension");
    }
}
