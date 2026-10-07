//! `impact` depth and the cuts it reports (field feedback 2026-10-07: with
//! `maxHops: 1` the answer still held distances 1–3, and a pinned `filePath`
//! dropped ambiguous callers without saying so).

use tonic::Request;
use workspace_qdrant_core::graph::{
    create_sqlite_graph_store, EdgeType, GraphEdge, GraphNode, NodeType,
};

use crate::proto::graph_service_server::GraphService;
use crate::proto::{ImpactAnalysisRequest, ImpactAnalysisResponse};
use crate::services::GraphServiceImpl;

const TENANT: &str = "abcd12345678";

fn function(file: &str, name: &str) -> GraphNode {
    GraphNode::new(TENANT, file, name, NodeType::Function)
}

fn calls(from: &GraphNode, to: &GraphNode, weight: f64) -> GraphEdge {
    let mut edge = GraphEdge::new(
        TENANT,
        &from.node_id,
        &to.node_id,
        EdgeType::Calls,
        &from.file_path,
    );
    edge.weight = weight;
    edge
}

/// `target` in t.rs and its callers:
///   c4 -> c3 -> c2 -> c1 -> target        (a chain, distances 1–4)
///   guess -(0.5)-> target                  (one of 2 same-name candidates)
///   mixed -(0.5)-> target, mixed -> c1     (weak direct, strong via c1)
///   Batch.flush -> target                  (a class member)
async fn service() -> (GraphServiceImpl, tempfile::TempDir) {
    let tmp = tempfile::tempdir().unwrap();
    let store = create_sqlite_graph_store(tmp.path()).await.unwrap();
    let target = function("t.rs", "target");
    let chain: Vec<GraphNode> = (1..=4)
        .map(|i| function(&format!("c{i}.rs"), &format!("c{i}")))
        .collect();
    let guess = function("g.rs", "guess");
    let mixed = function("m.rs", "mixed");
    let flush = GraphNode::member(TENANT, "b.rs", "flush", Some("Batch"), NodeType::Method);
    let mut nodes = vec![target.clone(), guess.clone(), mixed.clone(), flush.clone()];
    nodes.extend(chain.iter().cloned());
    store.upsert_nodes(&nodes).await.unwrap();

    let mut edges = vec![
        calls(&chain[0], &target, 1.0),
        calls(&guess, &target, 0.5),
        calls(&mixed, &target, 0.5),
        calls(&mixed, &chain[0], 1.0),
        calls(&flush, &target, 1.0),
    ];
    for pair in chain.windows(2) {
        edges.push(calls(&pair[1], &pair[0], 1.0));
    }
    store.insert_edges(&edges).await.unwrap();
    (GraphServiceImpl::new(store), tmp)
}

async fn impact(
    svc: &GraphServiceImpl,
    file_path: Option<&str>,
    max_hops: Option<u32>,
) -> ImpactAnalysisResponse {
    svc.impact_analysis(Request::new(ImpactAnalysisRequest {
        tenant_id: TENANT.into(),
        symbol_name: "target".into(),
        file_path: file_path.map(Into::into),
        top_k: None,
        min_confidence: None,
        branch: None,
        max_hops,
    }))
    .await
    .unwrap()
    .into_inner()
}

fn deepest(resp: &ImpactAnalysisResponse) -> u32 {
    resp.impacted_nodes
        .iter()
        .map(|n| n.distance)
        .max()
        .unwrap_or(0)
}

#[tokio::test]
async fn impact_walks_the_requested_depth_and_echoes_it() {
    let (svc, _tmp) = service().await;

    let one = impact(&svc, None, Some(1)).await;
    assert_eq!(one.max_hops, 1);
    assert_eq!(deepest(&one), 1, "maxHops 1 must stop at direct callers");
    let names: Vec<&str> = one
        .impacted_nodes
        .iter()
        .map(|n| n.symbol_name.as_str())
        .collect();
    assert!(names.contains(&"c1") && !names.contains(&"c2"), "{names:?}");

    let default = impact(&svc, None, None).await;
    assert_eq!(default.max_hops, 3, "absent = the documented default");
    assert_eq!(deepest(&default), 3);
    assert_eq!(impact(&svc, None, Some(0)).await.max_hops, 3, "0 = absent");

    let five = impact(&svc, None, Some(5)).await;
    assert_eq!(deepest(&five), 4, "c4 sits 4 hops up");

    let capped = impact(&svc, None, Some(99)).await;
    assert_eq!(
        capped.max_hops, 5,
        "above the ceiling is capped, and says so"
    );
}

#[tokio::test]
async fn a_pinned_file_path_reports_the_ambiguous_callers_it_drops() {
    let (svc, _tmp) = service().await;

    let pinned = impact(&svc, Some("t.rs"), None).await;
    let names: Vec<&str> = pinned
        .impacted_nodes
        .iter()
        .map(|n| n.symbol_name.as_str())
        .collect();
    assert!(!names.contains(&"guess"), "{names:?}");
    assert!(
        names.contains(&"mixed"),
        "mixed reaches target through c1 at full confidence: {names:?}"
    );
    assert_eq!(
        pinned.dropped_below_confidence_floor, 1,
        "only `guess` is reached by nothing but an ambiguous edge"
    );
    assert!(!pinned.node_budget_reached);

    let broad = impact(&svc, None, None).await;
    assert!(broad
        .impacted_nodes
        .iter()
        .any(|n| n.symbol_name == "guess"));
    assert_eq!(
        broad.dropped_below_confidence_floor, 0,
        "without a file_path no floor applies"
    );
}

#[tokio::test]
async fn an_impacted_member_names_its_class() {
    let (svc, _tmp) = service().await;
    let resp = impact(&svc, None, Some(1)).await;
    let flush = resp
        .impacted_nodes
        .iter()
        .find(|n| n.symbol_name == "flush")
        .expect("Batch.flush calls target");
    assert_eq!(flush.parent_symbol.as_deref(), Some("Batch"));
    let c1 = resp
        .impacted_nodes
        .iter()
        .find(|n| n.symbol_name == "c1")
        .unwrap();
    assert_eq!(c1.parent_symbol, None);
}
