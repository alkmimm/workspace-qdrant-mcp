//! gRPC handler implementations for GraphService.

use tonic::{Request, Response, Status};
use tracing::{debug, error, info};
use workspace_qdrant_core::graph::algorithms::{
    compute_betweenness_centrality, compute_pagerank, detect_communities, detect_cycles,
    detect_test_gaps, CommunityConfig, PageRankConfig,
};
use workspace_qdrant_core::graph::EdgeType;

use crate::proto::{
    graph_service_server::GraphService, BetweennessNodeProto, BetweennessRequest,
    BetweennessResponse, CommunityMemberProto, CommunityProto, CommunityRequest, CommunityResponse,
    CycleMemberProto, CycleProto, CycleRequest, CycleResponse, GraphMigrateRequest,
    GraphMigrateResponse, GraphStatsRequest, GraphStatsResponse, ImpactAnalysisRequest,
    ImpactAnalysisResponse, ImpactNodeProto, LanguageCoverageProto, PageRankNodeProto,
    PageRankRequest, PageRankResponse, QueryRelatedRequest, QueryRelatedResponse, TestGapProto,
    TestGapsRequest, TestGapsResponse, TraversalNodeProto,
};
use crate::validation::extract_relative_path;

use super::helpers::{parse_edge_type_filter, retain_min_confidence};
use super::service_impl::GraphServiceImpl;

#[tonic::async_trait]
impl GraphService for GraphServiceImpl {
    #[tracing::instrument(skip_all, fields(method = "GraphService.query_related"))]
    async fn query_related(
        &self,
        request: Request<QueryRelatedRequest>,
    ) -> Result<Response<QueryRelatedResponse>, Status> {
        let req = request.into_inner();

        if req.tenant_id.is_empty() {
            return Err(Status::invalid_argument("tenant_id is required"));
        }
        if req.node_id.is_empty() && req.symbol_name.as_deref().map_or(true, str::is_empty) {
            return Err(Status::invalid_argument(
                "either node_id or symbol_name is required",
            ));
        }
        // Confidence is a product in [0,1]; a threshold above 1.0 would silently
        // filter out EVERY node (indistinguishable from "no relations"), so
        // reject it loudly — the classic mistake is passing a percentage.
        if req.min_confidence.is_some_and(|m| m > 1.0) {
            return Err(Status::invalid_argument(
                "min_confidence must be within [0.0, 1.0] (best-path edge-weight product, not a percentage)",
            ));
        }

        let max_hops = req.max_hops.clamp(0, 5);
        // Bound the result list at the daemon (the query ORDERs BY depth, so this
        // keeps the nearest nodes). 0/absent = all, so `wqm graph query` (CLI) is
        // unchanged — otherwise a hub node's N-hop fan-out is serialized whole.
        let top_k = req.top_k.filter(|&v| v > 0).map(|v| v as usize);

        // Parse edge type filters
        let edge_types: Option<Vec<EdgeType>> = if req.edge_types.is_empty() {
            None
        } else {
            let mut types = Vec::with_capacity(req.edge_types.len());
            for t in &req.edge_types {
                match EdgeType::from_str(t) {
                    Some(et) => types.push(et),
                    None => {
                        return Err(Status::invalid_argument(format!(
                            "unknown edge type: {}",
                            t
                        )));
                    }
                }
            }
            Some(types)
        };

        debug!(
            "GraphService.QueryRelated: tenant={} node={} hops={} edge_types={:?}",
            req.tenant_id, req.node_id, max_hops, edge_types
        );

        let start = std::time::Instant::now();

        // Primary: node_id-based traversal (precise; the only path on backends
        // without name resolution).
        let mut nodes = if req.node_id.is_empty() {
            Vec::new()
        } else {
            match self
                .graph_store
                .query_related(
                    &req.tenant_id,
                    &req.node_id,
                    max_hops,
                    edge_types.as_deref(),
                )
                .await
            {
                Ok(n) => n,
                Err(e) => {
                    error!("GraphService.QueryRelated failed: {}", e);
                    return Err(Status::internal(format!("Graph query failed: {}", e)));
                }
            }
        };

        // Fallback: resolve by symbol name when the node_id missed. A client
        // computes node_id from (symbol, type, file_path); if its symbol_type or
        // file_path differs from what the extractor stored, the precise lookup
        // returns nothing even though the symbol exists in the graph. Name
        // resolution (as impact/usages already do) recovers it.
        if nodes.is_empty() {
            if let Some(sym) = req.symbol_name.as_deref().filter(|s| !s.is_empty()) {
                let fp = req.file_path.as_deref().filter(|s| !s.is_empty());
                match self
                    .graph_store
                    .query_related_by_symbol(
                        &req.tenant_id,
                        sym,
                        fp,
                        max_hops,
                        edge_types.as_deref(),
                    )
                    .await
                {
                    Ok(n) => nodes = n,
                    Err(e) => {
                        error!("GraphService.QueryRelated (by symbol) failed: {}", e);
                        return Err(Status::internal(format!("Graph query failed: {}", e)));
                    }
                }
            }
        }

        // Precision filter (MCP `minConfidence`): drop low-confidence homonym
        // fan-out BEFORE the top_k cap and `total`, so top_k fills with passing
        // nodes and `total` reflects the filtered set. 0/absent = no filter.
        retain_min_confidence(&mut nodes, req.min_confidence, |n| n.confidence);

        let query_time_ms = start.elapsed().as_millis() as i64;
        let total = nodes.len() as u32;
        let proto_nodes: Vec<TraversalNodeProto> = nodes
            .into_iter()
            .take(top_k.unwrap_or(usize::MAX))
            .map(|n| TraversalNodeProto {
                node_id: n.node_id,
                symbol_name: n.symbol_name,
                symbol_type: n.symbol_type,
                file_path: n.file_path,
                edge_type: n.edge_type,
                depth: n.depth,
                path: n.path,
                confidence: n.confidence,
            })
            .collect();

        Ok(Response::new(QueryRelatedResponse {
            nodes: proto_nodes,
            total,
            query_time_ms,
        }))
    }

