//! Unit tests for graph_service helpers and path validation.

use workspace_qdrant_core::graph::EdgeType;

use super::helpers::parse_edge_type_filter;

#[test]
fn test_edge_type_parsing() {
    assert!(EdgeType::from_str("CALLS").is_some());
    assert!(EdgeType::from_str("IMPORTS").is_some());
    assert!(EdgeType::from_str("USES_TYPE").is_some());
    assert!(EdgeType::from_str("CONTAINS").is_some());
    assert!(EdgeType::from_str("EXTENDS").is_some());
    assert!(EdgeType::from_str("IMPLEMENTS").is_some());
    assert!(EdgeType::from_str("INVALID").is_none());
}

#[test]
fn test_parse_edge_type_filter_empty() {
    let result = parse_edge_type_filter(&[]);
    assert!(result.is_ok());
    assert!(result.unwrap().is_none());
}

#[test]
fn test_parse_edge_type_filter_valid() {
    let types = vec!["CALLS".to_string(), "IMPORTS".to_string()];
    let result = parse_edge_type_filter(&types);
    assert!(result.is_ok());
    let filter = result.unwrap().unwrap();
    assert_eq!(filter.len(), 2);
}

#[test]
fn test_parse_edge_type_filter_invalid() {
    let types = vec!["CALLS".to_string(), "INVALID".to_string()];
    let result = parse_edge_type_filter(&types);
    assert!(result.is_err());
}

// ── ImpactAnalysisRequest.file_path (relative, optional) path validation ──

mod path_validation {
    use tonic::Request;
    use workspace_qdrant_core::graph::create_sqlite_graph_store;

    use crate::proto::graph_service_server::GraphService;
    use crate::proto::ImpactAnalysisRequest;
    use crate::services::GraphServiceImpl;

    /// Create a minimal graph store backed by a temp directory.
    async fn test_graph_service() -> (GraphServiceImpl, tempfile::TempDir) {
        let tmp = tempfile::tempdir().unwrap();
        let store = create_sqlite_graph_store(tmp.path()).await.unwrap();
        (GraphServiceImpl::new(store), tmp)
    }

    #[tokio::test]
    async fn test_impact_analysis_absolute_file_path_rejected() {
        let (service, _tmp) = test_graph_service().await;

        let request = Request::new(ImpactAnalysisRequest {
            tenant_id: "abcd12345678".to_string(),
            symbol_name: "my_func".to_string(),
            file_path: Some("/absolute/path.rs".to_string()),
            top_k: None,
            min_confidence: None,
            branch: None,
            max_hops: None,
        });

        let result = service.impact_analysis(request).await;
        assert!(result.is_err());
        let status = result.unwrap_err();
        assert_eq!(status.code(), tonic::Code::InvalidArgument);
        assert!(
            status.message().contains("file_path"),
            "error should mention field name, got: {}",
            status.message()
        );
    }

    #[tokio::test]
    async fn test_impact_analysis_parent_dir_file_path_rejected() {
        let (service, _tmp) = test_graph_service().await;

        let request = Request::new(ImpactAnalysisRequest {
            tenant_id: "abcd12345678".to_string(),
            symbol_name: "my_func".to_string(),
            file_path: Some("src/../secret.rs".to_string()),
            top_k: None,
            min_confidence: None,
            branch: None,
            max_hops: None,
        });

        let result = service.impact_analysis(request).await;
        assert!(result.is_err());
        let status = result.unwrap_err();
        assert_eq!(status.code(), tonic::Code::InvalidArgument);
        assert!(status.message().contains(".."));
    }

