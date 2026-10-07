use super::*;
use sqlx::sqlite::SqlitePoolOptions;

// The node filter lives in the parent module (shared with centrality), but the
// regression it guards is this module's measurement, so the test lives here.
use super::super::node_is_filtered_out;

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
                signature TEXT, language TEXT, parent_symbol TEXT,
                is_test_symbol INTEGER NOT NULL DEFAULT 0, generation TEXT NOT NULL DEFAULT '',
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
                weight REAL DEFAULT 1.0, metadata_json TEXT, generation TEXT NOT NULL DEFAULT '',
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
    weak_edge(pool, src, tgt, 1.0).await;
}

/// A CALLS edge at `weight` — below 0.6 it is one of N same-name guesses.
async fn weak_edge(pool: &SqlitePool, src: &str, tgt: &str, weight: f64) {
    sqlx::query(
        "INSERT INTO graph_edges
                (edge_id, tenant_id, source_node_id, target_node_id, edge_type, source_file, weight)
             VALUES (?, ?, ?, ?, 'CALLS', 's.rs', ?)",
    )
    .bind(format!("{src}->{tgt}"))
    .bind(T)
    .bind(src)
    .bind(tgt)
    .bind(weight)
    .execute(pool)
    .await
    .unwrap();
}

/// A method `name` of class `parent`.
async fn member(pool: &SqlitePool, id: &str, name: &str, parent: &str, file_path: &str) {
    sqlx::query(
        "INSERT INTO graph_nodes
                (node_id, tenant_id, symbol_name, symbol_type, file_path, parent_symbol)
             VALUES (?, ?, ?, 'method', ?, ?)",
    )
    .bind(id)
    .bind(T)
    .bind(name)
    .bind(file_path)
    .bind(parent)
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

    let r = detect_test_gaps(&p, T, &GraphScope::all(), None, 0)
        .await
        .unwrap();

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

    let r = detect_test_gaps(&p, T, &GraphScope::all(), None, 0)
        .await
        .unwrap();

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
    let r = detect_test_gaps(&p, T, &GraphScope::all(), None, 2)
        .await
        .unwrap();
    assert_eq!(r.total_production, 3);
    assert_eq!(r.covered, 0, "no test files → nothing covered");
    assert_eq!(r.gap_count, 3, "true total survives truncation");
    assert_eq!(r.gaps.len(), 2, "returned list capped at top_k");
}

/// Empty graph is a clean zero, not an error.
#[tokio::test]
async fn empty_graph_is_zero() {
    let p = pool().await;
    let r = detect_test_gaps(&p, T, &GraphScope::all(), None, 0)
        .await
        .unwrap();
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

    let r = detect_test_gaps(&p, T, &GraphScope::all(), None, 0)
        .await
        .unwrap();

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

    let r = detect_test_gaps(&p, T, &GraphScope::all(), None, 0)
        .await
        .unwrap();

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

    let r = detect_test_gaps(&p, T, &GraphScope::all(), None, 0)
        .await
        .unwrap();

    assert_eq!(r.covered, 3, "30% — well above the implausibility floor");
    assert!(r.reliability_warning.is_none());
}

