//! Tests for graph traversal queries, impact analysis, stats, and orphan pruning.

use super::*;

// -- Helper: build a call chain a -> b -> c -> d --

async fn build_call_chain(
    store: &SqliteGraphStore,
) -> (GraphNode, GraphNode, GraphNode, GraphNode) {
    let a = GraphNode::new(TENANT, "a.rs", "a", NodeType::Function);
    let b = GraphNode::new(TENANT, "b.rs", "b", NodeType::Function);
    let c = GraphNode::new(TENANT, "c.rs", "c", NodeType::Function);
    let d = GraphNode::new(TENANT, "d.rs", "d", NodeType::Function);
    store
        .upsert_nodes(&[a.clone(), b.clone(), c.clone(), d.clone()])
        .await
        .unwrap();

    let edges = vec![
        GraphEdge::new(TENANT, &a.node_id, &b.node_id, EdgeType::Calls, "a.rs"),
        GraphEdge::new(TENANT, &b.node_id, &c.node_id, EdgeType::Calls, "b.rs"),
        GraphEdge::new(TENANT, &c.node_id, &d.node_id, EdgeType::Calls, "c.rs"),
    ];
    store.insert_edges(&edges).await.unwrap();

    (a, b, c, d)
}

// -- Query related (recursive CTE) --

#[tokio::test]
async fn test_query_related_1_hop() {
    let store = test_store().await;
    let (a, b, _c, _d) = build_call_chain(&store).await;

    let results = store
        .query_related(TENANT, &a.node_id, 1, None)
        .await
        .unwrap();

    assert_eq!(results.len(), 1);
    assert_eq!(results[0].node_id, b.node_id);
    assert_eq!(results[0].depth, 1);
}

#[tokio::test]
async fn test_query_related_2_hops() {
    let store = test_store().await;
    let (a, _b, _c, _d) = build_call_chain(&store).await;

    let results = store
        .query_related(TENANT, &a.node_id, 2, None)
        .await
        .unwrap();

    assert_eq!(results.len(), 2);
    assert_eq!(results[0].depth, 1);
    assert_eq!(results[1].depth, 2);
}

#[tokio::test]
async fn test_query_related_3_hops_reaches_end() {
    let store = test_store().await;
    let (a, _b, _c, _d) = build_call_chain(&store).await;

    let results = store
        .query_related(TENANT, &a.node_id, 3, None)
        .await
        .unwrap();

    assert_eq!(results.len(), 3); // b, c, d
}

#[tokio::test]
async fn test_query_related_max_hops_boundary() {
    let store = test_store().await;
    let (a, _b, _c, _d) = build_call_chain(&store).await;

    // max_hops=0 should return nothing
    let results = store
        .query_related(TENANT, &a.node_id, 0, None)
        .await
        .unwrap();
    assert_eq!(results.len(), 0);
}

#[tokio::test]
async fn test_query_related_edge_type_filter() {
    let store = test_store().await;

    let a = GraphNode::new(TENANT, "a.rs", "a", NodeType::Function);
    let b = GraphNode::new(TENANT, "b.rs", "b", NodeType::Function);
    let c = GraphNode::new(TENANT, "c.rs", "c", NodeType::Struct);
    store
        .upsert_nodes(&[a.clone(), b.clone(), c.clone()])
        .await
        .unwrap();

    let edges = vec![
        GraphEdge::new(TENANT, &a.node_id, &b.node_id, EdgeType::Calls, "a.rs"),
        GraphEdge::new(TENANT, &a.node_id, &c.node_id, EdgeType::UsesType, "a.rs"),
    ];
    store.insert_edges(&edges).await.unwrap();

    // Filter to CALLS only
    let results = store
        .query_related(TENANT, &a.node_id, 1, Some(&[EdgeType::Calls]))
        .await
        .unwrap();
    assert_eq!(results.len(), 1);
    assert_eq!(results[0].node_id, b.node_id);

    // Filter to USES_TYPE only
    let results = store
        .query_related(TENANT, &a.node_id, 1, Some(&[EdgeType::UsesType]))
        .await
        .unwrap();
    assert_eq!(results.len(), 1);
    assert_eq!(results[0].node_id, c.node_id);
}