    #[tracing::instrument(skip_all, fields(method = "GraphService.impact_analysis"))]
    async fn impact_analysis(
        &self,
        request: Request<ImpactAnalysisRequest>,
    ) -> Result<Response<ImpactAnalysisResponse>, Status> {
        let req = request.into_inner();

        if req.tenant_id.is_empty() {
            return Err(Status::invalid_argument("tenant_id is required"));
        }
        if req.symbol_name.is_empty() {
            return Err(Status::invalid_argument("symbol_name is required"));
        }
        // Same guard as query_related: >1.0 would silently empty the result.
        if req.min_confidence.is_some_and(|m| m > 1.0) {
            return Err(Status::invalid_argument(
                "min_confidence must be within [0.0, 1.0] (best-path edge-weight product, not a percentage)",
            ));
        }

        // Validate optional file_path as RelativePath when present and non-empty.
        let validated_file_path = match req.file_path.as_deref().filter(|p| !p.is_empty()) {
            Some(raw) => {
                let rp = extract_relative_path!(raw.to_string(), "file_path")?;
                Some(rp.into_string())
            }
            None => None,
        };

        // Bound the impacted-node list at the daemon (reverse_traverse ORDERs BY
        // depth, so this keeps the nearest callers). 0/absent = all, so
        // `wqm graph impact` (CLI) is unchanged; total_impacted stays the TRUE count.
        let top_k = req.top_k.filter(|&v| v > 0).map(|v| v as usize);

        debug!(
            "GraphService.ImpactAnalysis: tenant={} symbol={} file={:?}",
            req.tenant_id, req.symbol_name, validated_file_path
        );

        let start = std::time::Instant::now();

        let result = self
            .graph_store
            .impact_analysis(
                &req.tenant_id,
                &req.symbol_name,
                validated_file_path.as_deref(),
            )
            .await;

        match result {
            Ok(mut report) => {
                let query_time_ms = start.elapsed().as_millis() as i64;

                // Precision filter (MCP `minConfidence`): drop low-confidence
                // nodes BEFORE the top_k cap and total_impacted, so both reflect
                // the filtered set. 0/absent = no filter (CLI output unchanged).
                retain_min_confidence(&mut report.impacted_nodes, req.min_confidence, |n| {
                    n.confidence
                });
                // Both backends set total_impacted = impacted_nodes.len(), so
                // recomputing unconditionally is an identity when no filter ran
                // and correct when it did — no flag to drift from the helper's
                // internal predicate.
                let total_impacted = report.impacted_nodes.len() as u32;

                let impacted_nodes: Vec<ImpactNodeProto> = report
                    .impacted_nodes
                    .into_iter()
                    .take(top_k.unwrap_or(usize::MAX))
                    .map(|n| ImpactNodeProto {
                        node_id: n.node_id,
                        symbol_name: n.symbol_name,
                        file_path: n.file_path,
                        impact_type: n.impact_type,
                        distance: n.distance,
                        confidence: n.confidence,
                    })
                    .collect();

                Ok(Response::new(ImpactAnalysisResponse {
                    impacted_nodes,
                    total_impacted,
                    query_time_ms,
                }))
            }
            Err(e) => {
                error!("GraphService.ImpactAnalysis failed: {}", e);
                Err(Status::internal(format!("Impact analysis failed: {}", e)))
            }
        }
    }

