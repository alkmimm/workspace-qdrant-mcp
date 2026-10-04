//! Per-generation writes and branch-scoped reads.
//!
//! The defect these pin: the graph held ONE version of each file — whichever
//! branch was extracted last — so a branch's `relations`/`impact`/`hotspots`
//! answered with another branch's symbols, and a file a branch deleted stayed
//! in its answers (emnify-sms-sender, 2026-10-03: `endpoint.ts`/`Message.ts`,
//! deleted on the phase-5 branch, still carried 14 nodes).

use std::collections::HashSet;

use super::*;

/// develop's and fase-5's versions of `page.tsx` call different helpers.
const DEV: &str = "page-dev";
const F5: &str = "page-f5";

fn scope(gens: &[&str]) -> GraphScope {
    GraphScope::generations(gens.iter().map(|g| g.to_string()).collect())
}

fn calls(caller: &GraphNode, callee: &GraphNode, file: &str) -> GraphEdge {
    GraphEdge::new(
        TENANT,
        &caller.node_id,
        &callee.node_id,
        EdgeType::Calls,
        file,
    )
}

/// `page.tsx` in two versions plus a helper file per branch:
///   develop: page.render -> legacy.listCommands   (legacy.ts only on develop)
///   fase-5:  page.render -> actions.listMessages  (actions.ts only on fase-5)
async fn two_branch_store() -> (SharedGraphStore<SqliteGraphStore>, GraphNode) {
    let store = SharedGraphStore::new(test_store().await);
    let render = GraphNode::new(TENANT, "page.tsx", "render", NodeType::Function);
    let legacy = GraphNode::new(TENANT, "legacy.ts", "listCommands", NodeType::Function);
    let modern = GraphNode::new(TENANT, "actions.ts", "listMessages", NodeType::Function);
    store
        .reingest_file(
            TENANT,
            "page.tsx",
            DEV,
            &[render.clone()],
            &[calls(&render, &legacy, "page.tsx")],
        )
        .await
        .unwrap();
    store
        .reingest_file(
            TENANT,
            "page.tsx",
            F5,
            &[render.clone()],
            &[calls(&render, &modern, "page.tsx")],
        )
        .await
        .unwrap();
    store
        .reingest_file(TENANT, "legacy.ts", "legacy-dev", &[legacy], &[])
        .await
        .unwrap();
    store
        .reingest_file(TENANT, "actions.ts", "actions-f5", &[modern], &[])
        .await
        .unwrap();
    (store, render)
}

fn names(nodes: &[TraversalNode]) -> HashSet<String> {
    nodes.iter().map(|n| n.symbol_name.clone()).collect()
}

#[tokio::test]
async fn each_branch_sees_only_its_own_version_of_a_file() {
    let (store, render) = two_branch_store().await;

    let dev = store
        .query_related(
            TENANT,
            &render.node_id,
            1,
            None,
            &scope(&[DEV, "legacy-dev"]),
        )
        .await
        .unwrap();
    assert_eq!(names(&dev), HashSet::from(["listCommands".to_string()]));

    let f5 = store
        .query_related(
            TENANT,
            &render.node_id,
            1,
            None,
            &scope(&[F5, "actions-f5"]),
        )
        .await
        .unwrap();
    assert_eq!(names(&f5), HashSet::from(["listMessages".to_string()]));

    // Unscoped (`*`) still sees both — the old mixed view, on request only.
    let all = store
        .query_related(TENANT, &render.node_id, 1, None, &GraphScope::all())
        .await
        .unwrap();
    assert_eq!(all.len(), 2);
}

#[tokio::test]
async fn a_symbol_the_branch_does_not_define_is_not_reported() {
    let (store, render) = two_branch_store().await;
    // fase-5's version of page.tsx calls listMessages, but suppose the branch
    // deleted actions.ts: its generation is not in the scope. The edge is
    // admitted (page.tsx is fase-5's), its target has no admitted row.
    let deleted_target = store
        .query_related(TENANT, &render.node_id, 1, None, &scope(&[F5]))
        .await
        .unwrap();
    assert!(
        deleted_target.is_empty(),
        "a node only another version defines must not be listed: {deleted_target:?}"
    );
}

#[tokio::test]
async fn impact_counts_callers_from_the_branch_only() {
    let (store, _) = two_branch_store().await;
    let on_dev = store
        .impact_analysis(TENANT, "listCommands", None, &scope(&[DEV, "legacy-dev"]))
        .await
        .unwrap();
    assert_eq!(on_dev.total_impacted, 1, "develop's page.render calls it");

    // fase-5 holds neither legacy.ts nor develop's page.tsx.
    let on_f5 = store
        .impact_analysis(TENANT, "listCommands", None, &scope(&[F5, "actions-f5"]))
        .await
        .unwrap();
    assert_eq!(on_f5.total_impacted, 0);
}

