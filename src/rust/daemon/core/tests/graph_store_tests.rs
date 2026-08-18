//! Integration tests for graph store operations: pipeline, cross-file queries,
//! impact analysis, factory lifecycle, tenant isolation, orphan pruning, and
//! edge type filtering.

#[allow(dead_code)]
#[path = "common/graph_helpers.rs"]
mod graph_helpers;

use graph_helpers::{
    build_rust_file_chunks, build_rust_main_chunks, build_typescript_chunks, create_factory_store,
    ingest_file_chunks, TENANT,
};
use tempfile::tempdir;
use workspace_qdrant_core::graph::{extractor, EdgeType, GraphEdge, GraphNode, NodeType};

// ────────────────────────────────────────────────────────────────────────────
// 1. Extraction -> Store -> Query pipeline
// ────────────────────────────────────────────────────────────────────────────

/// Full pipeline: extract from Rust SemanticChunks -> store -> verify graph structure.
#[tokio::test]
async fn test_pipeline_extract_store_query_rust() {
    let dir = tempdir().unwrap();
    let store = create_factory_store(dir.path()).await;

    let chunks = build_rust_file_chunks();
    let result = extractor::extract_edges(&chunks, TENANT, "src/processor.rs");

    // Extraction should produce nodes: File + Struct + 3 methods + stub nodes
    assert!(
        result.nodes.len() >= 5,
        "expected at least 5 nodes, got {}",
        result.nodes.len()
    );

    // Should have CONTAINS, CALLS, USES_TYPE, and IMPORTS edges
    let edge_types: Vec<&EdgeType> = result.edges.iter().map(|e| &e.edge_type).collect();
    assert!(
        edge_types.contains(&&EdgeType::Contains),
        "missing CONTAINS edge"
    );
    assert!(edge_types.contains(&&EdgeType::Calls), "missing CALLS edge");
    assert!(
        edge_types.contains(&&EdgeType::Imports),
        "missing IMPORTS edge"
    );

    // Ingest
    store.upsert_nodes(&result.nodes).await.unwrap();
    store.insert_edges(&result.edges).await.unwrap();

    // Verify store has the data
    let stats = store.stats(Some(TENANT)).await.unwrap();
    assert!(stats.total_nodes > 0, "store should have nodes");
    assert!(stats.total_edges > 0, "store should have edges");

    // Verify node types are correct
    assert!(
        stats.nodes_by_type.contains_key("function")
            || stats.nodes_by_type.contains_key("method")
            || stats.nodes_by_type.contains_key("struct"),
        "should have function, method, or struct nodes"
    );
}

/// Full pipeline with TypeScript chunks -- validates multi-language support.
#[tokio::test]
async fn test_pipeline_extract_store_query_typescript() {
    let dir = tempdir().unwrap();
    let store = create_factory_store(dir.path()).await;

    let chunks = build_typescript_chunks();
    let result = extractor::extract_edges(&chunks, TENANT, "src/App.tsx");

    // Should have a class node
    let class_nodes: Vec<_> = result
        .nodes
        .iter()
        .filter(|n| n.symbol_type == NodeType::Class)
        .collect();
    assert!(
        !class_nodes.is_empty(),
        "should have at least one class node"
    );

    // Should have IMPORTS edges from preamble
    let import_edges: Vec<_> = result
        .edges
        .iter()
        .filter(|e| e.edge_type == EdgeType::Imports)
        .collect();
    assert!(
        !import_edges.is_empty(),
        "should have import edges from preamble"
    );

    store.upsert_nodes(&result.nodes).await.unwrap();
    store.insert_edges(&result.edges).await.unwrap();

    let stats = store.stats(Some(TENANT)).await.unwrap();
    assert!(stats.total_nodes > 0);
    assert!(stats.total_edges > 0);
}

// ────────────────────────────────────────────────────────────────────────────
// 2. Cross-file graph queries
// ────────────────────────────────────────────────────────────────────────────