#[tokio::test]
async fn test_query_related_same_depth_keeps_strongest_path_confidence() {
    let store = test_store().await;

    // s -(0.2)-> m_weak -(1.0)-> c        (product 0.2)
    // s -(0.9)-> m_strong -(1.0)-> c      (product 0.9)
    // `c` is reached at depth 2 via BOTH parents; the recorded confidence must
    // be the strongest same-depth arrival (0.9), not whichever row the
    // unordered per-hop query returned first. (Regression: first-visit-wins
    // understated `confidence` and, under a min_confidence filter, wrongly
    // dropped such nodes.)
    let s = GraphNode::new(TENANT, "s.rs", "s", NodeType::Function);
    let m_weak = GraphNode::new(TENANT, "mw.rs", "m_weak", NodeType::Function);
    let m_strong = GraphNode::new(TENANT, "ms.rs", "m_strong", NodeType::Function);
    let c = GraphNode::new(TENANT, "c.rs", "c", NodeType::Function);
    store
        .upsert_nodes(&[s.clone(), m_weak.clone(), m_strong.clone(), c.clone()])
        .await
        .unwrap();

    let mut s_to_weak = GraphEdge::new(TENANT, &s.node_id, &m_weak.node_id, EdgeType::Calls, "s.rs");
    s_to_weak.weight = 0.2;
    let mut s_to_strong =
        GraphEdge::new(TENANT, &s.node_id, &m_strong.node_id, EdgeType::Calls, "s.rs");
    s_to_strong.weight = 0.9;
    // Insert the weak path's edge to `c` FIRST: without best-arrival
    // aggregation, SQLite's insertion-order rows make first-wins record 0.2.
    let weak_to_c = GraphEdge::new(TENANT, &m_weak.node_id, &c.node_id, EdgeType::Calls, "mw.rs");
    let strong_to_c =
        GraphEdge::new(TENANT, &m_strong.node_id, &c.node_id, EdgeType::Calls, "ms.rs");
    store
        .insert_edges(&[s_to_weak, s_to_strong, weak_to_c, strong_to_c])
        .await
        .unwrap();

    let results = store.query_related(TENANT, &s.node_id, 2, None).await.unwrap();
    let c_hit = results
        .iter()
        .find(|n| n.node_id == c.node_id)
        .expect("c reached at depth 2");
    assert_eq!(c_hit.depth, 2);
    assert!(
        (c_hit.confidence - 0.9).abs() < 1e-9,
        "confidence must be the strongest same-depth path product (0.9), got {}",
        c_hit.confidence
    );
}

#[tokio::test]
async fn test_impact_same_depth_keeps_strongest_caller_confidence() {
    let store = test_store().await;

    // x references `target` via TWO parallel edges: CALLS (0.2) and
    // USES_TYPE (0.9). Reverse traversal must record x with the strongest
    // edge's confidence, same rationale as the forward test above.
    let target = GraphNode::new(TENANT, "t.rs", "target", NodeType::Function);
    let x = GraphNode::new(TENANT, "x.rs", "x", NodeType::Function);
    store.upsert_nodes(&[target.clone(), x.clone()]).await.unwrap();

    let mut weak = GraphEdge::new(TENANT, &x.node_id, &target.node_id, EdgeType::Calls, "x.rs");
    weak.weight = 0.2;
    let mut strong =
        GraphEdge::new(TENANT, &x.node_id, &target.node_id, EdgeType::UsesType, "x.rs");
    strong.weight = 0.9;
    // Weak edge first (see forward test).
    store.insert_edges(&[weak, strong]).await.unwrap();

    let report = store.impact_analysis(TENANT, "target", None).await.unwrap();
    assert_eq!(report.total_impacted, 1);
    assert!(
        (report.impacted_nodes[0].confidence - 0.9).abs() < 1e-9,
        "strongest same-depth edge must win, got {}",
        report.impacted_nodes[0].confidence
    );
}

/// A re-convergent (diamond) graph — a -> b, a -> c, b -> d, c -> d — must return
/// each reachable node ONCE at its minimum depth. The old recursive UNION-ALL CTE
/// emitted `d` twice (one row per path a->b->d and a->c->d); on a hub-heavy graph
/// that per-path fan-out was exponential (a live 1-hop relation measured ~60s).
/// The bounded BFS visits each node once.
#[tokio::test]
async fn test_query_related_reconvergent_dedup() {
    let store = test_store().await;

    let a = GraphNode::new(TENANT, "a.rs", "a", NodeType::Function);
    let b = GraphNode::new(TENANT, "b.rs", "b", NodeType::Function);
    let c = GraphNode::new(TENANT, "c.rs", "c", NodeType::Function);
    let d = GraphNode::new(TENANT, "d.rs", "d", NodeType::Function);
    store
        .upsert_nodes(&[a.clone(), b.clone(), c.clone(), d.clone()])
        .await
        .unwrap();

    let edges = vec![
        GraphEdge::new(TENANT, &a.node_id, &b.node_id, EdgeType::Calls, "a.rs"),
        GraphEdge::new(TENANT, &a.node_id, &c.node_id, EdgeType::Calls, "a.rs"),
        GraphEdge::new(TENANT, &b.node_id, &d.node_id, EdgeType::Calls, "b.rs"),
        GraphEdge::new(TENANT, &c.node_id, &d.node_id, EdgeType::Calls, "c.rs"),
    ];
    store.insert_edges(&edges).await.unwrap();

    let results = store
        .query_related(TENANT, &a.node_id, 3, None)
        .await
        .unwrap();

    // b, c, d — d exactly once (reached via b OR c, not both), at its min depth.
    assert_eq!(
        results.len(),
        3,
        "each reachable node once: {:?}",
        results.iter().map(|r| &r.symbol_name).collect::<Vec<_>>()
    );
    let d_rows: Vec<_> = results.iter().filter(|r| r.symbol_name == "d").collect();
    assert_eq!(d_rows.len(), 1, "re-convergent node d appears once");
    assert_eq!(d_rows[0].depth, 2, "d reached at its minimum depth (2)");
}

