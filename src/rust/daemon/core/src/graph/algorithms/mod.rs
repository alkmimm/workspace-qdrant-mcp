/// Graph algorithms: PageRank, community detection, betweenness centrality.
///
/// Implemented as pure functions over adjacency data loaded from any
/// `GraphStore` backend (SQLite or LadybugDB). The algorithms operate on
/// in-memory adjacency lists, so they work identically regardless of backend.
mod betweenness;
mod community;
mod cycles;
mod pagerank;

pub use betweenness::{compute_betweenness_centrality, BetweennessEntry};
pub use community::{detect_communities, Community, CommunityConfig, CommunityMember};
pub use cycles::{detect_cycles, Cycle, CycleMember};
pub use pagerank::{compute_pagerank, PageRankConfig, PageRankEntry};

use std::collections::{HashMap, HashSet};
use std::sync::OnceLock;

use sqlx::{Row, SqlitePool};
use tracing::debug;

// ─── Internal adjacency representation ─────────────────────────────────

/// Node metadata loaded from the graph.
#[derive(Debug, Clone)]
pub(super) struct NodeInfo {
    pub(super) symbol_name: String,
    pub(super) symbol_type: String,
    pub(super) file_path: String,
}

/// Adjacency list representation for algorithm execution.
#[derive(Debug)]
pub(super) struct AdjacencyGraph {
    /// node_id → metadata
    pub(super) nodes: HashMap<String, NodeInfo>,
    /// node_id → set of outgoing neighbor node_ids
    pub(super) outgoing: HashMap<String, Vec<String>>,
    /// node_id → set of incoming neighbor node_ids (reverse edges)
    pub(super) incoming: HashMap<String, Vec<String>>,
}

/// Path patterns that exclude a node from ALL graph analysis — centrality
/// (hotspots/bridges/modules) AND cycle detection — but NOT search/grep/
/// relations/impact, which never call this loader. Excluding a legacy/generated
/// tree (`old_project/`) is a SCOPE decision ("don't analyze this"), so it
/// applies to cycles too: a cycle living entirely in `old_project/` is noise the
/// same way it inflates hotspots. (Distinct from the genericity filters, which
/// are centrality-only precision-for-ranking.)
///
/// Set via `WQM_GRAPH_EXCLUDE` (comma-separated), unioned with the legacy
/// `WQM_GRAPH_CENTRALITY_EXCLUDE` name for back-compat. Persist a default by
/// putting it in `docker/.env`. Matching is PATH-SEGMENT / suffix aware (see
/// `is_graph_excluded`), not raw substring. Empty/unset = no exclusion. Parsed
/// once per process.
fn graph_exclude_patterns() -> &'static [String] {
    static PATTERNS: OnceLock<Vec<String>> = OnceLock::new();
    PATTERNS.get_or_init(|| {
        let mut seen = HashSet::new();
        let mut out = Vec::new();
        for name in ["WQM_GRAPH_EXCLUDE", "WQM_GRAPH_CENTRALITY_EXCLUDE"] {
            for tok in std::env::var(name).unwrap_or_default().split(',') {
                let t = tok.trim();
                if !t.is_empty() && seen.insert(t.to_string()) {
                    out.push(t.to_string());
                }
            }
        }
        out
    })
}

/// True if `file_path` matches any graph-exclude pattern, matched at PATH-SEGMENT
/// / filename-suffix boundaries rather than as a raw substring — the #294 lesson
/// (a bare `out` must not exclude `RouteDefinition.java`). Patterns here are
/// user-typed path fragments, a richer vocabulary than the exclusion engine's
/// bare `build_outputs` tokens (so `patterns::exclusion::segment_or_suffix_match`
/// does not fit — this handles slash-fragments and arbitrary filename suffixes):
///   - contains `/` (`old_project/`, `/test/`, `src/generated/`): the trimmed
///     segment sequence must appear as CONSECUTIVE path segments.
///   - a bare filename/suffix with `.` (`Test.java`, `.pb.dart`): the final
///     segment (the filename) must END WITH it.
///   - a bare token (`test`, `build`): some path segment must EQUAL it exactly
///     (so `test` matches a `/test/` dir but never `attestation.rs`).
/// Case-sensitive, matching the prior behaviour.
fn is_graph_excluded(file_path: &str, patterns: &[String]) -> bool {
    let segs: Vec<&str> = file_path.split(|c: char| c == '/' || c == '\\').collect();
    patterns.iter().any(|p| {
        if p.contains('/') {
            let pat: Vec<&str> = p
                .split('/')
                .filter(|s| !s.is_empty())
                .collect();
            !pat.is_empty() && segs.windows(pat.len()).any(|w| w == pat.as_slice())
        } else if p.contains('.') {
            segs.last().is_some_and(|last| last.ends_with(p.as_str()))
        } else {
            segs.iter().any(|s| s == p)
        }
    })
}