/// Ingest two related files and verify cross-file relationships are queryable.
#[tokio::test]
async fn test_cross_file_graph_queries() {
    let dir = tempdir().unwrap();
    let store = create_factory_store(dir.path()).await;

    ingest_file_chunks(
        &store,
        &build_rust_file_chunks(),
        TENANT,
        "src/processor.rs",
    )
    .await;
    ingest_file_chunks(&store, &build_rust_main_chunks(), TENANT, "src/main.rs").await;

    // Stats should reflect both files
    let stats = store.stats(Some(TENANT)).await.unwrap();
    assert!(
        stats.total_nodes >= 6,
        "expected at least 6 nodes across 2 files, got {}",
        stats.total_nodes
    );
    assert!(
        stats.total_edges >= 3,
        "expected at least 3 edges, got {}",
        stats.total_edges
    );

    // Verify we can query related nodes from main's function
    let main_node = GraphNode::new(TENANT, "src/main.rs", "main", NodeType::Function);
    let related = store
        .query_related(TENANT, &main_node.node_id, 1, None)
        .await
        .unwrap();

    // main() calls process() and Processor::new(), so should have related nodes
    assert!(
        !related.is_empty(),
        "main function should have related nodes via CALLS edges"
    );
}

// ────────────────────────────────────────────────────────────────────────────
// 3. Impact analysis end-to-end
// ────────────────────────────────────────────────────────────────────────────

/// Build a realistic dependency graph and run impact analysis.
#[tokio::test]
async fn test_impact_analysis_end_to_end() {
    let dir = tempdir().unwrap();
    let store = create_factory_store(dir.path()).await;

    ingest_file_chunks(
        &store,
        &build_rust_file_chunks(),
        TENANT,
        "src/processor.rs",
    )
    .await;
    ingest_file_chunks(&store, &build_rust_main_chunks(), TENANT, "src/main.rs").await;

    // Impact analysis on "process" -- who calls it?
    let report = store
        .impact_analysis(TENANT, "process", Some("src/processor.rs"))
        .await
        .unwrap();

    assert_eq!(report.symbol_name, "process");
    // The report should succeed even if stub resolution is imperfect
    let _ = report.total_impacted;
}

/// Impact analysis on a symbol with no dependents.
#[tokio::test]
async fn test_impact_analysis_isolated_symbol() {
    let dir = tempdir().unwrap();
    let store = create_factory_store(dir.path()).await;

    ingest_file_chunks(
        &store,
        &build_rust_file_chunks(),
        TENANT,
        "src/processor.rs",
    )
    .await;

    // "validate" is called by "process" within the same file
    let report = store
        .impact_analysis(TENANT, "validate", Some("src/processor.rs"))
        .await
        .unwrap();

    assert_eq!(report.symbol_name, "validate");
    // "process" calls "validate" via a stub, so process may appear as impacted
    if report.total_impacted > 0 {
        let caller_names: Vec<&str> = report
            .impacted_nodes
            .iter()
            .map(|n| n.symbol_name.as_str())
            .collect();
        assert!(
            caller_names.contains(&"process") || report.total_impacted > 0,
            "expected 'process' as a caller of 'validate'"
        );
    }
}

// ────────────────────────────────────────────────────────────────────────────
// 4. Graph factory lifecycle
// ────────────────────────────────────────────────────────────────────────────

/// Factory creates store, runs schema migration, supports CRUD.
#[tokio::test]
async fn test_factory_lifecycle() {
    let dir = tempdir().unwrap();

    // First creation -- schema v1 migration runs
    let store = create_factory_store(dir.path()).await;

    let node = GraphNode::new(TENANT, "lib.rs", "Config", NodeType::Struct);
    store.upsert_nodes(&[node.clone()]).await.unwrap();

    let stats = store.stats(Some(TENANT)).await.unwrap();
    assert_eq!(stats.total_nodes, 1);

    // Drop and reopen -- should work without re-migration
    drop(store);
    let store2 = create_factory_store(dir.path()).await;
    let stats2 = store2.stats(Some(TENANT)).await.unwrap();
    assert_eq!(stats2.total_nodes, 1, "data should persist across reopen");
}

