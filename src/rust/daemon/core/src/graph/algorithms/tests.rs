use super::*;
use sqlx::sqlite::SqlitePoolOptions;
use sqlx::SqlitePool;

#[test]
fn graph_exclude_matches_by_substring() {
    let patterns = vec![
        "old_project/".to_string(),
        "/tests/".to_string(),
        "Test.java".to_string(),
    ];
    // Prefix, mid-path, and suffix path fragments all match via substring.
    assert!(is_graph_excluded("src/old_project/legacy.rs", &patterns));
    assert!(is_graph_excluded("app/src/tests/Helper.kt", &patterns));
    assert!(is_graph_excluded("api/UserTest.java", &patterns));
    // Real, current source is kept.
    assert!(!is_graph_excluded("src/core/user.rs", &patterns));
    // An empty pattern list never excludes.
    assert!(!is_graph_excluded("src/old_project/legacy.rs", &[]));

    // Substring is DELIBERATE (not the #294 segment matcher): the reference config
    // excludes generated code via filename-INFIX markers a segment/suffix matcher
    // would silently miss.
    let generated = vec![
        "OuterClass".to_string(),
        ".pb.".to_string(),
        "_pb2".to_string(),
        ".g.dart".to_string(),
    ];
    assert!(is_graph_excluded("gen/java/UserOuterClass.java", &generated));
    assert!(is_graph_excluded("lib/api/user.pb.dart", &generated));
    assert!(is_graph_excluded("proto/user_pb2.py", &generated));
    assert!(is_graph_excluded("lib/models/user.g.dart", &generated));
    // Hand-written code with the same base name is NOT generated → kept.
    assert!(!is_graph_excluded("lib/api/user.dart", &generated));
}

#[test]
fn graph_exclude_always_drops_dependency_and_vcs_trees() {
    assert!(is_graph_excluded(
        "src/typescript/mcp-server/node_modules/prom-client/index.d.ts",
        &[]
    ));
    assert!(is_graph_excluded(
        r"src\python\.venv\Lib\site-packages\pkg\module.py",
        &[]
    ));
    assert!(is_graph_excluded(".git/objects/pack/file", &[]));
    assert!(!is_graph_excluded("src/vendor/client.rs", &[]));
    assert!(!is_graph_excluded("src/node_modules_adapter.ts", &[]));
}

#[test]
fn centrality_generic_filter_is_dynamic_not_hardcoded() {
    // There is NO built-in/per-language stoplist — genericity is frequency-derived,
    // so nothing needs curating or updating per language. The env var is an optional
    // manual override only, empty by default.
    assert!(
        centrality_manual_skip_symbols().is_empty(),
        "must have no hardcoded stoplist"
    );
    // The threshold is corpus-derived: floored for a small graph, ~0.2% for a large
    // one (so it adapts to a small lib vs a large monorepo, any language).
    assert_eq!(centrality_generic_threshold(100), 15, "small corpus → floor (15)");
    assert_eq!(centrality_generic_threshold(50_000), 100, "large corpus → ~0.2%");
}

#[test]
fn centrality_usage_threshold_is_dynamic() {
    // USE-ubiquity axis (R3): a NODE called/used far more than any real symbol is
    // demoted even when its name is unique (def_count == 1) — the stdlib-collision
    // case (collect/iter/Result) the definition-count axis cannot see. Threshold is
    // corpus-derived: floor 50, then total/150, CAPPED at 125 (the generic-name line
    // is ~constant across project sizes, not proportional — calibrated on 3 tenants).
    assert_eq!(centrality_usage_threshold(100), 50, "small corpus → floor (50)");
    assert_eq!(centrality_usage_threshold(15_000), 100, "mid corpus → total/150");
    assert_eq!(
        centrality_usage_threshold(30_000),
        125,
        "large corpus → capped at 125 (not 200) — generic line is ~constant, not proportional"
    );
}

