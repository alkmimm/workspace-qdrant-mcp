//! The storage trait every graph backend implements.

use async_trait::async_trait;

use super::{
    EdgeType, ExtractedGeneration, GenerationBranches, GraphDbResult, GraphEdge, GraphNode,
    GraphScope, GraphStats, ImpactReport, TraversalNode,
};

/// Trait abstracting graph storage operations.
///
/// Implementations:
/// - `SqliteGraphStore`: SQLite with recursive CTEs (default)
/// - `LadybugGraphStore`: Kuzu fork with Cypher queries (`ladybug` feature)
///
/// Writes are per content GENERATION (see the module docs): a file's
/// extraction replaces exactly the rows of its generation, and a generation
/// leaves the graph only when it leaves the index. Reads take a
/// [`GraphScope`] — the generations the asking branch holds.
#[async_trait]
pub trait GraphStore: Send + Sync {
    /// Insert or update a node row (keyed by node_id and generation).
    async fn upsert_node(&self, node: &GraphNode) -> GraphDbResult<()>;

    /// Batch upsert multiple nodes in a single transaction.
    async fn upsert_nodes(&self, nodes: &[GraphNode]) -> GraphDbResult<()>;

    /// Insert an edge row. Ignores duplicates (same edge_id and generation).
    async fn insert_edge(&self, edge: &GraphEdge) -> GraphDbResult<()>;

    /// Batch insert multiple edges in a single transaction.
    async fn insert_edges(&self, edges: &[GraphEdge]) -> GraphDbResult<()>;

    /// Replace everything one content generation of `file_path` contributes,
    /// atomically, and record that the generation was extracted.
    ///
    /// The rows of OTHER generations of the same path are untouched: they
    /// belong to other branches. `nodes` and `edges` must already carry the
    /// generation (see `SharedGraphStore::reingest_file`, which stamps it).
    /// An empty extraction is recorded too, so a file with no symbols is not
    /// re-extracted on every pass.
    async fn replace_generation(
        &self,
        tenant_id: &str,
        file_path: &str,
        generation: &str,
        nodes: &[GraphNode],
        edges: &[GraphEdge],
    ) -> GraphDbResult<()>;

    /// Delete one generation's nodes, edges and extraction record. Called when
    /// the generation leaves the index. Returns the rows deleted.
    async fn delete_generation(&self, _tenant_id: &str, _generation: &str) -> GraphDbResult<u64> {
        Ok(0)
    }

    /// Whether `generation` has been extracted (an empty extraction counts).
    ///
    /// Default `true` ("assume present"): a backend without extraction
    /// records never triggers the dedup-path rebuild or the backfill.
    async fn generation_extracted(
        &self,
        _tenant_id: &str,
        _generation: &str,
    ) -> GraphDbResult<bool> {
        Ok(true)
    }

    /// Every extracted generation of a tenant, with its extraction time.
    async fn extracted_generations(
        &self,
        _tenant_id: &str,
    ) -> GraphDbResult<Vec<ExtractedGeneration>> {
        Ok(Vec::new())
    }

    /// Delete all nodes and edges for a tenant.
    async fn delete_tenant(&self, tenant_id: &str) -> GraphDbResult<u64>;

    /// Query nodes related to a given node within N hops, inside `scope`.
    async fn query_related(
        &self,
        tenant_id: &str,
        node_id: &str,
        max_hops: u32,
        edge_types: Option<&[EdgeType]>,
        scope: &GraphScope,
    ) -> GraphDbResult<Vec<TraversalNode>>;

    /// Like [`query_related`](Self::query_related) but resolves the source
    /// node(s) BY SYMBOL NAME (+ optional file_path) instead of a precomputed
    /// node_id. A client computes node_id = SHA256(tenant|file_path|name|type),
    /// which silently misses whenever its `symbol_type`/`file_path` differ from
    /// what the extractor stored (e.g. an async fn keyed as "async_function" vs
    /// "function"). This resolves the node the same robust way `impact_analysis`
    /// does (name match, file_path as a soft narrowing), traverses forward from
    /// every match, and merges (dedup by node_id, lowest depth wins).
    ///
    /// Default impl returns empty so the caller keeps its node_id-based result;
    /// backends with name resolution override it.
    async fn query_related_by_symbol(
        &self,
        _tenant_id: &str,
        _symbol_name: &str,
        _file_path: Option<&str>,
        _max_hops: u32,
        _edge_types: Option<&[EdgeType]>,
        _scope: &GraphScope,
    ) -> GraphDbResult<Vec<TraversalNode>> {
        Ok(Vec::new())
    }

    /// Find all nodes in `scope` that would be affected by changing a symbol.
    async fn impact_analysis(
        &self,
        tenant_id: &str,
        symbol_name: &str,
        file_path: Option<&str>,
        scope: &GraphScope,
    ) -> GraphDbResult<ImpactReport>;

    /// Get graph statistics, optionally filtered by tenant, counting only the
    /// rows `scope` admits.
    async fn stats(&self, tenant_id: Option<&str>, scope: &GraphScope)
        -> GraphDbResult<GraphStats>;

    /// Delete orphaned nodes (nodes with no edges).
    async fn prune_orphans(&self, tenant_id: &str) -> GraphDbResult<u64>;

    /// Resolve dangling "stub" edges to real symbol nodes by name.
    ///
    /// Tree-sitter emits name-only stub callees/targets with an empty
    /// `file_path` (a node_id that never matches the callee's real node).
    /// This pass repoints each such edge to a real node with the same
    /// `symbol_name` when an unambiguous match exists (same-file preference,
    /// then unique-in-tenant), recomputing the edge_id, and prunes the
    /// now-orphaned stub nodes. Stdlib/external names (no project node)
    /// stay dangling and are naturally excluded from the resolved graph.
    ///
    /// `membership` says which branches hold each generation: a candidate
    /// definition only counts for an edge when some branch holds both, so a
    /// name defined once per branch is unique on each branch instead of an
    /// ambiguous fan-out across all of them.
    ///
    /// Default impl is a no-op for backends that don't produce stub edges.
    /// Returns the number of edges repointed.
    async fn resolve_stub_edges(
        &self,
        _tenant_id: &str,
        _membership: &GenerationBranches,
    ) -> GraphDbResult<u64> {
        Ok(0)
    }

    /// Make a caller's LSP-resolved CALLS authoritative (R8.2 backfill).
    ///
    /// Deletes the caller's fuzzy CALLS edges in `generation` to ANY node whose
    /// `symbol_name` is in `resolved_names` (the by-name fan-out the LSP
    /// supersedes — by backfill time the stub has usually already fanned out,
    /// so this clears by target name, not by stub id), then inserts a precise
    /// CALLS edge to each `precise_targets` node id (weight 1.0,
    /// `metadata.resolution = "lsp"`, owned by `source_file`'s `generation` so
    /// it lives and dies with that version of the file). Returns the number of
    /// fuzzy edges deleted. Default impl: no-op.
    async fn make_calls_authoritative(
        &self,
        _tenant_id: &str,
        _caller_id: &str,
        _source_file: &str,
        _generation: &str,
        _resolved_names: &[String],
        _precise_targets: &[String],
    ) -> GraphDbResult<u64> {
        Ok(0)
    }
}