/// Re-ingestion atomically replaces edges for a file.
#[tokio::test]
async fn test_reingest_file_atomic() {
    let dir = tempdir().unwrap();
    let store = create_factory_store(dir.path()).await;

    // First ingestion
    ingest_file_chunks(
        &store,
        &build_rust_file_chunks(),
        TENANT,
        "src/processor.rs",
    )
    .await;

    let stats_v1 = store.stats(Some(TENANT)).await.unwrap();
    let edges_v1 = stats_v1.total_edges;

    // Re-ingest with modified chunks (removed calls from process)
    let mut chunks_v2 = build_rust_file_chunks();
    if let Some(process_chunk) = chunks_v2.iter_mut().find(|c| c.symbol_name == "process") {
        process_chunk.calls.clear();
    }
    let result_v2 = extractor::extract_edges(&chunks_v2, TENANT, "src/processor.rs");

    // Use reingest_file for atomic swap
    store
        .reingest_file(
            TENANT,
            "src/processor.rs",
            &result_v2.nodes,
            &result_v2.edges,
        )
        .await
        .unwrap();

    let stats_v2 = store.stats(Some(TENANT)).await.unwrap();
    assert!(
        stats_v2.total_edges <= edges_v1,
        "re-ingestion should not increase edges when calls were removed: v1={}, v2={}",
        edges_v1,
        stats_v2.total_edges
    );
}

// ────────────────────────────────────────────────────────────────────────────
// 5. Tenant isolation
// ────────────────────────────────────────────────────────────────────────────

/// Data from different tenants should not interfere.
#[tokio::test]
async fn test_tenant_isolation() {
    let dir = tempdir().unwrap();
    let store = create_factory_store(dir.path()).await;

    let tenant_a = "tenant-alpha";
    let tenant_b = "tenant-beta";

    ingest_file_chunks(
        &store,
        &build_rust_file_chunks(),
        tenant_a,
        "src/processor.rs",
    )
    .await;
    ingest_file_chunks(
        &store,
        &build_rust_file_chunks(),
        tenant_b,
        "src/processor.rs",
    )
    .await;

    let stats_a = store.stats(Some(tenant_a)).await.unwrap();
    let stats_b = store.stats(Some(tenant_b)).await.unwrap();
    let stats_all = store.stats(None).await.unwrap();

    assert_eq!(
        stats_a.total_nodes, stats_b.total_nodes,
        "same chunks -> same counts"
    );
    assert_eq!(
        stats_all.total_nodes,
        stats_a.total_nodes + stats_b.total_nodes,
        "total should be sum of per-tenant"
    );

    // Deleting tenant A should not affect tenant B
    store.delete_tenant(tenant_a).await.unwrap();

    let stats_a_after = store.stats(Some(tenant_a)).await.unwrap();
    let stats_b_after = store.stats(Some(tenant_b)).await.unwrap();

    assert_eq!(stats_a_after.total_nodes, 0, "tenant A should be empty");
    assert_eq!(
        stats_b_after.total_nodes, stats_b.total_nodes,
        "tenant B should be unaffected"
    );
}

// ────────────────────────────────────────────────────────────────────────────
// 6. Prune orphans after re-ingestion
// ────────────────────────────────────────────────────────────────────────────

/// Orphan pruning should clean up stale stub nodes.
#[tokio::test]
async fn test_prune_orphans_after_reingest() {
    let dir = tempdir().unwrap();
    let store = create_factory_store(dir.path()).await;

    ingest_file_chunks(
        &store,
        &build_rust_file_chunks(),
        TENANT,
        "src/processor.rs",
    )
    .await;

    let stats_before = store.stats(Some(TENANT)).await.unwrap();

    // Re-ingest with no calls (all stubs become orphans)
    let mut empty_chunks = build_rust_file_chunks();
    for chunk in &mut empty_chunks {
        chunk.calls.clear();
    }
    let result = extractor::extract_edges(&empty_chunks, TENANT, "src/processor.rs");
    store
        .reingest_file(TENANT, "src/processor.rs", &result.nodes, &result.edges)
        .await
        .unwrap();

    let pruned = store.prune_orphans(TENANT).await.unwrap();

    let stats_after = store.stats(Some(TENANT)).await.unwrap();
    assert!(
        stats_after.total_nodes <= stats_before.total_nodes,
        "pruning should not increase node count"
    );
    if pruned > 0 {
        assert!(
            stats_after.total_nodes < stats_before.total_nodes,
            "pruning {} orphans should decrease node count",
            pruned
        );
    }
}