    #[tokio::test]
    async fn test_impact_analysis_empty_file_path_allowed() {
        // Empty file_path means "no file scope" — should not be rejected.
        let (service, _tmp) = test_graph_service().await;

        let request = Request::new(ImpactAnalysisRequest {
            tenant_id: "abcd12345678".to_string(),
            symbol_name: "my_func".to_string(),
            file_path: Some(String::new()),
            top_k: None,
            min_confidence: None,
            branch: None,
            max_hops: None,
        });

        // Empty string is filtered to None by the handler.
        let result = service.impact_analysis(request).await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_impact_analysis_valid_relative_path_accepted() {
        let (service, _tmp) = test_graph_service().await;

        let request = Request::new(ImpactAnalysisRequest {
            tenant_id: "abcd12345678".to_string(),
            symbol_name: "my_func".to_string(),
            file_path: Some("src/lib.rs".to_string()),
            top_k: None,
            min_confidence: None,
            branch: None,
            max_hops: None,
        });

        // Valid relative path should pass validation (query may return empty).
        let result = service.impact_analysis(request).await;
        assert!(result.is_ok());
    }
}

// ── min_confidence: daemon-side precision filter (before top_k and totals) ──

mod min_confidence_filter {
    use tonic::Request;
    use workspace_qdrant_core::graph::{
        create_sqlite_graph_store, EdgeType, GraphEdge, GraphNode, NodeType,
    };

    use crate::proto::graph_service_server::GraphService;
    use crate::proto::{ImpactAnalysisRequest, QueryRelatedRequest};
    use crate::services::GraphServiceImpl;

    const TENANT: &str = "abcd12345678";

    /// Service over a seeded store:
    ///   caller_strong -(0.9)-> hub -(0.9)-> callee_strong
    ///   caller_weak  -(0.15)-> hub -(0.15)-> callee_weak
    async fn seeded_service() -> (GraphServiceImpl, tempfile::TempDir, String) {
        let tmp = tempfile::tempdir().unwrap();
        let store = create_sqlite_graph_store(tmp.path()).await.unwrap();

        let hub = GraphNode::new(TENANT, "hub.rs", "hub", NodeType::Function);
        let caller_strong = GraphNode::new(TENANT, "cs.rs", "caller_strong", NodeType::Function);
        let caller_weak = GraphNode::new(TENANT, "cw.rs", "caller_weak", NodeType::Function);
        let callee_strong = GraphNode::new(TENANT, "ts.rs", "callee_strong", NodeType::Function);
        let callee_weak = GraphNode::new(TENANT, "tw.rs", "callee_weak", NodeType::Function);
        let hub_id = hub.node_id.clone();
        store
            .upsert_nodes(&[
                hub.clone(),
                caller_strong.clone(),
                caller_weak.clone(),
                callee_strong.clone(),
                callee_weak.clone(),
            ])
            .await
            .unwrap();

        let mut in_strong = GraphEdge::new(
            TENANT,
            &caller_strong.node_id,
            &hub.node_id,
            EdgeType::Calls,
            "cs.rs",
        );
        in_strong.weight = 0.9;
        let mut in_weak = GraphEdge::new(
            TENANT,
            &caller_weak.node_id,
            &hub.node_id,
            EdgeType::Calls,
            "cw.rs",
        );
        in_weak.weight = 0.15;
        let mut out_strong = GraphEdge::new(
            TENANT,
            &hub.node_id,
            &callee_strong.node_id,
            EdgeType::Calls,
            "hub.rs",
        );
        out_strong.weight = 0.9;
        let mut out_weak = GraphEdge::new(
            TENANT,
            &hub.node_id,
            &callee_weak.node_id,
            EdgeType::Calls,
            "hub.rs",
        );
        out_weak.weight = 0.15;
        store
            .insert_edges(&[in_strong, in_weak, out_strong, out_weak])
            .await
            .unwrap();

        (GraphServiceImpl::new(store), tmp, hub_id)
    }

    fn related_request(
        hub_id: &str,
        min_confidence: Option<f64>,
        top_k: Option<u32>,
    ) -> Request<QueryRelatedRequest> {
        Request::new(QueryRelatedRequest {
            tenant_id: TENANT.to_string(),
            node_id: hub_id.to_string(),
            max_hops: 1,
            edge_types: vec![],
            top_k,
            symbol_name: None,
            file_path: None,
            min_confidence,
            branch: None,
        })
    }