/// OPTIONAL manual override: symbol names to exclude from CENTRALITY regardless of
/// frequency, via the comma-separated `WQM_GRAPH_CENTRALITY_SKIP_SYMBOLS` env var.
/// There is deliberately NO built-in or per-language list — genericity is derived
/// DYNAMICALLY from definition frequency (see `centrality_generic_threshold`), so
/// nothing needs curating or updating per language (fits the dynamic language
/// registry). Empty/unset = none. Parsed once per process. (R3)
fn centrality_manual_skip_symbols() -> &'static HashSet<String> {
    static SYMBOLS: OnceLock<HashSet<String>> = OnceLock::new();
    SYMBOLS.get_or_init(|| {
        std::env::var("WQM_GRAPH_CENTRALITY_SKIP_SYMBOLS")
            .unwrap_or_default()
            .split(',')
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect()
    })
}

/// Definition-count threshold above which a symbol NAME is treated as generic and
/// dropped from centrality. Corpus-derived and LANGUAGE-AGNOSTIC: a name defined in
/// many places (toString/build/get — in ANY language) is central by ubiquity, not
/// importance — exactly aider/deprank's data-driven model, no curated list. The
/// default scales with corpus size (~0.2%, floored at 15) so it adapts to a small
/// lib vs a large monorepo; override with an absolute
/// `WQM_GRAPH_CENTRALITY_GENERIC_THRESHOLD` (0 disables the frequency filter). (R3)
fn centrality_generic_threshold(total_definitions: usize) -> usize {
    static OVERRIDE: OnceLock<Option<usize>> = OnceLock::new();
    let ov = OVERRIDE.get_or_init(|| {
        std::env::var("WQM_GRAPH_CENTRALITY_GENERIC_THRESHOLD")
            .ok()
            .and_then(|v| v.trim().parse::<usize>().ok())
    });
    match *ov {
        Some(0) => usize::MAX, // explicitly disabled
        Some(n) => n,
        None => std::cmp::max(15, total_definitions / 500),
    }
}

/// In-degree (call/use ubiquity) threshold above which a file-backed NODE is
/// dropped from centrality. Complements `centrality_generic_threshold`, which
/// only sees DEFINITION ubiquity (a name defined in many places). It is blind to
/// the dominant noise class: a name DEFINED ONCE but CALLED everywhere — a project
/// method/type whose bare name collides with a stdlib builtin (`collect`, `iter`,
/// `Result`, `send`), so the by-name stub resolver repoints every same-named
/// stdlib call onto that single node (tenant-unique tier, weight 0.7). Such a node
/// has implausibly high in-degree and is central by ubiquity, not importance
/// (aider/deprank model), burying the real hotspots and gluing unrelated modules
/// into one giant community.
///
/// Corpus-derived and LANGUAGE-AGNOSTIC, but with a CAP: the generic-name line is
/// roughly CONSTANT across projects (~115-125 in-degree), NOT proportional to
/// size — a bigger codebase just has MORE names above the line, not a higher line.
/// Calibrated on three tenants (real-domain peak ~111-113 in-degree everywhere;
/// generic floor ~118+): floor 50 (small libs), then `total/150` in the mid-range,
/// capped at 125 so a large monorepo (e.g. DOC-V2 at 40k defs) is not handed an
/// over-lenient 270 that lets `isBlank`/`collect`/`assertEquals` survive. Override
/// with `WQM_GRAPH_CENTRALITY_USAGE_THRESHOLD` (0 disables). (R3)
fn centrality_usage_threshold(total_definitions: usize) -> usize {
    static OVERRIDE: OnceLock<Option<usize>> = OnceLock::new();
    let ov = OVERRIDE.get_or_init(|| {
        std::env::var("WQM_GRAPH_CENTRALITY_USAGE_THRESHOLD")
            .ok()
            .and_then(|v| v.trim().parse::<usize>().ok())
    });
    match *ov {
        Some(0) => usize::MAX, // explicitly disabled
        Some(n) => n,
        None => std::cmp::max(50, std::cmp::min(total_definitions / 150, 125)),
    }
}