// -- Impact analysis --

#[tokio::test]
async fn test_impact_analysis_direct_callers() {
    let store = test_store().await;

    let caller1 = GraphNode::new(TENANT, "a.rs", "caller1", NodeType::Function);
    let caller2 = GraphNode::new(TENANT, "b.rs", "caller2", NodeType::Function);
    let target = GraphNode::new(TENANT, "lib.rs", "target_fn", NodeType::Function);
    store
        .upsert_nodes(&[caller1.clone(), caller2.clone(), target.clone()])
        .await
        .unwrap();

    let edges = vec![
        GraphEdge::new(
            TENANT,
            &caller1.node_id,
            &target.node_id,
            EdgeType::Calls,
            "a.rs",
        ),
        GraphEdge::new(
            TENANT,
            &caller2.node_id,
            &target.node_id,
            EdgeType::Calls,
            "b.rs",
        ),
    ];
    store.insert_edges(&edges).await.unwrap();

    let report = store
        .impact_analysis(TENANT, "target_fn", Some("lib.rs"))
        .await
        .unwrap();

    assert_eq!(report.symbol_name, "target_fn");
    assert_eq!(report.total_impacted, 2);
    assert!(report
        .impacted_nodes
        .iter()
        .all(|n| n.impact_type == "direct_caller"));
}

/// A path-anchored `impact_analysis` (file_path given) must drop the R1 ambiguous
/// fan-out — the low-weight (<0.6) edge a call site emits to a same-name definition
/// when it could not resolve which one. `find_target_nodes` already scopes the
/// target by file; this proves the reverse traversal also gates on edge confidence,
/// matching the `weight >= 0.6` floor cycles/centrality use. Without a file_path the
/// query stays broad and keeps the fan-out.
#[tokio::test]
async fn test_impact_strict_filepath_drops_ambiguous_fanout() {
    let store = test_store().await;

    let target = GraphNode::new(TENANT, "lib.rs", "remove", NodeType::Function);
    let confident = GraphNode::new(TENANT, "user.rs", "confident_caller", NodeType::Function);
    let ambiguous = GraphNode::new(TENANT, "other.rs", "ambiguous_caller", NodeType::Function);
    store
        .upsert_nodes(&[target.clone(), confident.clone(), ambiguous.clone()])
        .await
        .unwrap();

    // A confident caller (default weight 1.0) and an unresolved 1/N fan-out caller
    // whose call could belong to any same-name `remove` (ambiguous, weight 0.3).
    let good = GraphEdge::new(
        TENANT,
        &confident.node_id,
        &target.node_id,
        EdgeType::Calls,
        "user.rs",
    );
    let mut fanout = GraphEdge::new(
        TENANT,
        &ambiguous.node_id,
        &target.node_id,
        EdgeType::Calls,
        "other.rs",
    );
    fanout.weight = 0.3;
    store.insert_edges(&[good, fanout]).await.unwrap();

    // Strict: file_path anchors the target → the 0.3 fan-out edge is dropped.
    let strict = store
        .impact_analysis(TENANT, "remove", Some("lib.rs"))
        .await
        .unwrap();
    assert_eq!(
        strict.total_impacted,
        1,
        "path-anchored impact keeps only the confident caller, got {:?}",
        strict
            .impacted_nodes
            .iter()
            .map(|n| &n.symbol_name)
            .collect::<Vec<_>>()
    );
    assert_eq!(strict.impacted_nodes[0].symbol_name, "confident_caller");

    // Broad: no file_path → unchanged, both callers returned.
    let broad = store.impact_analysis(TENANT, "remove", None).await.unwrap();
    assert_eq!(
        broad.total_impacted, 2,
        "unanchored impact stays broad and keeps the fan-out"
    );
}

