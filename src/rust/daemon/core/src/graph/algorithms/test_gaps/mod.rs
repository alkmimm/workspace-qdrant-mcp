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
//! [`reliability::IMPLAUSIBLE_COVERAGE_RATIO`]. Reporting "0.6% covered" as a
//! finding is a worse failure than reporting nothing at all.
//!
//! **Ambiguous test calls.** A test whose call matched several same-named
//! definitions reaches its subject only through a sub-0.6 edge the walk does
//! not follow. Each gap reports how many tests call it that way
//! (`ambiguous_test_callers`) and the report counts such gaps, so "untested"
//! and "tested through a call the resolver could not pin" stay apart.

mod reach;
mod reliability;

use std::collections::{BTreeMap, HashSet};
use std::path::Path;

use serde::{Deserialize, Serialize};
use sqlx::SqlitePool;

use crate::graph::GraphScope;
use tracing::info;

use super::{load_adjacency_graph, AdjacencyGraph, GenericityFilter};
use crate::file_classification::is_test_file;
use reliability::build_reliability_warning;

/// A production definition that no test reaches over the call graph.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TestGap {
    pub node_id: String,
    pub symbol_name: String,
    /// The class the symbol is a member of, when it is one.
    #[serde(default)]
    pub parent_symbol: Option<String>,
    pub symbol_type: String,
    pub file_path: String,
    /// How many PRODUCTION nodes depend on this symbol (incoming edges whose
    /// source is a non-test node). High = important untested code: many callers
    /// rely on something no test exercises. Drives the ranking.
    pub production_dependents: u32,
    /// Test nodes that call this symbol only through an ambiguous same-name
    /// edge (below the 0.6 gate). Non-zero = possibly tested; check before
    /// writing a test for it.
    #[serde(default)]
    pub ambiguous_test_callers: u32,
}

/// Coverage-by-reachability summary + ranked gaps for a tenant.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
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
    /// Of `gap_count`, the gaps a test calls only through an ambiguous
    /// same-name edge (see [`TestGap::ambiguous_test_callers`]).
    #[serde(default)]
    pub gaps_with_ambiguous_test_callers: u32,
}

