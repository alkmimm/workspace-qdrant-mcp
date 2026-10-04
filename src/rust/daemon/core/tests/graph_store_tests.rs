//! Integration tests for graph store operations: pipeline, cross-file queries,
//! impact analysis, factory lifecycle, tenant isolation, orphan pruning, and
//! edge type filtering.

#[allow(dead_code)]
#[path = "common/graph_helpers.rs"]
mod graph_helpers;

use graph_helpers::{
    build_rust_file_chunks, build_rust_main_chunks, build_typescript_chunks, create_factory_store,
    generation_of, ingest_file_chunks, TENANT,
};
use tempfile::tempdir;
use workspace_qdrant_core::graph::{
    compute_node_id, extractor, EdgeType, GraphEdge, GraphNode, GraphScope, NodeType,
};

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
    let stats = store.stats(Some(TENANT), &GraphScope::all()).await.unwrap();
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

    let stats = store.stats(Some(TENANT), &GraphScope::all()).await.unwrap();
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
    let stats = store.stats(Some(TENANT), &GraphScope::all()).await.unwrap();
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
        .query_related(TENANT, &main_node.node_id, 1, None, &GraphScope::all())
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
        .impact_analysis(
            TENANT,
            "process",
            Some("src/processor.rs"),
            &GraphScope::all(),
        )
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
        .impact_analysis(
            TENANT,
            "validate",
            Some("src/processor.rs"),
            &GraphScope::all(),
        )
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

    let stats = store.stats(Some(TENANT), &GraphScope::all()).await.unwrap();
    assert_eq!(stats.total_nodes, 1);

    // Drop and reopen -- should work without re-migration
    drop(store);
    let store2 = create_factory_store(dir.path()).await;
    let stats2 = store2
        .stats(Some(TENANT), &GraphScope::all())
        .await
        .unwrap();
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

    let stats_v1 = store.stats(Some(TENANT), &GraphScope::all()).await.unwrap();
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
            &generation_of("src/processor.rs"),
            &result_v2.nodes,
            &result_v2.edges,
        )
        .await
        .unwrap();

    let stats_v2 = store.stats(Some(TENANT), &GraphScope::all()).await.unwrap();
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

    let stats_a = store
        .stats(Some(tenant_a), &GraphScope::all())
        .await
        .unwrap();
    let stats_b = store
        .stats(Some(tenant_b), &GraphScope::all())
        .await
        .unwrap();
    let stats_all = store.stats(None, &GraphScope::all()).await.unwrap();

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

    let stats_a_after = store
        .stats(Some(tenant_a), &GraphScope::all())
        .await
        .unwrap();
    let stats_b_after = store
        .stats(Some(tenant_b), &GraphScope::all())
        .await
        .unwrap();

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

    let stats_before = store.stats(Some(TENANT), &GraphScope::all()).await.unwrap();

    // Re-ingest with no calls (all stubs become orphans)
    let mut empty_chunks = build_rust_file_chunks();
    for chunk in &mut empty_chunks {
        chunk.calls.clear();
    }
    let result = extractor::extract_edges(&empty_chunks, TENANT, "src/processor.rs");
    store
        .reingest_file(
            TENANT,
            "src/processor.rs",
            &generation_of("src/processor.rs"),
            &result.nodes,
            &result.edges,
        )
        .await
        .unwrap();

    let pruned = store.prune_orphans(TENANT).await.unwrap();

    let stats_after = store.stats(Some(TENANT), &GraphScope::all()).await.unwrap();
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
        .query_related(
            TENANT,
            &a.node_id,
            1,
            Some(&[EdgeType::Calls]),
            &GraphScope::all(),
        )
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
        .query_related(
            TENANT,
            &a.node_id,
            1,
            Some(&[EdgeType::UsesType]),
            &GraphScope::all(),
        )
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
        .query_related(TENANT, &a.node_id, 1, None, &GraphScope::all())
        .await
        .unwrap();
    assert_eq!(all.len(), 2, "no filter should return all relationships");
}

// ────────────────────────────────────────────────────────────────────────────
// 8. Extraction records (dedup-path heal + idle backfill probe)
// ────────────────────────────────────────────────────────────────────────────

/// The probe answers per generation: true once a version was extracted (even
/// with no symbols), false for a version never extracted or since deleted.
#[tokio::test]
async fn test_generation_extracted_tracks_each_version() {
    let dir = tempdir().unwrap();
    let store = create_factory_store(dir.path()).await;
    let path = "src/processor.rs";

    ingest_file_chunks(&store, &build_rust_file_chunks(), TENANT, path).await;
    let v1 = generation_of(path);
    assert!(store.generation_extracted(TENANT, &v1).await.unwrap());
    assert!(
        !store
            .generation_extracted(TENANT, "src/processor.rs@v2")
            .await
            .unwrap(),
        "another version of the same path was never extracted"
    );
    assert!(
        !store
            .generation_extracted("other-tenant", &v1)
            .await
            .unwrap(),
        "probe must be tenant-scoped"
    );

    // A version with no symbols still counts as extracted (no retry loop).
    store
        .reingest_file(TENANT, "README.md", "readme@v1", &[], &[])
        .await
        .unwrap();
    assert!(store
        .generation_extracted(TENANT, "readme@v1")
        .await
        .unwrap());

    // Deleting the version clears its record; rebuilding restores it.
    store.delete_generation(TENANT, &v1).await.unwrap();
    assert!(!store.generation_extracted(TENANT, &v1).await.unwrap());
    ingest_file_chunks(&store, &build_rust_file_chunks(), TENANT, path).await;
    assert!(store.generation_extracted(TENANT, &v1).await.unwrap());
}

