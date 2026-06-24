/// Graph algorithms: PageRank, community detection, betweenness centrality.
///
/// Implemented as pure functions over adjacency data loaded from any
/// `GraphStore` backend (SQLite or LadybugDB). The algorithms operate on
/// in-memory adjacency lists, so they work identically regardless of backend.
mod betweenness;
mod community;
mod pagerank;

pub use betweenness::{compute_betweenness_centrality, BetweennessEntry};
pub use community::{detect_communities, Community, CommunityConfig, CommunityMember};
pub use pagerank::{compute_pagerank, PageRankConfig, PageRankEntry};

use std::collections::HashMap;
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

/// Path substrings that exclude a node from CENTRALITY (hotspots/bridges/modules)
/// — NOT from search/grep/relations/impact, which never call this loader. Set via
/// the `WQM_GRAPH_CENTRALITY_EXCLUDE` env var (comma-separated substrings), e.g.
/// `old_project/,/test/,Test.java`. A node is excluded when its file_path CONTAINS
/// any pattern. Without this, legacy/test/util code (`old_project/`, `assertEquals`,
/// util builders) dominates PageRank/betweenness/community and buries the real,
/// current hotspots. Empty/unset = no exclusion. Parsed once per process.
fn centrality_exclude_patterns() -> &'static [String] {
    static PATTERNS: OnceLock<Vec<String>> = OnceLock::new();
    PATTERNS.get_or_init(|| {
        std::env::var("WQM_GRAPH_CENTRALITY_EXCLUDE")
            .unwrap_or_default()
            .split(',')
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect()
    })
}

/// True if `file_path` matches any centrality-exclude pattern (substring match).
fn is_centrality_excluded(file_path: &str, patterns: &[String]) -> bool {
    patterns.iter().any(|p| file_path.contains(p.as_str()))
}

/// Load the full adjacency graph for a tenant from SQLite.
pub(super) async fn load_adjacency_graph(
    pool: &SqlitePool,
    tenant_id: &str,
    edge_types: Option<&[&str]>,
) -> Result<AdjacencyGraph, sqlx::Error> {
    // Load nodes
    let node_rows = sqlx::query(
        "SELECT node_id, symbol_name, symbol_type, file_path
         FROM graph_nodes WHERE tenant_id = ?1",
    )
    .bind(tenant_id)
    .fetch_all(pool)
    .await?;

    let mut nodes = HashMap::with_capacity(node_rows.len());
    let exclude = centrality_exclude_patterns();
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
        // Skip nodes on centrality-excluded paths (legacy/test/util noise, via the
        // WQM_GRAPH_CENTRALITY_EXCLUDE env var). Edges to them auto-drop below (the
        // same "endpoint absent from `nodes`" logic that drops stub edges), so
        // out-degrees stay accurate.
        if !exclude.is_empty() && is_centrality_excluded(&file_path, exclude) {
            excluded += 1;
            continue;
        }
        let node_id: String = row.get("node_id");
        nodes.insert(
            node_id,
            NodeInfo {
                symbol_name: row.get("symbol_name"),
                symbol_type: row.get("symbol_type"),
                file_path,
            },
        );
    }

    // Load edges with optional type filter
    let edge_rows = if let Some(types) = edge_types {
        let placeholders: Vec<String> = types.iter().map(|t| format!("'{}'", t)).collect();
        let query = format!(
            "SELECT source_node_id, target_node_id FROM graph_edges
             WHERE tenant_id = ?1 AND edge_type IN ({})",
            placeholders.join(", ")
        );
        sqlx::query(&query).bind(tenant_id).fetch_all(pool).await?
    } else {
        sqlx::query(
            "SELECT source_node_id, target_node_id FROM graph_edges
             WHERE tenant_id = ?1",
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
