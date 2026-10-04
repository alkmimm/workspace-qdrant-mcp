//! Shared graph store with read-write coordination.
//!
//! Wraps a `GraphStore` in `Arc<RwLock<...>>` so that batch writes
//! (a generation's delete-then-insert) appear atomic to concurrent
//! readers. SQLite WAL handles DB-level concurrency; this RwLock
//! coordinates the Rust-level access pattern.

use std::sync::Arc;

use tokio::sync::RwLock;

use super::{
    EdgeType, ExtractedGeneration, GenerationBranches, GraphDbResult, GraphEdge, GraphNode,
    GraphScope, GraphStats, GraphStore, ImpactReport, TraversalNode,
};

/// Thread-safe, cloneable handle to a `GraphStore` with read-write coordination.
///
/// - **Readers** (gRPC query handlers): acquire a shared read lock.
/// - **Writers** (queue processor): acquire an exclusive write lock for the
///   full delete-then-insert cycle, so readers never see a half-updated file.
///
/// Cloning is cheap (Arc bump).
#[derive(Clone)]
pub struct SharedGraphStore<S: GraphStore> {
    inner: Arc<RwLock<S>>,
}

impl<S: GraphStore> SharedGraphStore<S> {
    /// Wrap a store in a shared handle.
    pub fn new(store: S) -> Self {
        Self {
            inner: Arc::new(RwLock::new(store)),
        }
    }