    #[tracing::instrument(skip_all, fields(method = "GraphService.get_graph_stats"))]
    async fn get_graph_stats(
        &self,
        request: Request<GraphStatsRequest>,
    ) -> Result<Response<GraphStatsResponse>, Status> {
        let req = request.into_inner();

        let tenant_filter = req.tenant_id.as_deref().filter(|s| !s.is_empty());

        debug!("GraphService.GetGraphStats: tenant={:?}", tenant_filter);

        let start = std::time::Instant::now();

        match self.graph_store.stats(tenant_filter).await {
            Ok(stats) => {
                let query_time_ms = start.elapsed().as_millis();
                debug!(
                    "GraphService.GetGraphStats: {} nodes, {} edges in {}ms",
                    stats.total_nodes, stats.total_edges, query_time_ms
                );

                Ok(Response::new(GraphStatsResponse {
                    total_nodes: stats.total_nodes,
                    total_edges: stats.total_edges,
                    nodes_by_type: stats.nodes_by_type,
                    edges_by_type: stats.edges_by_type,
                }))
            }
            Err(e) => {
                error!("GraphService.GetGraphStats failed: {}", e);
                Err(Status::internal(format!("Graph stats query failed: {}", e)))
            }
        }
    }

    #[tracing::instrument(skip_all, fields(method = "GraphService.compute_page_rank"))]
    async fn compute_page_rank(
        &self,
        request: Request<PageRankRequest>,
    ) -> Result<Response<PageRankResponse>, Status> {
        let req = request.into_inner();

        if req.tenant_id.is_empty() {
            return Err(Status::invalid_argument("tenant_id is required"));
        }

        let config = PageRankConfig {
            damping: req.damping.unwrap_or(0.85),
            max_iterations: req.max_iterations.unwrap_or(100) as usize,
            tolerance: req.tolerance.unwrap_or(1e-6),
        };

        let edge_filter = parse_edge_type_filter(&req.edge_types)?;
        let edge_refs: Option<Vec<&str>> = edge_filter
            .as_ref()
            .map(|v| v.iter().map(|s| s.as_str()).collect());

        debug!(
            "GraphService.ComputePageRank: tenant={} damping={} max_iter={}",
            req.tenant_id, config.damping, config.max_iterations
        );

        let start = std::time::Instant::now();

        let guard = self.graph_store.read().await;
        let pool = guard.pool();

        match compute_pagerank(pool, &req.tenant_id, &config, edge_refs.as_deref()).await {
            Ok(mut entries) => {
                let total = entries.len() as u32;

                // Apply top_k if requested
                if let Some(k) = req.top_k {
                    if k > 0 && (k as usize) < entries.len() {
                        entries.truncate(k as usize);
                    }
                }

                let query_time_ms = start.elapsed().as_millis() as i64;

                let proto_entries: Vec<PageRankNodeProto> = entries
                    .into_iter()
                    .map(|e| PageRankNodeProto {
                        node_id: e.node_id,
                        symbol_name: e.symbol_name,
                        symbol_type: e.symbol_type,
                        file_path: e.file_path,
                        score: e.score,
                    })
                    .collect();

                Ok(Response::new(PageRankResponse {
                    entries: proto_entries,
                    total,
                    query_time_ms,
                }))
            }
            Err(e) => {
                error!("GraphService.ComputePageRank failed: {}", e);
                Err(Status::internal(format!("PageRank failed: {}", e)))
            }
        }
    }