    fn impact_request(
        min_confidence: Option<f64>,
        top_k: Option<u32>,
    ) -> Request<ImpactAnalysisRequest> {
        Request::new(ImpactAnalysisRequest {
            tenant_id: TENANT.to_string(),
            symbol_name: "hub".to_string(),
            file_path: None,
            top_k,
            min_confidence,
            branch: None,
            max_hops: None,
        })
    }

    // ── #367: the cap must cut from a DEFINED order, not an arbitrary one ──
    //
    // The traversal emitted each depth's nodes by iterating a `HashMap`, whose
    // order Rust randomizes per instance, and the handler then applied
    // `take(top_k)` with no `sort` anywhere before it. A cap over more
    // candidates than it keeps was therefore a coin flip: the same question
    // answered differently on successive calls (field measurement: 50 of 3644
    // usages, a different 50 each run), and nothing in the response said the
    // answer was a sample. These pin both halves — which node survives, and
    // that repeating the question repeats the answer.

    #[tokio::test]
    async fn query_related_cap_keeps_strongest_not_arbitrary() {
        let (service, _tmp, hub_id) = seeded_service().await;

        // No min_confidence: BOTH callees are candidates, so the cap alone
        // decides. Under the old code this assertion failed roughly half the
        // time — which is exactly why it is worth pinning.
        let resp = service
            .query_related(related_request(&hub_id, None, Some(1)))
            .await
            .unwrap()
            .into_inner();

        assert_eq!(resp.total, 2, "total must count the uncapped set");
        assert_eq!(resp.nodes.len(), 1);
        assert_eq!(
            resp.nodes[0].symbol_name, "callee_strong",
            "the cap must keep the highest-confidence node, not whichever one \
             the hash map happened to yield first"
        );
    }

    #[tokio::test]
    async fn impact_analysis_cap_keeps_strongest_not_arbitrary() {
        let (service, _tmp, _hub_id) = seeded_service().await;

        let resp = service
            .impact_analysis(impact_request(None, Some(1)))
            .await
            .unwrap()
            .into_inner();

        assert_eq!(resp.total_impacted, 2, "total must count the uncapped set");
        assert_eq!(resp.impacted_nodes.len(), 1);
        assert_eq!(
            resp.impacted_nodes[0].symbol_name, "caller_strong",
            "the cap must keep the highest-confidence caller"
        );
    }

    /// The same question must return the same answer. `RandomState` reseeds per
    /// map instance, so a fresh traversal per call is enough to expose the
    /// reordering without needing a second process.
    #[tokio::test]
    async fn repeated_queries_return_identical_order() {
        let (service, _tmp, hub_id) = seeded_service().await;

        let mut seen: Vec<Vec<String>> = Vec::new();
        for _ in 0..8 {
            let resp = service
                .query_related(related_request(&hub_id, None, None))
                .await
                .unwrap()
                .into_inner();
            seen.push(resp.nodes.into_iter().map(|n| n.symbol_name).collect());
        }

        assert_eq!(
            seen[0],
            vec!["callee_strong".to_string(), "callee_weak".to_string()],
            "documented order is nearest depth first, then confidence descending"
        );
        assert!(
            seen.windows(2).all(|w| w[0] == w[1]),
            "identical queries returned different orders: {seen:?}"
        );
    }

    #[tokio::test]
    async fn query_related_filters_before_top_k_and_total() {
        let (service, _tmp, hub_id) = seeded_service().await;

        // Unfiltered baseline: both callees, true total.
        let resp = service
            .query_related(related_request(&hub_id, None, None))
            .await
            .unwrap()
            .into_inner();
        assert_eq!(resp.total, 2);
        assert_eq!(resp.nodes.len(), 2);

        // Filter at 0.5 with top_k=1: the one returned node must be the strong
        // callee (the filter runs BEFORE the cap — a post-cap filter could
        // return the weak node or nothing), and total counts the filtered set.
        let resp = service
            .query_related(related_request(&hub_id, Some(0.5), Some(1)))
            .await
            .unwrap()
            .into_inner();
        assert_eq!(resp.total, 1, "total must reflect the filtered set");
        assert_eq!(resp.nodes.len(), 1);
        assert_eq!(resp.nodes[0].symbol_name, "callee_strong");
    }