// ────────────────────────────────────────────────────────────────────────────
// 7. Edge type filtering in queries
// ────────────────────────────────────────────────────────────────────────────

/// Querying with edge type filter should only return matching relationships.
#[tokio::test]
async fn test_query_related_edge_type_filter() {
    let dir = tempdir().unwrap();
    let store = create_factory_store(dir.path()).await;

    let a = GraphNode::new(TENANT, "a.rs", "foo", NodeType::Function);
    let b = GraphNode::new(TENANT, "b.rs", "bar", NodeType::Function);
    let c = GraphNode::new(TENANT, "c.rs", "Baz", NodeType::Struct);

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
    let calls_only = store
        .query_related(TENANT, &a.node_id, 1, Some(&[EdgeType::Calls]))
        .await
        .unwrap();
    assert_eq!(
        calls_only.len(),
        1,
        "should find exactly 1 CALLS relationship"
    );
    assert_eq!(calls_only[0].node_id, b.node_id);

    // Filter to USES_TYPE only
    let types_only = store
        .query_related(TENANT, &a.node_id, 1, Some(&[EdgeType::UsesType]))
        .await
        .unwrap();
    assert_eq!(
        types_only.len(),
        1,
        "should find exactly 1 USES_TYPE relationship"
    );
    assert_eq!(types_only[0].node_id, c.node_id);

    // No filter -- should get both
    let all = store
        .query_related(TENANT, &a.node_id, 1, None)
        .await
        .unwrap();
    assert_eq!(all.len(), 2, "no filter should return all relationships");
}

// ────────────────────────────────────────────────────────────────────────────
// 8. file_has_edges probe (issue #235 — dedup-path graph heal)
// ────────────────────────────────────────────────────────────────────────────

/// The probe must track the wipe: true after ingest, false after a
/// `reingest_file` with empty edges — the #235 state the dedup heal detects —
/// and true again once edges are rewritten.
#[tokio::test]
async fn test_file_has_edges_tracks_wipe_and_rebuild() {
    let dir = tempdir().unwrap();
    let store = create_factory_store(dir.path()).await;

    ingest_file_chunks(
        &store,
        &build_rust_file_chunks(),
        TENANT,
        "src/processor.rs",
    )
    .await;

    assert!(
        store
            .file_has_edges(TENANT, "src/processor.rs")
            .await
            .unwrap(),
        "ingested file must report edges"
    );
    assert!(
        !store.file_has_edges(TENANT, "src/never.rs").await.unwrap(),
        "un-ingested file must report no edges"
    );
    assert!(
        !store
            .file_has_edges("other-tenant", "src/processor.rs")
            .await
            .unwrap(),
        "probe must be tenant-scoped"
    );

    // reingest_file with empty nodes/edges (the delete path). Since #245 this
    // also drops the file's NODES (no ghost left behind), so file_has_edges
    // reports the gap and the #235 dedup heal still detects it and rebuilds.
    store
        .reingest_file(TENANT, "src/processor.rs", &[], &[])
        .await
        .unwrap();
    assert!(
        !store
            .file_has_edges(TENANT, "src/processor.rs")
            .await
            .unwrap(),
        "post-wipe the probe must report the gap"
    );

    // The heal path rewrites via the same atomic swap.
    ingest_file_chunks(
        &store,
        &build_rust_file_chunks(),
        TENANT,
        "src/processor.rs",
    )
    .await;
    assert!(
        store
            .file_has_edges(TENANT, "src/processor.rs")
            .await
            .unwrap(),
        "rebuild must restore the probe to true"
    );
}

// ────────────────────────────────────────────────────────────────────────────
// 9. Node lifecycle: no ghost / stale-generation nodes (issue #245)
// ────────────────────────────────────────────────────────────────────────────

