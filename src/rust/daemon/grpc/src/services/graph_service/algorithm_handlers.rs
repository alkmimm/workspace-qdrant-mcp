//! The graph-wide algorithm handlers: PageRank, communities, betweenness,
//! cycles and test gaps. Each scopes its graph load to the asking branch.

use tonic::{Request, Response, Status};
use tracing::{debug, error};
use workspace_qdrant_core::graph::algorithms::{
    compute_betweenness_report, compute_pagerank, detect_communities_report, detect_cycles,
    detect_test_gaps, CommunityConfig, PageRankConfig,
};

use crate::proto::{
    BetweennessNodeProto, BetweennessRequest, BetweennessResponse, CommunityMemberProto,
    CommunityProto, CommunityRequest, CommunityResponse, CycleMemberProto, CycleProto,
    CycleRequest, CycleResponse, LanguageCoverageProto, PageRankNodeProto, PageRankRequest,
    PageRankResponse, TestGapProto, TestGapsRequest, TestGapsResponse,
};

use super::helpers::parse_edge_type_filter;
use super::service_impl::GraphServiceImpl;

/// Wall-clock budgets for the graph-wide passes. They live HERE, at the gRPC
/// boundary, because they are a property of the call (the client-side timeout
/// they must undercut), not of the algorithm: the core functions take the
/// budget as a parameter so a test can drive the interrupted path with
/// `Duration::ZERO`. Whichever way a pass ends, the response says so
/// (`partial`), so a load-dependent cut is never read as a complete answer.
const BETWEENNESS_TIME_BUDGET: std::time::Duration = std::time::Duration::from_secs(20);
const COMMUNITY_TIME_BUDGET: std::time::Duration = std::time::Duration::from_secs(20);

impl GraphServiceImpl {
    pub(super) async fn run_compute_page_rank(
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

        let (scope, scope_info) = self
            .resolve_scope(&req.tenant_id, req.branch.as_deref())
            .await?;
        let start = std::time::Instant::now();

        let guard = self.graph_store.read().await;
        let pool = guard.pool();

        match compute_pagerank(pool, &req.tenant_id, &scope, &config, edge_refs.as_deref()).await {
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
                    scope: Some(scope_info),
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

    pub(super) async fn run_detect_communities(
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

        let (scope, scope_info) = self
            .resolve_scope(&req.tenant_id, req.branch.as_deref())
            .await?;
        let start = std::time::Instant::now();

        let guard = self.graph_store.read().await;
        let pool = guard.pool();

        match detect_communities_report(
            pool,
            &req.tenant_id,
            &scope,
            &config,
            edge_refs.as_deref(),
            COMMUNITY_TIME_BUDGET,
        )
        .await
        {
            Ok(report) => {
                let outcome = report.outcome;
                let mut communities = report.communities;
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
                    scope: Some(scope_info),
                    communities: proto_communities,
                    total_communities,
                    query_time_ms,
                    partial: outcome.budget_hit,
                    iterations: outcome.iterations as u32,
                    converged: outcome.converged,
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

    pub(super) async fn run_compute_betweenness(
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

        let (scope, scope_info) = self
            .resolve_scope(&req.tenant_id, req.branch.as_deref())
            .await?;
        let start = std::time::Instant::now();

        let guard = self.graph_store.read().await;
        let pool = guard.pool();

        match compute_betweenness_report(
            pool,
            &req.tenant_id,
            &scope,
            edge_refs.as_deref(),
            max_samples,
            BETWEENNESS_TIME_BUDGET,
        )
        .await
        {
            Ok(report) => {
                let mut entries = report.entries;
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
                    scope: Some(scope_info),
                    entries: proto_entries,
                    total,
                    query_time_ms,
                    partial: report.budget_hit,
                    sources_processed: report.sources_processed as u32,
                    sources_total: report.sources_total as u32,
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

    pub(super) async fn run_detect_cycles(
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

        let (scope, scope_info) = self
            .resolve_scope(&req.tenant_id, req.branch.as_deref())
            .await?;
        let start = std::time::Instant::now();

        let guard = self.graph_store.read().await;
        let pool = guard.pool();

        match detect_cycles(
            pool,
            &req.tenant_id,
            &scope,
            edge_refs.as_deref(),
            min_cycle_size,
        )
        .await
        {
            Ok(report) => {
                let suppressed_ubiquitous = report.suppressed_ubiquitous as u32;
                let mut cycles = report.cycles;
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
                    scope: Some(scope_info),
                    cycles: proto_cycles,
                    total,
                    query_time_ms,
                    suppressed_ubiquitous,
                }))
            }
            Err(e) => {
                error!("GraphService.DetectCycles failed: {}", e);
                Err(Status::internal(format!("Cycle detection failed: {}", e)))
            }
        }
    }

    pub(super) async fn run_detect_test_gaps(
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

        let (scope, scope_info) = self
            .resolve_scope(&req.tenant_id, req.branch.as_deref())
            .await?;
        let start = std::time::Instant::now();

        let guard = self.graph_store.read().await;
        let pool = guard.pool();

        match detect_test_gaps(pool, &req.tenant_id, &scope, edge_refs.as_deref(), top_k).await {
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
                        ambiguous_test_callers: g.ambiguous_test_callers,
                        parent_symbol: g.parent_symbol,
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
                    scope: Some(scope_info),
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
                    gaps_with_ambiguous_test_callers: report.gaps_with_ambiguous_test_callers,
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
}