    #[tokio::test]
    async fn impact_analysis_filters_before_top_k_and_total() {
        let (service, _tmp, _hub_id) = seeded_service().await;

        let resp = service
            .impact_analysis(impact_request(None, None))
            .await
            .unwrap()
            .into_inner();
        assert_eq!(resp.total_impacted, 2);

        let resp = service
            .impact_analysis(impact_request(Some(0.5), Some(1)))
            .await
            .unwrap()
            .into_inner();
        assert_eq!(
            resp.total_impacted, 1,
            "total_impacted must reflect the filtered set"
        );
        assert_eq!(resp.impacted_nodes.len(), 1);
        assert_eq!(resp.impacted_nodes[0].symbol_name, "caller_strong");
    }

    #[tokio::test]
    async fn min_confidence_above_one_is_rejected() {
        let (service, _tmp, hub_id) = seeded_service().await;

        // > 1.0 would silently filter out EVERY node (confidence <= 1.0), so
        // both handlers reject it loudly instead.
        let err = service
            .query_related(related_request(&hub_id, Some(1.5), None))
            .await
            .unwrap_err();
        assert_eq!(err.code(), tonic::Code::InvalidArgument);
        assert!(err.message().contains("min_confidence"));

        let err = service
            .impact_analysis(impact_request(Some(1.5), None))
            .await
            .unwrap_err();
        assert_eq!(err.code(), tonic::Code::InvalidArgument);
        assert!(err.message().contains("min_confidence"));
    }
}

// ── Branch scoping: an answer describes one branch's file versions ──

mod branch_scoping {
    use sqlx::sqlite::SqlitePoolOptions;
    use tonic::Request;
    use workspace_qdrant_core::graph::{
        create_sqlite_graph_store, EdgeType, GraphEdge, GraphNode, NodeType,
    };

    use crate::proto::graph_service_server::GraphService;
    use crate::proto::{GraphStatsRequest, ImpactAnalysisRequest};
    use crate::services::GraphServiceImpl;

    const TENANT: &str = "abcd12345678";