/// Production candidates split into covered symbols and gaps.
#[derive(Default)]
struct Census {
    total_production: u32,
    covered: u32,
    excluded_non_production: u32,
    gaps: Vec<TestGap>,
    /// (production, covered) per language, keyed by extension. BTreeMap so the
    /// pre-sort order is deterministic for equal counts.
    per_language: BTreeMap<String, (u32, u32)>,
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
    scope: &GraphScope,
    edge_types: Option<&[&str]>,
    top_k: usize,
) -> Result<TestGapsReport, sqlx::Error> {
    let types = edge_types.unwrap_or(DEFAULT_TEST_GAP_EDGE_TYPES);
    // apply_genericity_filters = false: keep the raw resolved graph — a heavily
    // used production symbol must still be judged tested-or-not, not filtered
    // away for being "generic". The loader already drops stub nodes (empty
    // file_path), sub-0.6 ambiguous edges, and WQM_GRAPH_EXCLUDE paths, so
    // generated/legacy trees are out of the coverage picture too.
    // `keep_test_nodes: true` — test files ARE the seeds of this measurement, so
    // the ranking-oriented path exclude must not delete them (#370).
    let graph = load_adjacency_graph(
        pool,
        tenant_id,
        scope,
        Some(types),
        GenericityFilter::None,
        true,
    )
    .await?;
    if graph.nodes.is_empty() {
        return Ok(TestGapsReport::default());
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
    let reached = reach::reached_from_tests(&graph, &test_set);
    let mut census = census(&graph, &test_set, &reached);
    let gap_count = census.gaps.len() as u32;
    let gaps_with_ambiguous_test_callers = rank_gaps(
        pool,
        tenant_id,
        scope,
        types,
        &test_set,
        &mut census.gaps,
        top_k,
    )
    .await?;

    let test_nodes = test_set.len() as u32;
    let coverage_by_language = language_rows(&graph, &test_set, census.per_language);
    let reliability_warning = build_reliability_warning(
        census.total_production,
        census.covered,
        test_nodes,
        &coverage_by_language,
    );

    info!(
        "GraphService test-gaps: tenant={} production={} covered={} gaps={} test_nodes={} excluded_tooling={} ambiguous={} unreliable={}",
        tenant_id,
        census.total_production,
        census.covered,
        gap_count,
        test_nodes,
        census.excluded_non_production,
        gaps_with_ambiguous_test_callers,
        reliability_warning.is_some()
    );

    Ok(TestGapsReport {
        total_production: census.total_production,
        covered: census.covered,
        gaps: census.gaps,
        gap_count,
        test_nodes,
        reliability_warning,
        excluded_non_production: census.excluded_non_production,
        coverage_by_language,
        gaps_with_ambiguous_test_callers,
    })
}

/// Mark each gap with its ambiguous test callers, rank the gaps (most
/// depended-upon first; deterministic tie-break by name then node_id), cap
/// them at `top_k` (0 = all) and name the class of the ones kept. Returns how
/// many gaps — of all of them, not only the kept ones — a test calls only
/// ambiguously.
async fn rank_gaps(
    pool: &SqlitePool,
    tenant_id: &str,
    scope: &GraphScope,
    edge_types: &[&str],
    test_set: &HashSet<&str>,
    gaps: &mut Vec<TestGap>,
    top_k: usize,
) -> Result<u32, sqlx::Error> {
    let test_ids: Vec<&str> = test_set.iter().copied().collect();
    let gap_ids: HashSet<&str> = gaps.iter().map(|g| g.node_id.as_str()).collect();
    let ambiguous =
        reach::ambiguous_test_callers(pool, tenant_id, scope, edge_types, &test_ids, &gap_ids)
            .await?;
    for gap in gaps.iter_mut() {
        gap.ambiguous_test_callers = ambiguous.get(&gap.node_id).copied().unwrap_or(0);
    }
    gaps.sort_by(|a, b| {
        b.production_dependents
            .cmp(&a.production_dependents)
            .then_with(|| a.symbol_name.cmp(&b.symbol_name))
            .then_with(|| a.node_id.cmp(&b.node_id))
    });
    if top_k > 0 && top_k < gaps.len() {
        gaps.truncate(top_k);
    }
    let shown: Vec<String> = gaps.iter().map(|g| g.node_id.clone()).collect();
    let parents = reach::parent_symbols(pool, tenant_id, &shown).await?;
    for gap in gaps.iter_mut() {
        gap.parent_symbol = parents.get(&gap.node_id).cloned();
    }
    Ok(ambiguous.len() as u32)
}

/// Production candidates = testable-typed nodes on non-test files. A candidate
/// not in `reached` is a gap, ranked later by how many PRODUCTION nodes call it.
fn census(graph: &AdjacencyGraph, test_set: &HashSet<&str>, reached: &HashSet<&str>) -> Census {
    let mut census = Census::default();
    for (id, info) in &graph.nodes {
        if test_set.contains(id.as_str()) || !is_testable_symbol_type(&info.symbol_type) {
            continue;
        }
        // Tooling is not the product: excluded from the denominator, and
        // counted so the caller can see the filter acted.
        if is_non_production_path(&info.file_path) {
            census.excluded_non_production += 1;
            continue;
        }
        census.total_production += 1;
        let language = language_key(&info.file_path);
        if let Some(ext) = language.clone() {
            census.per_language.entry(ext).or_insert((0, 0)).0 += 1;
        }
        if reached.contains(id.as_str()) {
            census.covered += 1;
            if let Some(ext) = language {
                census.per_language.entry(ext).or_insert((0, 0)).1 += 1;
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
        census.gaps.push(TestGap {
            node_id: id.clone(),
            symbol_name: info.symbol_name.clone(),
            parent_symbol: None,
            symbol_type: info.symbol_type.clone(),
            file_path: info.file_path.clone(),
            production_dependents,
            ambiguous_test_callers: 0,
        });
    }
    census
}

/// Coverage per language, largest language first: the caller reads the top
/// rows, and a stack with few symbols cannot say much about the report either
/// way. Each row carries its test symbols, so a blind language can be told
/// apart from one that simply has no tests.
fn language_rows(
    graph: &AdjacencyGraph,
    test_set: &HashSet<&str>,
    per_language: BTreeMap<String, (u32, u32)>,
) -> Vec<LanguageCoverage> {
    let mut test_nodes_per_language: BTreeMap<String, u32> = BTreeMap::new();
    for id in test_set {
        if let Some(ext) = graph
            .nodes
            .get(*id)
            .and_then(|info| language_key(&info.file_path))
        {
            *test_nodes_per_language.entry(ext).or_insert(0) += 1;
        }
    }
    let mut rows: Vec<LanguageCoverage> = per_language
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
    rows.sort_by(|a, b| {
        b.production
            .cmp(&a.production)
            .then_with(|| a.extension.cmp(&b.extension))
    });
    rows
}

#[cfg(test)]
mod tests;