/// Create an in-memory SQLite pool with graph schema.
async fn setup_graph_pool() -> SqlitePool {
    let pool = SqlitePoolOptions::new()
        .max_connections(1)
        .connect("sqlite::memory:")
        .await
        .unwrap();

    sqlx::query(
        "CREATE TABLE graph_nodes (
            node_id TEXT PRIMARY KEY,
            tenant_id TEXT NOT NULL,
            symbol_name TEXT NOT NULL,
            symbol_type TEXT NOT NULL,
            file_path TEXT NOT NULL,
            start_line INTEGER,
            end_line INTEGER,
            signature TEXT,
            language TEXT,
            is_test_symbol INTEGER NOT NULL DEFAULT 0,
            created_at TEXT NOT NULL DEFAULT '',
            updated_at TEXT NOT NULL DEFAULT ''
        )",
    )
    .execute(&pool)
    .await
    .unwrap();

    sqlx::query("CREATE INDEX idx_nodes_tenant ON graph_nodes(tenant_id)")
        .execute(&pool)
        .await
        .unwrap();

    sqlx::query(
        "CREATE TABLE graph_edges (
            edge_id TEXT PRIMARY KEY,
            tenant_id TEXT NOT NULL,
            source_node_id TEXT NOT NULL,
            target_node_id TEXT NOT NULL,
            edge_type TEXT NOT NULL,
            source_file TEXT NOT NULL,
            weight REAL DEFAULT 1.0,
            metadata_json TEXT,
            created_at TEXT NOT NULL DEFAULT ''
        )",
    )
    .execute(&pool)
    .await
    .unwrap();

    sqlx::query("CREATE INDEX idx_edges_tenant ON graph_edges(tenant_id)")
        .execute(&pool)
        .await
        .unwrap();

    pool
}

async fn insert_node(pool: &SqlitePool, tenant: &str, id: &str, name: &str, stype: &str) {
    sqlx::query(
        "INSERT OR IGNORE INTO graph_nodes (node_id, tenant_id, symbol_name, symbol_type, file_path)
         VALUES (?, ?, ?, ?, ?)",
    )
    .bind(id)
    .bind(tenant)
    .bind(name)
    .bind(stype)
    .bind(format!("{}.rs", name))
    .execute(pool)
    .await
    .unwrap();
}

async fn insert_edge(pool: &SqlitePool, tenant: &str, src: &str, tgt: &str, etype: &str) {
    let edge_id = format!("{}_{}_{}_{}", tenant, src, tgt, etype);
    sqlx::query(
        "INSERT OR IGNORE INTO graph_edges (edge_id, tenant_id, source_node_id, target_node_id, edge_type, source_file)
         VALUES (?, ?, ?, ?, ?, ?)",
    )
    .bind(&edge_id)
    .bind(tenant)
    .bind(src)
    .bind(tgt)
    .bind(etype)
    .bind("src.rs")
    .execute(pool)
    .await
    .unwrap();
}

/// Build a diamond graph: A -> B, A -> C, B -> D, C -> D
async fn build_diamond(pool: &SqlitePool) {
    for (id, name) in &[
        ("a", "alpha"),
        ("b", "beta"),
        ("c", "gamma"),
        ("d", "delta"),
    ] {
        insert_node(pool, "t1", id, name, "function").await;
    }
    insert_edge(pool, "t1", "a", "b", "CALLS").await;
    insert_edge(pool, "t1", "a", "c", "CALLS").await;
    insert_edge(pool, "t1", "b", "d", "CALLS").await;
    insert_edge(pool, "t1", "c", "d", "CALLS").await;
}

/// Build a chain: A -> B -> C -> D -> E
async fn build_chain(pool: &SqlitePool) {
    for (id, name) in &[("a", "a"), ("b", "b"), ("c", "c"), ("d", "d"), ("e", "e")] {
        insert_node(pool, "t1", id, name, "function").await;
    }
    insert_edge(pool, "t1", "a", "b", "CALLS").await;
    insert_edge(pool, "t1", "b", "c", "CALLS").await;
    insert_edge(pool, "t1", "c", "d", "CALLS").await;
    insert_edge(pool, "t1", "d", "e", "CALLS").await;
}