    /// develop and fase-5 hold different versions of page.ts: develop's calls
    /// legacy() (legacy.ts, develop only), fase-5's calls modern() (modern.ts,
    /// fase-5 only). fase-5 also holds notes.md, never extracted.
    async fn service() -> (GraphServiceImpl, tempfile::TempDir) {
        let tmp = tempfile::tempdir().unwrap();
        let store = create_sqlite_graph_store(tmp.path()).await.unwrap();
        let render = GraphNode::new(TENANT, "page.ts", "render", NodeType::Function);
        let legacy = GraphNode::new(TENANT, "legacy.ts", "legacy", NodeType::Function);
        let modern = GraphNode::new(TENANT, "modern.ts", "modern", NodeType::Function);
        let call = |to: &GraphNode| {
            GraphEdge::new(
                TENANT,
                &render.node_id,
                &to.node_id,
                EdgeType::Calls,
                "page.ts",
            )
        };
        for (file, generation, nodes, edges) in [
            (
                "page.ts",
                "page-dev",
                vec![render.clone()],
                vec![call(&legacy)],
            ),
            (
                "page.ts",
                "page-f5",
                vec![render.clone()],
                vec![call(&modern)],
            ),
            ("legacy.ts", "legacy-dev", vec![legacy.clone()], vec![]),
            ("modern.ts", "modern-f5", vec![modern.clone()], vec![]),
        ] {
            store
                .reingest_file(TENANT, file, generation, &nodes, &edges)
                .await
                .unwrap();
        }

        let state = SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .unwrap();
        for ddl in [
            "CREATE TABLE watch_folders (watch_id TEXT PRIMARY KEY, path TEXT NOT NULL, \
             tenant_id TEXT NOT NULL, enabled INTEGER NOT NULL DEFAULT 1, parent_watch_id TEXT)",
            "CREATE TABLE tracked_files (watch_folder_id TEXT, relative_path TEXT, \
             base_point TEXT, branches TEXT)",
        ] {
            sqlx::query(ddl).execute(&state).await.unwrap();
        }
        sqlx::query("INSERT INTO watch_folders VALUES ('w', '/nonexistent', ?1, 1, NULL)")
            .bind(TENANT)
            .execute(&state)
            .await
            .unwrap();
        for (path, generation, branch) in [
            ("page.ts", "page-dev", "develop"),
            ("page.ts", "page-f5", "fase-5"),
            ("legacy.ts", "legacy-dev", "develop"),
            ("modern.ts", "modern-f5", "fase-5"),
            ("notes.md", "notes-f5", "fase-5"),
            // develop is the trunk (most tagged): fase-5 is tagged only on what
            // it changed and sees develop's copy of everything else.
            ("shared.ts", "shared-dev", "develop"),
            ("util.ts", "util-dev", "develop"),
        ] {
            sqlx::query("INSERT INTO tracked_files VALUES ('w', ?1, ?2, json_array(?3))")
                .bind(path)
                .bind(generation)
                .bind(branch)
                .execute(&state)
                .await
                .unwrap();
        }
        (
            GraphServiceImpl::new(store).with_state_pool(Some(state)),
            tmp,
        )
    }

    fn impact(symbol: &str, branch: &str) -> Request<ImpactAnalysisRequest> {
        Request::new(ImpactAnalysisRequest {
            tenant_id: TENANT.into(),
            symbol_name: symbol.into(),
            file_path: None,
            top_k: None,
            min_confidence: None,
            branch: Some(branch.into()),
            max_hops: None,
        })
    }

    #[tokio::test]
    async fn impact_answers_for_the_asked_branch_only() {
        let (svc, _tmp) = service().await;
        let dev = svc
            .impact_analysis(impact("legacy", "develop"))
            .await
            .unwrap()
            .into_inner();
        assert_eq!(dev.total_impacted, 1, "develop's page.ts calls legacy");

        let f5 = svc
            .impact_analysis(impact("legacy", "fase-5"))
            .await
            .unwrap()
            .into_inner();
        assert_eq!(
            f5.total_impacted, 0,
            "fase-5 holds neither legacy.ts nor develop's page.ts"
        );
        let scope = f5
            .scope
            .expect("every answer says which branch it describes");
        assert_eq!(scope.branch, "fase-5");
        // Its own page-f5, modern-f5, notes-f5, plus develop's copy of the paths
        // it holds none of (no git here, so every such path fills in).
        assert_eq!(scope.indexed_files, 6);
        assert_eq!(
            scope.graphed_files, 3,
            "page-f5, modern-f5 and the filled-in legacy-dev were extracted"
        );
    }

    #[tokio::test]
    async fn stats_count_the_branch_and_star_counts_everything() {
        let (svc, _tmp) = service().await;
        let stats = |branch: &str| {
            Request::new(GraphStatsRequest {
                tenant_id: Some(TENANT.into()),
                branch: Some(branch.into()),
            })
        };
        let dev = svc
            .get_graph_stats(stats("develop"))
            .await
            .unwrap()
            .into_inner();
        assert_eq!((dev.total_nodes, dev.total_edges), (2, 1));
        let all = svc.get_graph_stats(stats("*")).await.unwrap().into_inner();
        assert_eq!((all.total_nodes, all.total_edges), (4, 2));
        assert_eq!(all.scope.unwrap().branch, "*");
    }
}