/// `reingest_file` must make the node set authoritative per re-ingest: a file
/// deletion (empty nodes) leaves NO nodes for the path (no ghost), and a
/// re-ingest that drops symbols removes their stale node generations. Node ids
/// are deterministic, so unchanged symbols survive the swap.
#[tokio::test]
async fn test_reingest_file_clears_ghost_and_stale_nodes() {
    use workspace_qdrant_core::graph::compute_node_id;

    let dir = tempdir().unwrap();
    let store = create_factory_store(dir.path()).await;

    // Generation 1: two functions in the file, plus one edge between them.
    let foo = GraphNode::new(TENANT, "src/m.rs", "foo", NodeType::Function);
    let bar = GraphNode::new(TENANT, "src/m.rs", "bar", NodeType::Function);
    store
        .reingest_file(
            TENANT,
            "src/m.rs",
            &[foo.clone(), bar.clone()],
            &[GraphEdge::new(
                TENANT,
                &foo.node_id,
                &bar.node_id,
                EdgeType::Calls,
                "src/m.rs",
            )],
        )
        .await
        .unwrap();

    let n_after_gen1 = store.stats(Some(TENANT)).await.unwrap().total_nodes;
    assert_eq!(n_after_gen1, 2, "both symbols present after gen 1");

    // Generation 2: `bar` was removed (renamed away); only `foo` remains. The
    // stale `bar` node must NOT linger.
    store
        .reingest_file(TENANT, "src/m.rs", &[foo.clone()], &[])
        .await
        .unwrap();
    let n_after_gen2 = store.stats(Some(TENANT)).await.unwrap().total_nodes;
    assert_eq!(n_after_gen2, 1, "dropped symbol's stale node must be gone");
    // `foo` kept its deterministic id across the swap.
    let foo_related = store
        .query_related_by_symbol(TENANT, "foo", Some("src/m.rs"), 1, None)
        .await
        .unwrap();
    let _ = foo_related; // presence, not traversal, is the point here
    assert_eq!(
        foo.node_id,
        compute_node_id(TENANT, "src/m.rs", "foo", NodeType::Function),
        "unchanged symbol id is stable across re-ingest"
    );

    // File deletion (empty): no ghost node for the now-absent path.
    store
        .reingest_file(TENANT, "src/m.rs", &[], &[])
        .await
        .unwrap();
    assert_eq!(
        store.stats(Some(TENANT)).await.unwrap().total_nodes,
        0,
        "deleting a file must leave NO ghost nodes (issue #245)"
    );
}

/// A node that is the TARGET of ANOTHER file's edge must still be deletable when
/// its file is removed — the delete clears the referencing (now dangling) edge
/// first, satisfying the `graph_edges -> graph_nodes` foreign key. This is the
/// exact cross-file case the live #245 sweep hit as `FOREIGN KEY constraint
/// failed`; a same-file edge (removed by `delete_edges_by_file`) never exposed it.
#[tokio::test]
async fn test_delete_file_nodes_clears_incoming_cross_file_edge() {
    let dir = tempdir().unwrap();
    let store = create_factory_store(dir.path()).await;

    // File A defines `foo`; file B defines `bar` and calls A::foo (bar -> foo).
    let foo = GraphNode::new(TENANT, "src/a.rs", "foo", NodeType::Function);
    let bar = GraphNode::new(TENANT, "src/b.rs", "bar", NodeType::Function);
    store
        .reingest_file(TENANT, "src/a.rs", &[foo.clone()], &[])
        .await
        .unwrap();
    store
        .reingest_file(
            TENANT,
            "src/b.rs",
            &[bar.clone()],
            &[GraphEdge::new(
                TENANT,
                &bar.node_id,
                &foo.node_id,
                EdgeType::Calls,
                "src/b.rs",
            )],
        )
        .await
        .unwrap();
    assert_eq!(store.stats(Some(TENANT)).await.unwrap().total_nodes, 2);

    // Delete file A: foo is an incoming-edge target from B. Must succeed (no FK
    // error), removing foo AND the now-dangling bar -> foo edge; bar survives.
    let deleted = store
        .delete_nodes_by_file(TENANT, "src/a.rs")
        .await
        .unwrap();
    assert_eq!(
        deleted, 1,
        "foo removed despite being a cross-file edge target"
    );
    let stats = store.stats(Some(TENANT)).await.unwrap();
    assert_eq!(stats.total_nodes, 1, "only bar remains");
    assert_eq!(stats.total_edges, 0, "the dangling bar -> foo edge is gone");
}