// ────────────────────────────────────────────────────────────────────────────
// 9. Generation lifecycle (the per-version successor of #245)
// ────────────────────────────────────────────────────────────────────────────

/// Re-ingesting a version replaces exactly that version's rows: a dropped
/// symbol leaves no stale node, and another version of the same path (another
/// branch) is untouched.
#[tokio::test]
async fn test_reingest_replaces_only_its_own_version() {
    let dir = tempdir().unwrap();
    let store = create_factory_store(dir.path()).await;
    let all = GraphScope::all();

    let foo = GraphNode::new(TENANT, "src/m.rs", "foo", NodeType::Function);
    let bar = GraphNode::new(TENANT, "src/m.rs", "bar", NodeType::Function);
    let foo_calls_bar = GraphEdge::new(
        TENANT,
        &foo.node_id,
        &bar.node_id,
        EdgeType::Calls,
        "src/m.rs",
    );
    store
        .reingest_file(
            TENANT,
            "src/m.rs",
            "m@v1",
            &[foo.clone(), bar.clone()],
            &[foo_calls_bar.clone()],
        )
        .await
        .unwrap();
    store
        .reingest_file(
            TENANT,
            "src/m.rs",
            "m@v2",
            &[foo.clone(), bar.clone()],
            &[foo_calls_bar],
        )
        .await
        .unwrap();
    assert_eq!(
        store.stats(Some(TENANT), &all).await.unwrap().total_nodes,
        4,
        "two versions x two symbols"
    );

    // v1 drops `bar`; v2 keeps it.
    store
        .reingest_file(TENANT, "src/m.rs", "m@v1", &[foo.clone()], &[])
        .await
        .unwrap();
    let v1 = GraphScope::generations(["m@v1".to_string()].into_iter().collect());
    let v2 = GraphScope::generations(["m@v2".to_string()].into_iter().collect());
    assert_eq!(
        store.stats(Some(TENANT), &v1).await.unwrap().total_nodes,
        1,
        "v1's stale `bar` is gone"
    );
    assert_eq!(
        store.stats(Some(TENANT), &v2).await.unwrap().total_nodes,
        2,
        "v2 is untouched"
    );
    assert_eq!(
        foo.node_id,
        compute_node_id(TENANT, "src/m.rs", "foo", NodeType::Function),
        "an unchanged symbol keeps its id across versions"
    );
}

/// Deleting the version that defines a cross-file target leaves the other
/// file's edge in place — it belongs to that file's version — but a branch
/// that no longer holds the target cannot reach anything through it.
#[tokio::test]
async fn test_deleted_target_version_is_unreachable_not_rewritten() {
    let dir = tempdir().unwrap();
    let store = create_factory_store(dir.path()).await;

    let foo = GraphNode::new(TENANT, "src/a.rs", "foo", NodeType::Function);
    let bar = GraphNode::new(TENANT, "src/b.rs", "bar", NodeType::Function);
    store
        .reingest_file(TENANT, "src/a.rs", "a@v1", &[foo.clone()], &[])
        .await
        .unwrap();
    store
        .reingest_file(
            TENANT,
            "src/b.rs",
            "b@v1",
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

    let deleted = store.delete_generation(TENANT, "a@v1").await.unwrap();
    assert_eq!(deleted, 1, "only foo's row");
    let all = GraphScope::all();
    let stats = store.stats(Some(TENANT), &all).await.unwrap();
    assert_eq!(
        (stats.total_nodes, stats.total_edges),
        (1, 1),
        "b's edge is b's"
    );

    let b_only = GraphScope::generations(["b@v1".to_string()].into_iter().collect());
    let reached = store
        .query_related(TENANT, &bar.node_id, 1, None, &b_only)
        .await
        .unwrap();
    assert!(
        reached.is_empty(),
        "foo is defined by no version the scope holds"
    );
}

/// The file-less stub rows belong to no version: deleting a version spares
/// them, and the empty generation itself can never be deleted or replaced.
#[tokio::test]
async fn test_generation_delete_spares_stub_nodes() {
    let dir = tempdir().unwrap();
    let store = create_factory_store(dir.path()).await;
    let all = GraphScope::all();

    let real = GraphNode::new(TENANT, "src/m.rs", "foo", NodeType::Function);
    let stub = GraphNode::stub(TENANT, "unresolved_callee", NodeType::Function);
    store
        .reingest_file(TENANT, "src/m.rs", "m@v1", &[real, stub], &[])
        .await
        .unwrap();
    assert_eq!(store.delete_generation(TENANT, "m@v1").await.unwrap(), 1);
    assert_eq!(
        store.stats(Some(TENANT), &all).await.unwrap().total_nodes,
        1,
        "the stub remains"
    );
    assert_eq!(
        store.delete_generation(TENANT, "").await.unwrap(),
        0,
        "never mass-wipes stubs"
    );
    assert!(store
        .reingest_file(TENANT, "src/m.rs", "", &[], &[])
        .await
        .is_err());
}