#[tokio::test]
async fn test_impact_analysis_transitive() {
    let store = test_store().await;

    // indirect_caller -> direct_caller -> target
    let indirect = GraphNode::new(TENANT, "a.rs", "indirect", NodeType::Function);
    let direct = GraphNode::new(TENANT, "b.rs", "direct", NodeType::Function);
    let target = GraphNode::new(TENANT, "c.rs", "target", NodeType::Function);
    store
        .upsert_nodes(&[indirect.clone(), direct.clone(), target.clone()])
        .await
        .unwrap();

    let edges = vec![
        GraphEdge::new(
            TENANT,
            &indirect.node_id,
            &direct.node_id,
            EdgeType::Calls,
            "a.rs",
        ),
        GraphEdge::new(
            TENANT,
            &direct.node_id,
            &target.node_id,
            EdgeType::Calls,
            "b.rs",
        ),
    ];
    store.insert_edges(&edges).await.unwrap();

    let report = store
        .impact_analysis(TENANT, "target", Some("c.rs"))
        .await
        .unwrap();

    assert_eq!(report.total_impacted, 2);
    // direct at distance 1, indirect at distance 2
    let direct_node = report
        .impacted_nodes
        .iter()
        .find(|n| n.symbol_name == "direct")
        .unwrap();
    assert_eq!(direct_node.distance, 1);
    let indirect_node = report
        .impacted_nodes
        .iter()
        .find(|n| n.symbol_name == "indirect")
        .unwrap();
    assert_eq!(indirect_node.distance, 2);
}

#[tokio::test]
async fn test_impact_analysis_symbol_not_found() {
    let store = test_store().await;

    let report = store
        .impact_analysis(TENANT, "nonexistent", None)
        .await
        .unwrap();

    assert_eq!(report.total_impacted, 0);
    assert!(report.impacted_nodes.is_empty());
}

// -- Stats --

#[tokio::test]
async fn test_stats_empty() {
    let store = test_store().await;
    let stats = store.stats(Some(TENANT)).await.unwrap();
    assert_eq!(stats.total_nodes, 0);
    assert_eq!(stats.total_edges, 0);
}

#[tokio::test]
async fn test_stats_by_type() {
    let store = test_store().await;

    let nodes = vec![
        GraphNode::new(TENANT, "a.rs", "a", NodeType::Function),
        GraphNode::new(TENANT, "b.rs", "b", NodeType::Function),
        GraphNode::new(TENANT, "c.rs", "C", NodeType::Struct),
    ];
    store.upsert_nodes(&nodes).await.unwrap();

    let stats = store.stats(Some(TENANT)).await.unwrap();
    assert_eq!(stats.total_nodes, 3);
    assert_eq!(stats.nodes_by_type.get("function"), Some(&2));
    assert_eq!(stats.nodes_by_type.get("struct"), Some(&1));
}

#[tokio::test]
async fn test_stats_all_tenants() {
    let store = test_store().await;

    let node_a = GraphNode::new("tenant-a", "a.rs", "x", NodeType::Function);
    let node_b = GraphNode::new("tenant-b", "b.rs", "y", NodeType::Function);
    store.upsert_nodes(&[node_a, node_b]).await.unwrap();

    let stats = store.stats(None).await.unwrap();
    assert_eq!(stats.total_nodes, 2);
}

// -- Prune orphans --

#[tokio::test]
async fn test_prune_orphans() {
    let store = test_store().await;

    let connected_a = GraphNode::new(TENANT, "a.rs", "a", NodeType::Function);
    let connected_b = GraphNode::new(TENANT, "b.rs", "b", NodeType::Function);
    let orphan = GraphNode::new(TENANT, "c.rs", "orphan", NodeType::Function);
    store
        .upsert_nodes(&[connected_a.clone(), connected_b.clone(), orphan.clone()])
        .await
        .unwrap();

    // Only a->b has an edge; orphan has none
    let edge = GraphEdge::new(
        TENANT,
        &connected_a.node_id,
        &connected_b.node_id,
        EdgeType::Calls,
        "a.rs",
    );
    store.insert_edge(&edge).await.unwrap();

    let pruned = store.prune_orphans(TENANT).await.unwrap();
    assert_eq!(pruned, 1);

    let stats = store.stats(Some(TENANT)).await.unwrap();
    assert_eq!(stats.total_nodes, 2); // only connected nodes remain
}

#[tokio::test]
async fn test_prune_orphans_none_to_prune() {
    let store = test_store().await;

    let a = GraphNode::new(TENANT, "a.rs", "a", NodeType::Function);
    let b = GraphNode::new(TENANT, "b.rs", "b", NodeType::Function);
    store.upsert_nodes(&[a.clone(), b.clone()]).await.unwrap();

    let edge = GraphEdge::new(TENANT, &a.node_id, &b.node_id, EdgeType::Calls, "a.rs");
    store.insert_edge(&edge).await.unwrap();

    let pruned = store.prune_orphans(TENANT).await.unwrap();
    assert_eq!(pruned, 0);
}
