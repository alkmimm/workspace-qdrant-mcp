//! Tests for graph store CRUD operations: upsert nodes, insert edges, delete.

use super::*;

// -- REFERENCES resolution and drop (#369) --

/// The whole economics of the REFERENCES edge live here.
///
/// The extractor parses chunk TEXT, so it cannot tell a top-level provider from
/// a local variable and emits both. This is the step that separates them: a
/// reference that resolves to a file-backed symbol survives; one that never
/// resolves is deleted rather than left as a stub edge. Measured on DOC-V2 that
/// is 1,719 persisted edges instead of 51,199.
///
/// Asserting only the survivor would let a regression that keeps everything
/// pass, so both halves are pinned.
#[tokio::test]
async fn resolve_drops_unresolved_references_and_keeps_resolved_ones() {
    let store = test_store().await;

    let consumer = GraphNode::new(TENANT, "lib/home.dart", "build", NodeType::Method);
    // A real top-level binding, file-backed — the provider.
    let provider = GraphNode::new(
        TENANT,
        "lib/providers.dart",
        "activeContextProvider",
        NodeType::Constant,
    );
    store
        .upsert_nodes(&[consumer.clone(), provider.clone()])
        .await
        .unwrap();

    // Two extractor-shaped reference edges: one to a name that exists in the
    // tenant, one to a local variable that never will.
    let real_stub = GraphNode::stub(TENANT, "activeContextProvider", NodeType::Constant);
    let bogus_stub = GraphNode::stub(TENANT, "localCounter", NodeType::Constant);
    store
        .upsert_nodes(&[real_stub.clone(), bogus_stub.clone()])
        .await
        .unwrap();
    store
        .insert_edges(&[
            GraphEdge::new(
                TENANT,
                &consumer.node_id,
                &real_stub.node_id,
                EdgeType::References,
                "lib/home.dart",
            ),
            GraphEdge::new(
                TENANT,
                &consumer.node_id,
                &bogus_stub.node_id,
                EdgeType::References,
                "lib/home.dart",
            ),
        ])
        .await
        .unwrap();

    store.resolve_stub_edges(TENANT).await.unwrap();

    let surviving: Vec<(String,)> = sqlx::query_as(
        "SELECT target_node_id FROM graph_edges
         WHERE tenant_id = ?1 AND edge_type = 'REFERENCES'",
    )
    .bind(TENANT)
    .fetch_all(store.pool())
    .await
    .unwrap();

    assert_eq!(
        surviving.len(),
        1,
        "the unresolved reference must not persist"
    );
    assert_eq!(
        surviving[0].0, provider.node_id,
        "the surviving edge must point at the file-backed provider, not the stub"
    );

    // And the symbol is now reachable in reverse — which is what `usages` asks.
    let dangling: (i64,) =
        sqlx::query_as("SELECT COUNT(*) FROM graph_nodes WHERE tenant_id = ?1 AND file_path = ''")
            .bind(TENANT)
            .fetch_one(store.pool())
            .await
            .unwrap();
    assert_eq!(dangling.0, 0, "both stubs are collected once edgeless");
}

// -- Upsert node --

#[tokio::test]
async fn test_upsert_node_insert() {
    let store = test_store().await;
    let node = GraphNode::new(TENANT, "src/lib.rs", "Config", NodeType::Struct);
    store.upsert_node(&node).await.unwrap();

    // Verify via stats
    let stats = store.stats(Some(TENANT)).await.unwrap();
    assert_eq!(stats.total_nodes, 1);
}

#[tokio::test]
async fn test_upsert_node_update() {
    let store = test_store().await;

    // Insert stub first
    let stub = GraphNode::stub(TENANT, "Foo", NodeType::Struct);
    let stub_id = stub.node_id.clone();
    store.upsert_node(&stub).await.unwrap();

    // Upsert with full info -- same node_id because same (tenant, "", "Foo", struct)
    let mut full = GraphNode::stub(TENANT, "Foo", NodeType::Struct);
    full.file_path = "src/foo.rs".to_string();
    full.start_line = Some(10);
    full.end_line = Some(50);
    store.upsert_node(&full).await.unwrap();

    // Should still be one node
    let stats = store.stats(Some(TENANT)).await.unwrap();
    assert_eq!(stats.total_nodes, 1);

    // Verify file_path was updated (non-empty replaces empty)
    let row: (String,) = sqlx::query_as("SELECT file_path FROM graph_nodes WHERE node_id = ?1")
        .bind(&stub_id)
        .fetch_one(store.pool())
        .await
        .unwrap();
    assert_eq!(row.0, "src/foo.rs");
}

// -- Batch upsert nodes --

#[tokio::test]
async fn test_upsert_nodes_batch() {
    let store = test_store().await;
    let nodes = vec![
        GraphNode::new(TENANT, "a.rs", "alpha", NodeType::Function),
        GraphNode::new(TENANT, "b.rs", "beta", NodeType::Function),
        GraphNode::new(TENANT, "c.rs", "gamma", NodeType::Struct),
    ];
    store.upsert_nodes(&nodes).await.unwrap();

    let stats = store.stats(Some(TENANT)).await.unwrap();
    assert_eq!(stats.total_nodes, 3);
}