/// Build two clusters: {A,B,C} densely connected, {D,E,F} densely connected,
/// with one bridge B->D.
async fn build_two_clusters(pool: &SqlitePool) {
    for (id, name) in &[
        ("a", "a"),
        ("b", "b"),
        ("c", "c"),
        ("d", "d"),
        ("e", "e"),
        ("f", "f"),
    ] {
        insert_node(pool, "t1", id, name, "function").await;
    }
    // Cluster 1: a-b-c fully connected
    insert_edge(pool, "t1", "a", "b", "CALLS").await;
    insert_edge(pool, "t1", "b", "a", "CALLS").await;
    insert_edge(pool, "t1", "a", "c", "CALLS").await;
    insert_edge(pool, "t1", "c", "a", "CALLS").await;
    insert_edge(pool, "t1", "b", "c", "CALLS").await;
    insert_edge(pool, "t1", "c", "b", "CALLS").await;
    // Cluster 2: d-e-f fully connected
    insert_edge(pool, "t1", "d", "e", "CALLS").await;
    insert_edge(pool, "t1", "e", "d", "CALLS").await;
    insert_edge(pool, "t1", "d", "f", "CALLS").await;
    insert_edge(pool, "t1", "f", "d", "CALLS").await;
    insert_edge(pool, "t1", "e", "f", "CALLS").await;
    insert_edge(pool, "t1", "f", "e", "CALLS").await;
    // Bridge: b -> d
    insert_edge(pool, "t1", "b", "d", "CALLS").await;
}

// ─── PageRank tests ──────────────────────────────────────────────

#[tokio::test]
async fn test_pagerank_empty_graph() {
    let pool = setup_graph_pool().await;
    let config = PageRankConfig::default();
    let results = compute_pagerank(&pool, "t1", &config, None).await.unwrap();
    assert!(results.is_empty());
}

#[tokio::test]
async fn test_pagerank_single_node() {
    let pool = setup_graph_pool().await;
    insert_node(&pool, "t1", "a", "alpha", "function").await;

    let config = PageRankConfig::default();
    let results = compute_pagerank(&pool, "t1", &config, None).await.unwrap();
    assert_eq!(results.len(), 1);
    // compute_pagerank applies the R3 IDF demotion (score *= ln(total/count)). A
    // lone unique symbol has total == count == 1 → ln(1) == 0, so its final score
    // is 0 even though the raw power-iteration gives it all the rank. Assert the
    // node is returned rather than an (IDF-erased) absolute score.
    assert_eq!(results[0].node_id, "a");
}

#[tokio::test]
async fn test_pagerank_diamond() {
    let pool = setup_graph_pool().await;
    build_diamond(&pool).await;

    let config = PageRankConfig::default();
    let results = compute_pagerank(&pool, "t1", &config, None).await.unwrap();
    assert_eq!(results.len(), 4);

    // Node D should have highest PageRank (two incoming edges)
    let d_score = results.iter().find(|r| r.node_id == "d").unwrap().score;
    let a_score = results.iter().find(|r| r.node_id == "a").unwrap().score;
    assert!(
        d_score > a_score,
        "D (sink with 2 inputs) should rank higher than A (source): d={}, a={}",
        d_score,
        a_score
    );
}

#[tokio::test]
async fn test_pagerank_chain() {
    let pool = setup_graph_pool().await;
    build_chain(&pool).await;

    let config = PageRankConfig::default();
    let results = compute_pagerank(&pool, "t1", &config, None).await.unwrap();
    assert_eq!(results.len(), 5);

    // Raw PageRank sums to ~1.0. compute_pagerank scales each score by the R3 IDF
    // factor, which for these all-distinct names is the uniform ln(total); divide
    // it back out to check the underlying normalization survives.
    let total: f64 = results.iter().map(|r| r.score).sum();
    let raw_total = total / (results.len() as f64).ln();
    assert!(
        (raw_total - 1.0).abs() < 0.01,
        "raw PageRank should sum to ~1.0 (IDF-weighted total {total}), got raw {raw_total}"
    );
}