#[tokio::test]
async fn stats_sum_only_the_versions_the_branch_holds() {
    let (store, _) = two_branch_store().await;
    let dev = store
        .stats(Some(TENANT), &scope(&[DEV, "legacy-dev"]))
        .await
        .unwrap();
    assert_eq!(dev.total_nodes, 2, "render + listCommands");
    assert_eq!(dev.total_edges, 1);
    let all = store.stats(Some(TENANT), &GraphScope::all()).await.unwrap();
    assert_eq!(
        all.total_nodes, 4,
        "render twice (one row per version) + two helpers"
    );
    assert_eq!(all.total_edges, 2);
}

#[tokio::test]
async fn replacing_a_generation_leaves_the_other_versions_untouched() {
    let (store, render) = two_branch_store().await;
    // fase-5's page.tsx is edited again: same generation key replaced.
    let other = GraphNode::new(TENANT, "x.ts", "other", NodeType::Function);
    store
        .reingest_file(
            TENANT,
            "page.tsx",
            F5,
            &[render.clone()],
            &[calls(&render, &other, "page.tsx")],
        )
        .await
        .unwrap();
    let dev = store
        .query_related(
            TENANT,
            &render.node_id,
            1,
            None,
            &scope(&[DEV, "legacy-dev"]),
        )
        .await
        .unwrap();
    assert_eq!(names(&dev), HashSet::from(["listCommands".to_string()]));
    let all = store.stats(Some(TENANT), &GraphScope::all()).await.unwrap();
    assert_eq!(all.total_edges, 2, "the replaced generation kept one edge");
}

#[tokio::test]
async fn deleting_a_generation_removes_only_its_rows() {
    let (store, render) = two_branch_store().await;
    let stub = GraphNode::stub(TENANT, "println", NodeType::Function);
    store.upsert_nodes(&[stub.clone()]).await.unwrap();

    let deleted = store.delete_generation(TENANT, F5).await.unwrap();
    assert_eq!(deleted, 2, "fase-5's render node and its one edge");
    assert!(!store.generation_extracted(TENANT, F5).await.unwrap());
    assert!(store.generation_extracted(TENANT, DEV).await.unwrap());

    let dev = store
        .query_related(
            TENANT,
            &render.node_id,
            1,
            None,
            &scope(&[DEV, "legacy-dev"]),
        )
        .await
        .unwrap();
    assert_eq!(dev.len(), 1, "develop's version survives");
    let stubs = store.stats(Some(TENANT), &scope(&[])).await.unwrap();
    assert_eq!(
        stubs.total_nodes, 1,
        "the generation-less stub row survives"
    );
    // Deleting the shared stub namespace is refused.
    assert_eq!(store.delete_generation(TENANT, "").await.unwrap(), 0);
}

#[tokio::test]
async fn stamping_owns_the_files_nodes_and_leaves_foreign_ones_shared() {
    let own = GraphNode::new(TENANT, "a.rs", "f", NodeType::Function);
    let stub = GraphNode::stub(TENANT, "g", NodeType::Function);
    let foreign = GraphNode::new(TENANT, "b.rs", "h", NodeType::Function);
    let edge = calls(&own, &stub, "a.rs");
    let (nodes, edges) =
        crate::graph::shared::stamp_generation("a.rs", "a1", &[own, stub, foreign], &[edge]);
    let by_name: std::collections::HashMap<&str, &str> = nodes
        .iter()
        .map(|n| (n.symbol_name.as_str(), n.generation.as_str()))
        .collect();
    assert_eq!(by_name["f"], "a1");
    assert_eq!(by_name["g"], "", "a stub belongs to no version");
    assert_eq!(
        by_name["h"], "",
        "another file's node is defined by that file"
    );
    assert_eq!(edges[0].generation, "a1");
}

#[tokio::test]
async fn an_empty_extraction_is_recorded_and_an_empty_generation_refused() {
    let store = SharedGraphStore::new(test_store().await);
    store
        .reingest_file(TENANT, "README.md", "readme-1", &[], &[])
        .await
        .unwrap();
    assert!(store
        .generation_extracted(TENANT, "readme-1")
        .await
        .unwrap());
    let listed = store.extracted_generations(TENANT).await.unwrap();
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].file_path, "README.md");

    let refused = store.reingest_file(TENANT, "a.rs", "", &[], &[]).await;
    assert!(
        refused.is_err(),
        "the empty generation is the stub namespace"
    );
}