    #[tracing::instrument(skip_all, fields(method = "GraphService.detect_communities"))]
    async fn detect_communities(
        &self,
        request: Request<CommunityRequest>,
    ) -> Result<Response<CommunityResponse>, Status> {
        let req = request.into_inner();

        if req.tenant_id.is_empty() {
            return Err(Status::invalid_argument("tenant_id is required"));
        }

        let config = CommunityConfig {
            max_iterations: req.max_iterations.unwrap_or(50) as usize,
            min_community_size: req.min_community_size.unwrap_or(2) as usize,
        };

        let edge_filter = parse_edge_type_filter(&req.edge_types)?;
        let edge_refs: Option<Vec<&str>> = edge_filter
            .as_ref()
            .map(|v| v.iter().map(|s| s.as_str()).collect());

        debug!(
            "GraphService.DetectCommunities: tenant={} max_iter={} min_size={}",
            req.tenant_id, config.max_iterations, config.min_community_size
        );

        let start = std::time::Instant::now();

        let guard = self.graph_store.read().await;
        let pool = guard.pool();

        match detect_communities(pool, &req.tenant_id, &config, edge_refs.as_deref()).await {
            Ok(mut communities) => {
                let total_communities = communities.len() as u32;

                // Return only the top K largest communities when requested.
                // `detect_communities` already sorts by member count descending,
                // so a prefix truncation keeps the largest. This bounds the
                // response: a tenant-wide modules call on a big graph can
                // otherwise serialize every community/member and blow past the
                // gRPC message-size limit. `total_communities` still reports the
                // full count before truncation, mirroring PageRank/betweenness.
                if let Some(k) = req.top_k {
                    if k > 0 && (k as usize) < communities.len() {
                        communities.truncate(k as usize);
                    }
                }

                let query_time_ms = start.elapsed().as_millis() as i64;

                // Bound the per-community member list at the SOURCE. `top_k` caps
                // the community COUNT, but each of the largest communities can hold
                // thousands of members, so serializing them all still blows up the
                // gRPC message (and the downstream MCP response). `member_limit`
                // (0/absent = all) caps the per-community sample sent over the wire;
                // `member_count` preserves each community's true size so callers can
                // see how many were elided. The CLI omits member_limit and still
                // receives the full list.
                let member_limit = req.member_limit.filter(|&v| v > 0).map(|v| v as usize);

                let proto_communities: Vec<CommunityProto> = communities
                    .into_iter()
                    .map(|c| {
                        let member_count = c.members.len() as u32;
                        let members: Vec<CommunityMemberProto> = c
                            .members
                            .into_iter()
                            .take(member_limit.unwrap_or(usize::MAX))
                            .map(|m| CommunityMemberProto {
                                node_id: m.node_id,
                                symbol_name: m.symbol_name,
                                symbol_type: m.symbol_type,
                                file_path: m.file_path,
                            })
                            .collect();
                        CommunityProto {
                            community_id: c.community_id,
                            members,
                            member_count,
                        }
                    })
                    .collect();

                Ok(Response::new(CommunityResponse {
                    communities: proto_communities,
                    total_communities,
                    query_time_ms,
                }))
            }
            Err(e) => {
                error!("GraphService.DetectCommunities failed: {}", e);
                Err(Status::internal(format!(
                    "Community detection failed: {}",
                    e
                )))
            }
        }
    }