#[tokio::test]
async fn test_pagerank_convergence() {
    let pool = setup_graph_pool().await;
    build_diamond(&pool).await;

    let config = PageRankConfig {
        damping: 0.85,
        max_iterations: 1000,
        tolerance: 1e-10,
        ..Default::default()
    };
    let results = compute_pagerank(&pool, "t1", &config, None).await.unwrap();

    // Should converge to stable values. Divide out the uniform R3 IDF factor
    // (ln(total), all names distinct) to check the raw distribution sums to ~1.0.
    let total: f64 = results.iter().map(|r| r.score).sum();
    let raw_total = total / (results.len() as f64).ln();
    assert!((raw_total - 1.0).abs() < 1e-6);
}

#[tokio::test]
async fn test_pagerank_edge_type_filter() {
    let pool = setup_graph_pool().await;
    // A -CALLS-> B, A -IMPORTS-> C
    insert_node(&pool, "t1", "a", "a", "function").await;
    insert_node(&pool, "t1", "b", "b", "function").await;
    insert_node(&pool, "t1", "c", "c", "function").await;
    insert_edge(&pool, "t1", "a", "b", "CALLS").await;
    insert_edge(&pool, "t1", "a", "c", "IMPORTS").await;

    let config = PageRankConfig::default();
    let results = compute_pagerank(&pool, "t1", &config, Some(&["CALLS"]))
        .await
        .unwrap();

    // C should have low PageRank since IMPORTS edges are excluded
    let b_score = results.iter().find(|r| r.node_id == "b").unwrap().score;
    let c_score = results.iter().find(|r| r.node_id == "c").unwrap().score;
    assert!(
        b_score > c_score,
        "B should rank higher when only CALLS are considered"
    );
}

// ─── Community detection tests ───────────────────────────────────

#[tokio::test]
async fn test_communities_empty() {
    let pool = setup_graph_pool().await;
    let config = CommunityConfig::default();
    let communities = detect_communities(&pool, "t1", &config, None)
        .await
        .unwrap();
    assert!(communities.is_empty());
}

#[tokio::test]
async fn test_communities_two_disconnected_clusters() {
    let pool = setup_graph_pool().await;

    // Two disconnected clusters: {a,b,c} and {d,e,f}
    for (id, name) in &[
        ("a", "a"),
        ("b", "b"),
        ("c", "c"),
        ("d", "d"),
        ("e", "e"),
        ("f", "f"),
    ] {
        insert_node(&pool, "t1", id, name, "function").await;
    }
    // Cluster 1
    insert_edge(&pool, "t1", "a", "b", "CALLS").await;
    insert_edge(&pool, "t1", "b", "c", "CALLS").await;
    insert_edge(&pool, "t1", "c", "a", "CALLS").await;
    // Cluster 2
    insert_edge(&pool, "t1", "d", "e", "CALLS").await;
    insert_edge(&pool, "t1", "e", "f", "CALLS").await;
    insert_edge(&pool, "t1", "f", "d", "CALLS").await;

    let config = CommunityConfig {
        max_iterations: 100,
        min_community_size: 2,
    };
    let communities = detect_communities(&pool, "t1", &config, None)
        .await
        .unwrap();

    // Should detect exactly 2 communities
    assert_eq!(
        communities.len(),
        2,
        "Expected 2 disconnected communities, got {}",
        communities.len()
    );

    // Each community should have 3 members
    assert_eq!(communities[0].members.len(), 3);
    assert_eq!(communities[1].members.len(), 3);
}

#[tokio::test]
async fn test_communities_fully_connected() {
    let pool = setup_graph_pool().await;

    // All nodes connected → one community
    for (id, name) in &[("a", "a"), ("b", "b"), ("c", "c")] {
        insert_node(&pool, "t1", id, name, "function").await;
    }
    insert_edge(&pool, "t1", "a", "b", "CALLS").await;
    insert_edge(&pool, "t1", "b", "c", "CALLS").await;
    insert_edge(&pool, "t1", "c", "a", "CALLS").await;

    let config = CommunityConfig::default();
    let communities = detect_communities(&pool, "t1", &config, None)
        .await
        .unwrap();

    assert_eq!(communities.len(), 1);
    assert_eq!(communities[0].members.len(), 3);
}