#[tokio::test]
async fn test_upsert_nodes_empty() {
    let store = test_store().await;
    store.upsert_nodes(&[]).await.unwrap(); // should not error
}

// -- Insert edge --

#[tokio::test]
async fn test_insert_edge() {
    let store = test_store().await;

    let caller = GraphNode::new(TENANT, "a.rs", "foo", NodeType::Function);
    let callee = GraphNode::new(TENANT, "b.rs", "bar", NodeType::Function);
    store
        .upsert_nodes(&[caller.clone(), callee.clone()])
        .await
        .unwrap();

    let edge = GraphEdge::new(
        TENANT,
        &caller.node_id,
        &callee.node_id,
        EdgeType::Calls,
        "a.rs",
    );
    store.insert_edge(&edge).await.unwrap();

    let stats = store.stats(Some(TENANT)).await.unwrap();
    assert_eq!(stats.total_edges, 1);
    assert_eq!(stats.edges_by_type.get("CALLS"), Some(&1));
}

#[tokio::test]
async fn test_insert_edge_duplicate_ignored() {
    let store = test_store().await;

    let a = GraphNode::new(TENANT, "a.rs", "x", NodeType::Function);
    let b = GraphNode::new(TENANT, "b.rs", "y", NodeType::Function);
    store.upsert_nodes(&[a.clone(), b.clone()]).await.unwrap();

    let edge = GraphEdge::new(TENANT, &a.node_id, &b.node_id, EdgeType::Calls, "a.rs");
    store.insert_edge(&edge).await.unwrap();
    store.insert_edge(&edge).await.unwrap(); // duplicate -- should not error

    let stats = store.stats(Some(TENANT)).await.unwrap();
    assert_eq!(stats.total_edges, 1);
}

// -- Batch insert edges --

#[tokio::test]
async fn test_insert_edges_batch() {
    let store = test_store().await;

    let a = GraphNode::new(TENANT, "a.rs", "a", NodeType::Function);
    let b = GraphNode::new(TENANT, "b.rs", "b", NodeType::Function);
    let c = GraphNode::new(TENANT, "c.rs", "c", NodeType::Function);
    store
        .upsert_nodes(&[a.clone(), b.clone(), c.clone()])
        .await
        .unwrap();

    let edges = vec![
        GraphEdge::new(TENANT, &a.node_id, &b.node_id, EdgeType::Calls, "a.rs"),
        GraphEdge::new(TENANT, &a.node_id, &c.node_id, EdgeType::Imports, "a.rs"),
    ];
    store.insert_edges(&edges).await.unwrap();

    let stats = store.stats(Some(TENANT)).await.unwrap();
    assert_eq!(stats.total_edges, 2);
}

// -- Delete edges by file --

#[tokio::test]
async fn test_delete_edges_by_file() {
    let store = test_store().await;

    let a = GraphNode::new(TENANT, "a.rs", "a", NodeType::Function);
    let b = GraphNode::new(TENANT, "b.rs", "b", NodeType::Function);
    let c = GraphNode::new(TENANT, "c.rs", "c", NodeType::Function);
    store
        .upsert_nodes(&[a.clone(), b.clone(), c.clone()])
        .await
        .unwrap();

    let edges = vec![
        GraphEdge::new(TENANT, &a.node_id, &b.node_id, EdgeType::Calls, "a.rs"),
        GraphEdge::new(TENANT, &a.node_id, &c.node_id, EdgeType::Imports, "a.rs"),
        GraphEdge::new(TENANT, &b.node_id, &c.node_id, EdgeType::Calls, "b.rs"),
    ];
    store.insert_edges(&edges).await.unwrap();

    // Delete edges from a.rs only
    let deleted = store.delete_edges_by_file(TENANT, "a.rs").await.unwrap();
    assert_eq!(deleted, 2);

    let stats = store.stats(Some(TENANT)).await.unwrap();
    assert_eq!(stats.total_edges, 1); // only the b->c edge remains
}

// -- Delete tenant --

#[tokio::test]
async fn test_delete_tenant() {
    let store = test_store().await;

    let a = GraphNode::new(TENANT, "a.rs", "a", NodeType::Function);
    let b = GraphNode::new(TENANT, "b.rs", "b", NodeType::Function);
    store.upsert_nodes(&[a.clone(), b.clone()]).await.unwrap();

    let edge = GraphEdge::new(TENANT, &a.node_id, &b.node_id, EdgeType::Calls, "a.rs");
    store.insert_edge(&edge).await.unwrap();

    let deleted = store.delete_tenant(TENANT).await.unwrap();
    assert_eq!(deleted, 3); // 1 edge + 2 nodes

    let stats = store.stats(Some(TENANT)).await.unwrap();
    assert_eq!(stats.total_nodes, 0);
    assert_eq!(stats.total_edges, 0);
}