    #[tracing::instrument(skip_all, fields(method = "GraphService.compute_betweenness"))]
    async fn compute_betweenness(
        &self,
        request: Request<BetweennessRequest>,
    ) -> Result<Response<BetweennessResponse>, Status> {
        let req = request.into_inner();

        if req.tenant_id.is_empty() {
            return Err(Status::invalid_argument("tenant_id is required"));
        }

        let edge_filter = parse_edge_type_filter(&req.edge_types)?;
        let edge_refs: Option<Vec<&str>> = edge_filter
            .as_ref()
            .map(|v| v.iter().map(|s| s.as_str()).collect());

        let max_samples = req.max_samples.filter(|&v| v > 0).map(|v| v as usize);

        debug!(
            "GraphService.ComputeBetweenness: tenant={} max_samples={:?}",
            req.tenant_id, max_samples
        );

        let start = std::time::Instant::now();

        let guard = self.graph_store.read().await;
        let pool = guard.pool();

        match compute_betweenness_centrality(
            pool,
            &req.tenant_id,
            edge_refs.as_deref(),
            max_samples,
        )
        .await
        {
            Ok(mut entries) => {
                let total = entries.len() as u32;

                if let Some(k) = req.top_k {
                    if k > 0 && (k as usize) < entries.len() {
                        entries.truncate(k as usize);
                    }
                }

                let query_time_ms = start.elapsed().as_millis() as i64;

                let proto_entries: Vec<BetweennessNodeProto> = entries
                    .into_iter()
                    .map(|e| BetweennessNodeProto {
                        node_id: e.node_id,
                        symbol_name: e.symbol_name,
                        symbol_type: e.symbol_type,
                        file_path: e.file_path,
                        score: e.score,
                    })
                    .collect();

                Ok(Response::new(BetweennessResponse {
                    entries: proto_entries,
                    total,
                    query_time_ms,
                }))
            }
            Err(e) => {
                error!("GraphService.ComputeBetweenness failed: {}", e);
                Err(Status::internal(format!(
                    "Betweenness centrality failed: {}",
                    e
                )))
            }
        }
    }

    #[tracing::instrument(skip_all, fields(method = "GraphService.detect_cycles"))]
    async fn detect_cycles(
        &self,
        request: Request<CycleRequest>,
    ) -> Result<Response<CycleResponse>, Status> {
        let req = request.into_inner();

        if req.tenant_id.is_empty() {
            return Err(Status::invalid_argument("tenant_id is required"));
        }

        let edge_filter = parse_edge_type_filter(&req.edge_types)?;
        let edge_refs: Option<Vec<&str>> = edge_filter
            .as_ref()
            .map(|v| v.iter().map(|s| s.as_str()).collect());

        // Absent → 2 (skip single-node self-loops / direct recursion). An
        // explicit 1 opts into self-loops; the algorithm floors at 1.
        let min_cycle_size = req.min_cycle_size.map(|v| v as usize).unwrap_or(2);

        debug!(
            "GraphService.DetectCycles: tenant={} min_cycle_size={}",
            req.tenant_id, min_cycle_size
        );

        let start = std::time::Instant::now();

        let guard = self.graph_store.read().await;
        let pool = guard.pool();

        match detect_cycles(pool, &req.tenant_id, edge_refs.as_deref(), min_cycle_size).await {
            Ok(mut cycles) => {
                let total = cycles.len() as u32;

                if let Some(k) = req.top_k {
                    if k > 0 && (k as usize) < cycles.len() {
                        cycles.truncate(k as usize);
                    }
                }

                let query_time_ms = start.elapsed().as_millis() as i64;

                let proto_cycles: Vec<CycleProto> = cycles
                    .into_iter()
                    .map(|c| CycleProto {
                        members: c
                            .members
                            .into_iter()
                            .map(|m| CycleMemberProto {
                                node_id: m.node_id,
                                symbol_name: m.symbol_name,
                                symbol_type: m.symbol_type,
                                file_path: m.file_path,
                            })
                            .collect(),
                        files: c.files,
                        cross_file: c.cross_file,
                    })
                    .collect();

                Ok(Response::new(CycleResponse {
                    cycles: proto_cycles,
                    total,
                    query_time_ms,
                }))
            }
            Err(e) => {
                error!("GraphService.DetectCycles failed: {}", e);
                Err(Status::internal(format!("Cycle detection failed: {}", e)))
            }
        }
    }