#[tokio::test]
async fn test_communities_min_size_filter() {
    let pool = setup_graph_pool().await;

    // Two nodes connected, one isolated
    insert_node(&pool, "t1", "a", "a", "function").await;
    insert_node(&pool, "t1", "b", "b", "function").await;
    insert_node(&pool, "t1", "c", "c", "function").await;
    insert_edge(&pool, "t1", "a", "b", "CALLS").await;

    let config = CommunityConfig {
        min_community_size: 2,
        ..Default::default()
    };
    let communities = detect_communities(&pool, "t1", &config, None)
        .await
        .unwrap();

    // Only the {a, b} community should pass the filter
    assert_eq!(communities.len(), 1);
    assert_eq!(communities[0].members.len(), 2);
}

#[tokio::test]
async fn test_communities_sorted_by_size() {
    let pool = setup_graph_pool().await;
    build_two_clusters(&pool).await;

    // Add extra node to cluster 1 to make it bigger
    insert_node(&pool, "t1", "g", "g", "function").await;
    insert_edge(&pool, "t1", "a", "g", "CALLS").await;
    insert_edge(&pool, "t1", "g", "a", "CALLS").await;

    let config = CommunityConfig::default();
    let communities = detect_communities(&pool, "t1", &config, None)
        .await
        .unwrap();

    if communities.len() >= 2 {
        assert!(
            communities[0].members.len() >= communities[1].members.len(),
            "Communities should be sorted by size descending"
        );
    }
}

// ─── Betweenness centrality tests ────────────────────────────────

#[tokio::test]
async fn test_betweenness_empty() {
    let pool = setup_graph_pool().await;
    let results = compute_betweenness_centrality(&pool, "t1", None, None)
        .await
        .unwrap();
    assert!(results.is_empty());
}

#[tokio::test]
async fn test_betweenness_chain() {
    let pool = setup_graph_pool().await;
    build_chain(&pool).await;

    let results = compute_betweenness_centrality(&pool, "t1", None, None)
        .await
        .unwrap();
    assert_eq!(results.len(), 5);

    // Middle nodes (b, c, d) should have higher betweenness than endpoints
    let _b_score = results.iter().find(|r| r.node_id == "b").unwrap().score;
    let c_score = results.iter().find(|r| r.node_id == "c").unwrap().score;
    let a_score = results.iter().find(|r| r.node_id == "a").unwrap().score;
    let e_score = results.iter().find(|r| r.node_id == "e").unwrap().score;

    assert!(
        c_score >= a_score,
        "Center node c should have >= betweenness than endpoint a: c={}, a={}",
        c_score,
        a_score
    );
    assert!(
        c_score >= e_score,
        "Center node c should have >= betweenness than endpoint e: c={}, e={}",
        c_score,
        e_score
    );
}

#[tokio::test]
async fn test_betweenness_bridge_node() {
    let pool = setup_graph_pool().await;
    build_two_clusters(&pool).await;

    let results = compute_betweenness_centrality(&pool, "t1", None, None)
        .await
        .unwrap();

    // Bridge nodes (b and d) should have highest betweenness
    let b_score = results.iter().find(|r| r.node_id == "b").unwrap().score;
    let d_score = results.iter().find(|r| r.node_id == "d").unwrap().score;
    let a_score = results.iter().find(|r| r.node_id == "a").unwrap().score;

    // b connects the two clusters, so it should have high betweenness
    assert!(
        b_score > a_score || d_score > a_score,
        "Bridge nodes should have higher betweenness: b={}, d={}, a={}",
        b_score,
        d_score,
        a_score
    );
}