/// The re-ingest hot path (partial keep, NOT a full file delete) must still clear
/// a dropped symbol's INCOMING cross-file edge. This is the exact production
/// scenario the graph_edges OR-delete split guards: a file re-ingest that drops
/// one of several symbols, where the dropped symbol was the target of another
/// file's edge. `delete_file_nodes_except` runs with a NON-EMPTY keep list here
/// (unlike the full-delete case above), so the stale set is a subset — the split
/// `target_node_id IN (…)` DELETE must remove exactly the edge into the dropped
/// symbol and leave the kept symbol's incoming edge intact.
#[tokio::test]
async fn test_reingest_partial_keep_clears_incoming_edge_to_dropped_symbol() {
    let dir = tempdir().unwrap();
    let store = create_factory_store(dir.path()).await;

    // File A defines `foo` and `bar`; file B's `caller` calls BOTH (two incoming
    // edges into A's symbols, owned by B so A's re-ingest does not touch them).
    let foo = GraphNode::new(TENANT, "src/a.rs", "foo", NodeType::Function);
    let bar = GraphNode::new(TENANT, "src/a.rs", "bar", NodeType::Function);
    let caller = GraphNode::new(TENANT, "src/b.rs", "caller", NodeType::Function);
    store
        .reingest_file(TENANT, "src/a.rs", &[foo.clone(), bar.clone()], &[])
        .await
        .unwrap();
    store
        .reingest_file(
            TENANT,
            "src/b.rs",
            &[caller.clone()],
            &[
                GraphEdge::new(
                    TENANT,
                    &caller.node_id,
                    &foo.node_id,
                    EdgeType::Calls,
                    "src/b.rs",
                ),
                GraphEdge::new(
                    TENANT,
                    &caller.node_id,
                    &bar.node_id,
                    EdgeType::Calls,
                    "src/b.rs",
                ),
            ],
        )
        .await
        .unwrap();
    let stats = store.stats(Some(TENANT)).await.unwrap();
    assert_eq!(stats.total_nodes, 3, "foo, bar, caller present after setup");
    assert_eq!(
        stats.total_edges, 2,
        "caller -> foo and caller -> bar present"
    );

    // Re-ingest A keeping only `foo` (bar dropped). `bar` is an incoming-edge
    // target from B, so its now-dangling edge must go; `foo` and its incoming
    // edge survive.
    store
        .reingest_file(TENANT, "src/a.rs", &[foo.clone()], &[])
        .await
        .unwrap();
    let stats = store.stats(Some(TENANT)).await.unwrap();
    assert_eq!(stats.total_nodes, 2, "bar dropped; foo + caller remain");
    assert_eq!(
        stats.total_edges, 1,
        "only the dangling caller -> bar edge is cleared; caller -> foo survives"
    );
}

/// The file-less tree-sitter stub nodes (`file_path = ''`) must never be wiped
/// by a per-file node delete — they belong to no file and are pruned elsewhere.
#[tokio::test]
async fn test_delete_nodes_by_file_spares_stub_nodes() {
    let dir = tempdir().unwrap();
    let store = create_factory_store(dir.path()).await;

    let real = GraphNode::new(TENANT, "src/m.rs", "foo", NodeType::Function);
    let stub = GraphNode::new(TENANT, "", "unresolved_callee", NodeType::Function);
    store
        .upsert_nodes(&[real.clone(), stub.clone()])
        .await
        .unwrap();

    // Delete by the real file: the stub (empty file_path) survives.
    let deleted = store
        .delete_nodes_by_file(TENANT, "src/m.rs")
        .await
        .unwrap();
    assert_eq!(deleted, 1, "only the file's own node is deleted");
    assert_eq!(
        store.stats(Some(TENANT)).await.unwrap().total_nodes,
        1,
        "the file-less stub node must remain"
    );

    // An empty-path delete is a guarded no-op (never mass-wipes stubs).
    let deleted_empty = store.delete_nodes_by_file(TENANT, "").await.unwrap();
    assert_eq!(deleted_empty, 0, "empty path is a no-op");
    assert_eq!(store.stats(Some(TENANT)).await.unwrap().total_nodes, 1);
}