    #[tracing::instrument(skip_all, fields(method = "GraphService.detect_test_gaps"))]
    async fn detect_test_gaps(
        &self,
        request: Request<TestGapsRequest>,
    ) -> Result<Response<TestGapsResponse>, Status> {
        let req = request.into_inner();

        if req.tenant_id.is_empty() {
            return Err(Status::invalid_argument("tenant_id is required"));
        }

        let edge_filter = parse_edge_type_filter(&req.edge_types)?;
        let edge_refs: Option<Vec<&str>> = edge_filter
            .as_ref()
            .map(|v| v.iter().map(|s| s.as_str()).collect());

        // Absent/0 = return all gaps; the algorithm floors truncation itself.
        let top_k = req.top_k.map(|v| v as usize).unwrap_or(0);

        debug!(
            "GraphService.DetectTestGaps: tenant={} top_k={}",
            req.tenant_id, top_k
        );

        let start = std::time::Instant::now();

        let guard = self.graph_store.read().await;
        let pool = guard.pool();

        match detect_test_gaps(pool, &req.tenant_id, edge_refs.as_deref(), top_k).await {
            Ok(report) => {
                let query_time_ms = start.elapsed().as_millis() as i64;
                let gaps: Vec<TestGapProto> = report
                    .gaps
                    .into_iter()
                    .map(|g| TestGapProto {
                        node_id: g.node_id,
                        symbol_name: g.symbol_name,
                        symbol_type: g.symbol_type,
                        file_path: g.file_path,
                        production_dependents: g.production_dependents,
                    })
                    .collect();

                let coverage_by_language: Vec<LanguageCoverageProto> = report
                    .coverage_by_language
                    .into_iter()
                    .map(|l| LanguageCoverageProto {
                        extension: l.extension,
                        production: l.production,
                        covered: l.covered,
                        test_nodes: l.test_nodes,
                    })
                    .collect();

                Ok(Response::new(TestGapsResponse {
                    gaps,
                    total_production: report.total_production,
                    covered: report.covered,
                    gap_count: report.gap_count,
                    query_time_ms,
                    test_nodes: report.test_nodes,
                    // Carried verbatim from the core algorithm so the MCP tool
                    // and `wqm graph test_gaps` warn on identical terms.
                    reliability_warning: report.reliability_warning,
                    excluded_non_production: report.excluded_non_production,
                    coverage_by_language,
                }))
            }
            Err(e) => {
                error!("GraphService.DetectTestGaps failed: {}", e);
                Err(Status::internal(format!(
                    "Test-gap detection failed: {}",
                    e
                )))
            }
        }
    }

    #[tracing::instrument(skip_all, fields(method = "GraphService.migrate_graph"))]
    async fn migrate_graph(
        &self,
        request: Request<GraphMigrateRequest>,
    ) -> Result<Response<GraphMigrateResponse>, Status> {
        let req = request.into_inner();

        // Currently only sqlite->sqlite is supported (ladybug requires feature flag)
        if req.from_backend != "sqlite" {
            return Err(Status::unimplemented(format!(
                "Export from '{}' is not yet supported. Only 'sqlite' is available.",
                req.from_backend
            )));
        }

        if req.to_backend != "sqlite" && req.to_backend != "ladybug" {
            return Err(Status::invalid_argument(format!(
                "Unknown target backend: '{}'. Use 'sqlite' or 'ladybug'.",
                req.to_backend
            )));
        }

        if req.to_backend == "ladybug" {
            return Err(Status::unimplemented(
                "LadybugDB migration via gRPC is not yet implemented. \
                 Use the CLI: wqm graph migrate --from sqlite --to ladybug",
            ));
        }

        let tenant_id = req.tenant_id.as_deref().filter(|s| !s.is_empty());
        let batch_size = req.batch_size.unwrap_or(500) as usize;

        info!(
            "GraphService.MigrateGraph: {} → {} (tenant={:?}, batch={})",
            req.from_backend, req.to_backend, tenant_id, batch_size
        );

        let guard = self.graph_store.read().await;
        let pool = guard.pool();

        // Export from SQLite
        let snapshot = workspace_qdrant_core::graph::migrator::export_sqlite(pool, tenant_id)
            .await
            .map_err(|e| {
                error!("Migration export failed: {}", e);
                Status::internal(format!("Export failed: {}", e))
            })?;

        // For now, import back to the same SQLite store (real ladybug migration
        // requires runtime construction of the ladybug store which needs the
        // graph config from daemon state — future enhancement)
        let report =
            workspace_qdrant_core::graph::migrator::import_to_store(&snapshot, &*guard, batch_size)
                .await
                .map_err(|e| {
                    error!("Migration import failed: {}", e);
                    Status::internal(format!("Import failed: {}", e))
                })?;

        Ok(Response::new(GraphMigrateResponse {
            success: report.nodes_match && report.edges_match,
            nodes_exported: report.nodes_exported,
            edges_exported: report.edges_exported,
            nodes_imported: report.nodes_imported,
            edges_imported: report.edges_imported,
            nodes_match: report.nodes_match,
            edges_match: report.edges_match,
            warnings: report.warnings,
        }))
    }
}