/// Load the full adjacency graph for a tenant from SQLite.
///
/// `apply_genericity_filters` gates the CENTRALITY-only precision filters
/// (definition/usage-ubiquity drops, manual symbol skip). Centrality callers pass
/// `true` (rank only resolved, non-generic nodes); structural callers like cycle
/// detection pass `false` — a real dependency cycle may pass through a
/// high-in-degree node, so those filters would hide it. The stub drop (empty
/// `file_path`), the `weight >= 0.6` confidence gate, AND the graph-scope
/// path-exclude (`WQM_GRAPH_EXCLUDE`) always apply — excluding a legacy/generated
/// tree is a scope decision, so cycles honour it too.
pub(super) async fn load_adjacency_graph(
    pool: &SqlitePool,
    tenant_id: &str,
    edge_types: Option<&[&str]>,
    apply_genericity_filters: bool,
) -> Result<AdjacencyGraph, sqlx::Error> {
    // Load nodes
    let node_rows = sqlx::query(
        "SELECT node_id, symbol_name, symbol_type, file_path
         FROM graph_nodes WHERE tenant_id = ?1",
    )
    .bind(tenant_id)
    .fetch_all(pool)
    .await?;

    // Pre-pass: count file-backed definitions per symbol name, for the dynamic
    // genericity filter below (stubs with empty file_path don't count).
    let mut def_count: HashMap<String, usize> = HashMap::new();
    for row in &node_rows {
        let fp: String = row.get("file_path");
        if !fp.is_empty() {
            *def_count.entry(row.get("symbol_name")).or_default() += 1;
        }
    }
    let total_defs: usize = def_count.values().copied().sum();
    let generic_threshold = centrality_generic_threshold(total_defs);
    let usage_threshold = centrality_usage_threshold(total_defs);
    let manual_skip = centrality_manual_skip_symbols();

    // Pre-pass (R3, usage axis): high-confidence in-degree per file-backed node,
    // for the call/use-ubiquity filter below. Mirrors the centrality edge load
    // exactly (weight >= 0.6 + the same optional edge_types), so a node's measured
    // in-degree matches the graph centrality will actually walk. Skipped entirely
    // when the filter is disabled (threshold = usize::MAX).
    let mut indeg_by_node: HashMap<String, usize> = HashMap::new();
    if apply_genericity_filters && usage_threshold != usize::MAX {
        let indeg_rows = if let Some(types) = edge_types {
            let placeholders: Vec<String> = types.iter().map(|t| format!("'{}'", t)).collect();
            let query = format!(
                "SELECT target_node_id, COUNT(*) AS indeg FROM graph_edges
                 WHERE tenant_id = ?1 AND weight >= 0.6 AND edge_type IN ({})
                 GROUP BY target_node_id",
                placeholders.join(", ")
            );
            sqlx::query(&query).bind(tenant_id).fetch_all(pool).await?
        } else {
            sqlx::query(
                "SELECT target_node_id, COUNT(*) AS indeg FROM graph_edges
                 WHERE tenant_id = ?1 AND weight >= 0.6
                 GROUP BY target_node_id",
            )
            .bind(tenant_id)
            .fetch_all(pool)
            .await?
        };
        for row in &indeg_rows {
            let nid: String = row.get("target_node_id");
            let indeg: i64 = row.get("indeg");
            indeg_by_node.insert(nid, indeg.max(0) as usize);
        }
    }

    let mut nodes = HashMap::with_capacity(node_rows.len());
    let exclude = graph_exclude_patterns();
    let mut excluded = 0usize;
    for row in &node_rows {
        let file_path: String = row.get("file_path");
        // Skip unresolved stub nodes. `GraphNode::stub` keys a node on its bare
        // symbol name with an EMPTY file_path, so every same-named symbol across
        // the tenant (stdlib `push`/`join`/`log`, a never-resolved import, a
        // builtin) collapses into ONE tenant-wide mega-node. Left in, those
        // dangling stubs dominate PageRank/community/betweenness and bury the
        // real, file-backed hotspots. Centrality should rank only resolved
        // nodes; an edge that still points at a skipped stub simply contributes
        // no rank (its id is absent from `nodes`, treated as 0.0 downstream).
        if file_path.is_empty() {
            continue;
        }
        // Skip nodes on graph-excluded paths (legacy/generated trees, via
        // WQM_GRAPH_EXCLUDE). UNCONDITIONAL — unlike the genericity filters below,
        // this applies to cycles too (a cycle inside old_project/ is scope noise).
        // Edges to them auto-drop (the same "endpoint absent from `nodes`" logic
        // that drops stub edges), so out-degrees stay accurate.
        if !exclude.is_empty() && is_graph_excluded(&file_path, exclude) {
            excluded += 1;
            continue;
        }
        let symbol_name: String = row.get("symbol_name");
        let node_id: String = row.get("node_id");
        // Dynamic genericity filter (R3), two language-agnostic axes — both flag
        // "central by ubiquity, not importance" and drop from centrality only
        // (search/grep/relations/impact see the full graph). NO hardcoded list.
        //   1. DEFINITION ubiquity: a name defined in more places than the
        //      corpus-derived threshold (toString/build/get — any language).
        //   2. USE ubiquity: a NODE whose high-confidence in-degree exceeds the
        //      usage threshold — catches a unique def whose bare name collides
        //      with a stdlib builtin (collect/iter/Result), which axis 1 cannot
        //      see (def_count == 1). Also unglues the giant catch-all community.
        // Plus the optional manual symbol-name env override.
        if apply_genericity_filters
            && (def_count.get(&symbol_name).copied().unwrap_or(0) > generic_threshold
                || indeg_by_node.get(&node_id).copied().unwrap_or(0) > usage_threshold
                || manual_skip.contains(&symbol_name))
        {
            excluded += 1;
            continue;
        }
        nodes.insert(
            node_id,
            NodeInfo {
                symbol_name,
                symbol_type: row.get("symbol_type"),
                file_path,
            },
        );
    }

    // Load edges with optional type filter. Centrality consumes only HIGH-confidence
    // edges (weight >= 0.6): this excludes the 1/N ambiguous fan-out emitted by
    // resolve_stub_edges (R1) so a name collision cannot inflate PageRank/betweenness,
    // while impact/usages (which query graph_edges directly) still traverse every
    // candidate. Pre-R1 edges default to weight 1.0 and are unaffected.
    let edge_rows = if let Some(types) = edge_types {
        let placeholders: Vec<String> = types.iter().map(|t| format!("'{}'", t)).collect();
        let query = format!(
            "SELECT source_node_id, target_node_id FROM graph_edges
             WHERE tenant_id = ?1 AND weight >= 0.6 AND edge_type IN ({})",
            placeholders.join(", ")
        );
        sqlx::query(&query).bind(tenant_id).fetch_all(pool).await?
    } else {
        sqlx::query(
            "SELECT source_node_id, target_node_id FROM graph_edges
             WHERE tenant_id = ?1 AND weight >= 0.6",
        )
        .bind(tenant_id)
        .fetch_all(pool)
        .await?
    };

    let mut outgoing: HashMap<String, Vec<String>> = HashMap::new();
    let mut incoming: HashMap<String, Vec<String>> = HashMap::new();
    let mut dropped_dangling = 0usize;

    for row in &edge_rows {
        let src: String = row.get("source_node_id");
        let tgt: String = row.get("target_node_id");
        // Drop edges whose endpoint is a skipped stub (absent from `nodes`).
        // A stub is not a real node; counting it in a source's out-degree leaks
        // PageRank rank to nowhere — a node whose out-edges ALL point at stubs is
        // not detected as dangling (it has outgoing edges), so its rank is
        // divided away to targets no resolved node collects and the scores stop
        // summing to ~1.0; it also adds phantom hops to betweenness. Keep the
        // in-memory graph internally consistent: edges only between resolved,
        // file-backed nodes (community detection already filtered these at the
        // neighbor-build step).
        if !nodes.contains_key(src.as_str()) || !nodes.contains_key(tgt.as_str()) {
            dropped_dangling += 1;
            continue;
        }
        outgoing.entry(src.clone()).or_default().push(tgt.clone());
        incoming.entry(tgt).or_default().push(src);
    }

    debug!(
        tenant_id,
        nodes = nodes.len(),
        edges = edge_rows.len(),
        dropped_dangling,
        excluded,
        "Loaded adjacency graph"
    );

    Ok(AdjacencyGraph {
        nodes,
        outgoing,
        incoming,
    })
}

#[cfg(test)]
mod tests;