#[tokio::test]
async fn delete_tenant_clears_the_extraction_records_too() {
    let (store, _) = two_branch_store().await;
    store.delete_tenant(TENANT).await.unwrap();
    assert!(store
        .extracted_generations(TENANT)
        .await
        .unwrap()
        .is_empty());
}

#[tokio::test]
async fn a_name_defined_once_per_branch_resolves_uniquely_on_each() {
    // `save` moved: repo.ts defines it on develop, store.ts on fase-5. Across
    // the whole tenant that is two candidates (an ambiguous 1/N fan-out the
    // centrality gate drops); per branch it is one, and the resolver must see
    // it that way.
    let store = SharedGraphStore::new(test_store().await);
    let caller = GraphNode::new(TENANT, "app.ts", "run", NodeType::Function);
    let stub = GraphNode::stub(TENANT, "save", NodeType::Function);
    let on_dev = GraphNode::new(TENANT, "repo.ts", "save", NodeType::Function);
    let on_f5 = GraphNode::new(TENANT, "store.ts", "save", NodeType::Function);
    store
        .reingest_file(
            TENANT,
            "app.ts",
            "app-dev",
            &[caller.clone(), stub.clone()],
            &[calls(&caller, &stub, "app.ts")],
        )
        .await
        .unwrap();
    store
        .reingest_file(TENANT, "repo.ts", "repo-dev", &[on_dev.clone()], &[])
        .await
        .unwrap();
    store
        .reingest_file(TENANT, "store.ts", "store-f5", &[on_f5.clone()], &[])
        .await
        .unwrap();
    let membership = GenerationBranches::from_rows([
        ("app-dev".to_string(), vec!["develop".to_string()]),
        ("repo-dev".to_string(), vec!["develop".to_string()]),
        ("store-f5".to_string(), vec!["fase-5".to_string()]),
    ]);
    assert_eq!(
        store.resolve_stub_edges(TENANT, &membership).await.unwrap(),
        1
    );

    let dev = store
        .query_related(
            TENANT,
            &caller.node_id,
            1,
            Some(&[EdgeType::Calls]),
            &scope(&["app-dev", "repo-dev"]),
        )
        .await
        .unwrap();
    assert_eq!(dev.len(), 1);
    assert_eq!(dev[0].node_id, on_dev.node_id);
    assert!(
        (dev[0].confidence - 0.7).abs() < 1e-9,
        "unique on the branch, not a 1/2 fan-out: {}",
        dev[0].confidence
    );
    // fase-5's store.ts was never a candidate for develop's app.ts.
    let all = store
        .query_related(
            TENANT,
            &caller.node_id,
            1,
            Some(&[EdgeType::Calls]),
            &GraphScope::all(),
        )
        .await
        .unwrap();
    assert!(!all.iter().any(|n| n.node_id == on_f5.node_id));
}

#[tokio::test]
async fn lsp_supersession_touches_only_its_generation() {
    let store = SharedGraphStore::new(test_store().await);
    let caller = GraphNode::new(TENANT, "a.rs", "caller", NodeType::Function);
    let s1 = GraphNode::new(TENANT, "p1/repo.rs", "save", NodeType::Function);
    let s2 = GraphNode::new(TENANT, "p2/repo.rs", "save", NodeType::Function);
    store.upsert_nodes(&[s1.clone(), s2.clone()]).await.unwrap();
    for generation in ["a-dev", "a-f5"] {
        store
            .reingest_file(
                TENANT,
                "a.rs",
                generation,
                &[caller.clone()],
                &[calls(&caller, &s1, "a.rs"), calls(&caller, &s2, "a.rs")],
            )
            .await
            .unwrap();
    }
    let deleted = store
        .make_calls_authoritative(
            TENANT,
            &caller.node_id,
            "a.rs",
            "a-dev",
            &["save".to_string()],
            &[s1.node_id.clone()],
        )
        .await
        .unwrap();
    assert_eq!(deleted, 2, "develop's two fuzzy edges");
    let f5 = store
        .query_related(TENANT, &caller.node_id, 1, None, &scope(&["a-f5"]))
        .await
        .unwrap();
    assert_eq!(f5.len(), 2, "fase-5's version keeps its own fan-out");
    let dev = store
        .query_related(TENANT, &caller.node_id, 1, None, &scope(&["a-dev"]))
        .await
        .unwrap();
    assert_eq!(dev.len(), 1);
    assert_eq!(dev[0].node_id, s1.node_id);
}