    /// Access the inner store under a read lock for advanced operations.
    pub async fn read(&self) -> tokio::sync::RwLockReadGuard<'_, S> {
        self.inner.read().await
    }

    // ── Read operations (shared lock) ────────────────────────────────

    /// Query nodes related to a given node within N hops, inside `scope`.
    pub async fn query_related(
        &self,
        tenant_id: &str,
        node_id: &str,
        max_hops: u32,
        edge_types: Option<&[EdgeType]>,
        scope: &GraphScope,
    ) -> GraphDbResult<Vec<TraversalNode>> {
        let guard = self.inner.read().await;
        guard
            .query_related(tenant_id, node_id, max_hops, edge_types, scope)
            .await
    }

    /// Query related nodes resolving the source BY SYMBOL NAME (+ optional
    /// file_path) — the robust fallback when a client-computed node_id misses.
    pub async fn query_related_by_symbol(
        &self,
        tenant_id: &str,
        symbol_name: &str,
        file_path: Option<&str>,
        max_hops: u32,
        edge_types: Option<&[EdgeType]>,
        scope: &GraphScope,
    ) -> GraphDbResult<Vec<TraversalNode>> {
        let guard = self.inner.read().await;
        guard
            .query_related_by_symbol(
                tenant_id,
                symbol_name,
                file_path,
                max_hops,
                edge_types,
                scope,
            )
            .await
    }

    /// Impact analysis for a symbol change, inside `scope`.
    pub async fn impact_analysis(
        &self,
        tenant_id: &str,
        symbol_name: &str,
        file_path: Option<&str>,
        scope: &GraphScope,
    ) -> GraphDbResult<ImpactReport> {
        let guard = self.inner.read().await;
        guard
            .impact_analysis(tenant_id, symbol_name, file_path, scope)
            .await
    }

    /// Graph statistics over the rows `scope` admits.
    pub async fn stats(
        &self,
        tenant_id: Option<&str>,
        scope: &GraphScope,
    ) -> GraphDbResult<GraphStats> {
        let guard = self.inner.read().await;
        guard.stats(tenant_id, scope).await
    }

    /// Whether a generation has been extracted (indexed probe, shared lock).
    pub async fn generation_extracted(
        &self,
        tenant_id: &str,
        generation: &str,
    ) -> GraphDbResult<bool> {
        let guard = self.inner.read().await;
        guard.generation_extracted(tenant_id, generation).await
    }

    /// Every extracted generation of a tenant (shared lock).
    pub async fn extracted_generations(
        &self,
        tenant_id: &str,
    ) -> GraphDbResult<Vec<ExtractedGeneration>> {
        let guard = self.inner.read().await;
        guard.extracted_generations(tenant_id).await
    }

    // ── Write operations (exclusive lock) ────────────────────────────

    /// Upsert a batch of nodes (exclusive lock).
    pub async fn upsert_nodes(&self, nodes: &[GraphNode]) -> GraphDbResult<()> {
        let guard = self.inner.write().await;
        guard.upsert_nodes(nodes).await
    }

    /// Insert a batch of edges (exclusive lock).
    pub async fn insert_edges(&self, edges: &[GraphEdge]) -> GraphDbResult<()> {
        let guard = self.inner.write().await;
        guard.insert_edges(edges).await
    }

    /// Store one extraction of `file_path` as content `generation`, replacing
    /// whatever that generation held — and nothing any other version of the
    /// file holds. Holds the write lock for the whole replacement so readers
    /// never see a half-written version.
    ///
    /// The extraction is stamped first (see [`stamp_generation`]): the file's
    /// own nodes and every edge belong to the generation, a node of another
    /// file keeps the shared empty generation.
    pub async fn reingest_file(
        &self,
        tenant_id: &str,
        file_path: &str,
        generation: &str,
        nodes: &[GraphNode],
        edges: &[GraphEdge],
    ) -> GraphDbResult<()> {
        let (nodes, edges) = stamp_generation(file_path, generation, nodes, edges);
        let guard = self.inner.write().await;
        guard
            .replace_generation(tenant_id, file_path, generation, &nodes, &edges)
            .await
    }

    /// Delete one generation's rows (exclusive lock): it left the index.
    pub async fn delete_generation(&self, tenant_id: &str, generation: &str) -> GraphDbResult<u64> {
        let guard = self.inner.write().await;
        guard.delete_generation(tenant_id, generation).await
    }

    /// Delete all data for a tenant (exclusive lock).
    pub async fn delete_tenant(&self, tenant_id: &str) -> GraphDbResult<u64> {
        let guard = self.inner.write().await;
        guard.delete_tenant(tenant_id).await
    }

    /// Prune orphaned nodes (exclusive lock).
    pub async fn prune_orphans(&self, tenant_id: &str) -> GraphDbResult<u64> {
        let guard = self.inner.write().await;
        guard.prune_orphans(tenant_id).await
    }

    /// Resolve dangling stub edges to real nodes by name (exclusive lock).
    pub async fn resolve_stub_edges(
        &self,
        tenant_id: &str,
        membership: &GenerationBranches,
    ) -> GraphDbResult<u64> {
        let guard = self.inner.write().await;
        guard.resolve_stub_edges(tenant_id, membership).await
    }

    /// Make a caller's LSP-resolved CALLS authoritative (exclusive lock, R8.2).
    pub async fn make_calls_authoritative(
        &self,
        tenant_id: &str,
        caller_id: &str,
        source_file: &str,
        generation: &str,
        resolved_names: &[String],
        precise_targets: &[String],
    ) -> GraphDbResult<u64> {
        let guard = self.inner.write().await;
        guard
            .make_calls_authoritative(
                tenant_id,
                caller_id,
                source_file,
                generation,
                resolved_names,
                precise_targets,
            )
            .await
    }
}

/// Stamp one extraction of `file_path` with its content generation.
///
/// The file's own nodes and every edge it produced belong to `generation`. A
/// node of ANOTHER file — a name-only stub, or a definition the extraction
/// only referred to (an LSP-resolved callee) — keeps the empty generation:
/// that file's own extraction is what defines it, per version.
pub fn stamp_generation(
    file_path: &str,
    generation: &str,
    nodes: &[GraphNode],
    edges: &[GraphEdge],
) -> (Vec<GraphNode>, Vec<GraphEdge>) {
    let nodes = nodes
        .iter()
        .cloned()
        .map(|mut n| {
            n.generation = if n.file_path == file_path {
                generation.to_string()
            } else {
                String::new()
            };
            n
        })
        .collect();
    let edges = edges
        .iter()
        .cloned()
        .map(|mut e| {
            e.generation = generation.to_string();
            e
        })
        .collect();
    (nodes, edges)
}