/// A test whose call matched several same-named definitions reaches its
/// subject only through a sub-0.6 edge. The walk must not follow it (that
/// would fabricate coverage), but the gap must say a test calls it. Finance
/// 2026-10-07: `FirestoreFinanceBatch.set` read as untested with 47
/// production dependents while a test called it, with nothing to tell
/// "untested" from "tested through a call the resolver could not pin".
#[tokio::test]
async fn an_ambiguous_test_call_is_reported_not_counted_as_coverage() {
    let p = pool().await;
    node(&p, "tm", "test_batch", "function", "batch_test.rs").await;
    member(&p, "bs", "set", "Batch", "writes.rs").await;
    member(&p, "ts", "set", "Transaction", "writes.rs").await;
    node(&p, "repo", "transfer_item", "function", "repo.rs").await;
    // The test's `set` call could be either class's: 1/2 each.
    weak_edge(&p, "tm", "bs", 0.5).await;
    weak_edge(&p, "tm", "ts", 0.5).await;
    edge(&p, "repo", "bs").await;

    let r = detect_test_gaps(&p, T, &GraphScope::all(), None, 0)
        .await
        .unwrap();

    assert_eq!(r.covered, 0, "an ambiguous call is not coverage");
    assert_eq!(r.gap_count, 3);
    assert_eq!(r.gaps_with_ambiguous_test_callers, 2);
    let batch = r.gaps.iter().find(|g| g.node_id == "bs").unwrap();
    assert_eq!(batch.parent_symbol.as_deref(), Some("Batch"));
    assert_eq!(batch.ambiguous_test_callers, 1);
    assert_eq!(batch.production_dependents, 1);
    let repo = r.gaps.iter().find(|g| g.node_id == "repo").unwrap();
    assert_eq!(repo.ambiguous_test_callers, 0);
    assert_eq!(
        repo.parent_symbol, None,
        "a top-level function has no class"
    );
}

// ── #370: the ranking exclude must not eat this measurement's seeds ──
//
// `WQM_GRAPH_EXCLUDE` legitimately lists test paths so tests do not inflate
// hotspots/bridges. The same list reached this module through the shared
// adjacency loader, deleting every test node while leaving the production
// denominator whole. Measured in this repo: TypeScript reported 0 of 1396
// symbols covered with 1634 tests passing, and Rust looked fine only because
// its tests are inline `#[cfg(test)]` in production files, which no pattern
// matches. The numbers were internally consistent, so nothing failed.

/// The reference `docker/.env` list, verbatim enough to be representative.
fn reference_exclude() -> Vec<String> {
    [
        "old_project/",
        "/generated/",
        "/tests/",
        "_test.rs",
        ".test.ts",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect()
}

#[test]
fn test_seeds_survive_the_ranking_exclude() {
    let ex = reference_exclude();

    for path in [
        "src/typescript/mcp-server/tests/tools/graph.test.ts",
        "src/rust/daemon/core/tests/graph_store_tests.rs",
    ] {
        assert!(
            !node_is_filtered_out(path, &ex, true),
            "{path} is a SEED of the test-gap measurement and must survive"
        );
        assert!(
            node_is_filtered_out(path, &ex, false),
            "{path} must still be dropped for centrality ranking"
        );
    }
}

#[test]
fn scope_excludes_still_apply_to_non_test_files() {
    let ex = reference_exclude();

    // Keeping the seeds must not turn the scope list off wholesale: a
    // legacy/generated production file stays excluded in BOTH modes.
    for path in ["old_project/src/legacy.ts", "src/generated/api.pb.ts"] {
        assert!(
            node_is_filtered_out(path, &ex, true),
            "{path} is not a test file — the scope exclude still applies"
        );
    }

    // And with no configured patterns nothing is filtered either way.
    assert!(!node_is_filtered_out("src/a.ts", &[], false));
}

/// Tooling is not the product. Segment-matched, never substring — a
/// `transcripts/` directory must survive.
#[test]
fn tooling_paths_are_excluded_by_segment() {
    assert!(is_non_production_path(
        "doc-frontend/scripts/check_a11y.dart"
    ));
    assert!(is_non_production_path("scripts/build.ts"));
    assert!(!is_non_production_path("src/transcripts/parser.rs"));
    assert!(!is_non_production_path("src/scriptsupport/loader.rs"));
    // `tools/` is production in this very repo (tools/registry-updater).
    assert!(!is_non_production_path(
        "src/rust/tools/registry-updater/main.rs"
    ));
}

#[test]
fn language_key_reads_the_extension() {
    assert_eq!(language_key("a/b/c.dart").as_deref(), Some(".dart"));
    assert_eq!(language_key("a/b/C.JAVA").as_deref(), Some(".java"));
    assert_eq!(language_key("Makefile"), None);
    assert_eq!(
        language_key("a/.gitignore"),
        None,
        "a dotfile is not an extension"
    );
}