#[tokio::test]
async fn test_betweenness_small_graph() {
    let pool = setup_graph_pool().await;

    // Two nodes, one edge
    insert_node(&pool, "t1", "a", "a", "function").await;
    insert_node(&pool, "t1", "b", "b", "function").await;
    insert_edge(&pool, "t1", "a", "b", "CALLS").await;

    let results = compute_betweenness_centrality(&pool, "t1", None, None)
        .await
        .unwrap();
    assert_eq!(results.len(), 2);
    // With only 2 nodes, betweenness is 0 for both
    assert!(results.iter().all(|r| r.score == 0.0));
}

#[tokio::test]
async fn test_betweenness_with_sampling() {
    let pool = setup_graph_pool().await;
    build_chain(&pool).await;

    // Sample only 2 source nodes
    let results = compute_betweenness_centrality(&pool, "t1", None, Some(2))
        .await
        .unwrap();
    assert_eq!(results.len(), 5);
}

// ─── Load adjacency ──────────────────────────────────────────────

#[tokio::test]
async fn test_load_adjacency() {
    let pool = setup_graph_pool().await;
    build_diamond(&pool).await;

    let graph = load_adjacency_graph(&pool, "t1", None, true).await.unwrap();
    assert_eq!(graph.nodes.len(), 4);
    assert_eq!(graph.outgoing.get("a").unwrap().len(), 2); // a -> b, a -> c
    assert_eq!(graph.incoming.get("d").unwrap().len(), 2); // b -> d, c -> d
}

#[tokio::test]
async fn test_load_adjacency_filtered() {
    let pool = setup_graph_pool().await;

    insert_node(&pool, "t1", "a", "a", "function").await;
    insert_node(&pool, "t1", "b", "b", "function").await;
    insert_edge(&pool, "t1", "a", "b", "CALLS").await;
    insert_edge(&pool, "t1", "a", "b", "IMPORTS").await;

    // Filter to CALLS only
    let graph = load_adjacency_graph(&pool, "t1", Some(&["CALLS"]), true)
        .await
        .unwrap();
    let out = graph.outgoing.get("a").unwrap();
    assert_eq!(out.len(), 1); // only the CALLS edge
}

#[tokio::test]
async fn test_load_adjacency_drops_use_ubiquitous_node() {
    // A NODE called by far more sites than any real symbol — a unique-name def
    // whose bare name collides with a stdlib builtin (e.g. `collect`/`iter`), so
    // the resolver funnels every same-named call onto it — is dropped from
    // centrality by the USE-ubiquity axis (R3). def_count is 1, so the
    // definition-count axis cannot catch it; the in-degree axis does. A normally
    // referenced node and the callers themselves stay.
    let pool = setup_graph_pool().await;

    insert_node(&pool, "t1", "hub", "collect", "method").await;
    insert_node(&pool, "t1", "norm", "enqueue_unified", "method").await;

    // 60 distinct callers all calling the hub (> the floor-50 default threshold);
    // two of them also call the normal node (in-degree 2, well under threshold).
    for i in 0..60 {
        let cid = format!("c{i}");
        insert_node(&pool, "t1", &cid, &format!("caller_{i}"), "function").await;
        insert_edge(&pool, "t1", &cid, "hub", "CALLS").await;
    }
    insert_edge(&pool, "t1", "c0", "norm", "CALLS").await;
    insert_edge(&pool, "t1", "c1", "norm", "CALLS").await;

    let graph = load_adjacency_graph(&pool, "t1", None, true).await.unwrap();

    assert!(
        !graph.nodes.contains_key("hub"),
        "use-ubiquitous node (in-degree 60 > floor 50) must be dropped from centrality"
    );
    assert!(
        graph.nodes.contains_key("norm"),
        "a normally-referenced node (in-degree 2) must be kept"
    );
    assert!(
        graph.nodes.contains_key("c0"),
        "caller nodes must be kept"
    );

    // With genericity filters OFF (structural callers like cycle detection), the
    // same use-ubiquitous node is KEPT — a real cycle may pass through it.
    let raw = load_adjacency_graph(&pool, "t1", None, false)
        .await
        .unwrap();
    assert!(
        raw.nodes.contains_key("hub"),
        "with genericity filters off, the ubiquitous node must be retained"
    );
}
