//! Shared graph store with read-write coordination.
//!
//! Wraps a `GraphStore` in `Arc<RwLock<...>>` so that batch writes
//! (delete-then-insert during re-ingestion) appear atomic to concurrent
//! readers. SQLite WAL handles DB-level concurrency; this RwLock
//! coordinates the Rust-level access pattern.

use std::sync::Arc;

use tokio::sync::RwLock;

use super::{
    EdgeType, GraphDbResult, GraphEdge, GraphNode, GraphStats, GraphStore, ImpactReport,
    TraversalNode,
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

    /// Query nodes related to a given node within N hops.
    pub async fn query_related(
        &self,
        tenant_id: &str,
        node_id: &str,
        max_hops: u32,
        edge_types: Option<&[EdgeType]>,
    ) -> GraphDbResult<Vec<TraversalNode>> {
        let guard = self.inner.read().await;
        guard
            .query_related(tenant_id, node_id, max_hops, edge_types)
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
    ) -> GraphDbResult<Vec<TraversalNode>> {
        let guard = self.inner.read().await;
        guard
            .query_related_by_symbol(tenant_id, symbol_name, file_path, max_hops, edge_types)
            .await
    }

    /// Impact analysis for a symbol change.
    pub async fn impact_analysis(
        &self,
        tenant_id: &str,
        symbol_name: &str,
        file_path: Option<&str>,
    ) -> GraphDbResult<ImpactReport> {
        let guard = self.inner.read().await;
        guard
            .impact_analysis(tenant_id, symbol_name, file_path)
            .await
    }

    /// Graph statistics.
    pub async fn stats(&self, tenant_id: Option<&str>) -> GraphDbResult<GraphStats> {
        let guard = self.inner.read().await;
        guard.stats(tenant_id).await
    }

    /// Whether any edge is owned by a file (indexed probe, shared lock).
    pub async fn file_has_edges(&self, tenant_id: &str, file_path: &str) -> GraphDbResult<bool> {
        let guard = self.inner.read().await;
        guard.file_has_edges(tenant_id, file_path).await
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

    /// Atomic re-ingestion: delete the file's old edges AND nodes, then insert
    /// the new nodes and edges. Holds the write lock for the entire operation so
    /// readers never see a partially-updated file.
    ///
    /// Deleting the file's nodes first (not just its edges) makes the node set
    /// authoritative per re-ingest: dropped symbols no longer linger as stale
    /// generations, and a delete (`nodes`/`edges` empty) leaves no "ghost" nodes
    /// for a path that no longer exists (issue #245). Node ids are deterministic
    /// (`hash(tenant|file|symbol|type)`), so unchanged symbols keep the same id
    /// across the swap and incoming cross-file edges re-resolve to them; only
    /// genuinely-removed symbols' nodes disappear. The file-less stub nodes
    /// (`file_path = ''`) are never a `file_path` here, so they are untouched.
    pub async fn reingest_file(
        &self,
        tenant_id: &str,
        file_path: &str,
        nodes: &[GraphNode],
        edges: &[GraphEdge],
    ) -> GraphDbResult<()> {
        let guard = self.inner.write().await;
        guard.delete_edges_by_file(tenant_id, file_path).await?;
        guard.delete_nodes_by_file(tenant_id, file_path).await?;
        guard.upsert_nodes(nodes).await?;
        guard.insert_edges(edges).await?;
        Ok(())
    }

    /// Delete all NODES anchored to a file (exclusive lock). Used by the
    /// ghost-node sweep (issue #245) to clear nodes for paths that no longer
    /// exist. A no-op for an empty path (the file-less stub nodes are spared).
    pub async fn delete_nodes_by_file(
        &self,
        tenant_id: &str,
        file_path: &str,
    ) -> GraphDbResult<u64> {
        let guard = self.inner.write().await;
        guard.delete_nodes_by_file(tenant_id, file_path).await
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
    pub async fn resolve_stub_edges(&self, tenant_id: &str) -> GraphDbResult<u64> {
        let guard = self.inner.write().await;
        guard.resolve_stub_edges(tenant_id).await
    }

    /// Make a caller's LSP-resolved CALLS authoritative (exclusive lock, R8.2).
    pub async fn make_calls_authoritative(
        &self,
        tenant_id: &str,
        caller_id: &str,
        source_file: &str,
        resolved_names: &[String],
        precise_targets: &[String],
    ) -> GraphDbResult<u64> {
        let guard = self.inner.write().await;
        guard
            .make_calls_authoritative(
                tenant_id,
                caller_id,
                source_file,
                resolved_names,
                precise_targets,
            )
            .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::graph::SqliteGraphStore;
    use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};

    async fn test_shared_store() -> SharedGraphStore<SqliteGraphStore> {
        let opts = SqliteConnectOptions::new()
            .filename(":memory:")
            .create_if_missing(true)
            .journal_mode(sqlx::sqlite::SqliteJournalMode::Wal)
            .foreign_keys(true);

        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect_with(opts)
            .await
            .unwrap();

        // Run schema
        sqlx::query(
            "CREATE TABLE graph_nodes (
                node_id TEXT PRIMARY KEY,
                tenant_id TEXT NOT NULL,
                symbol_name TEXT NOT NULL,
                symbol_type TEXT NOT NULL,
                file_path TEXT NOT NULL,
                start_line INTEGER,
                end_line INTEGER,
                signature TEXT,
                language TEXT,
                created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
                updated_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now'))
            )",
        )
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query("CREATE INDEX idx_nodes_tenant ON graph_nodes(tenant_id)")
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query("CREATE INDEX idx_nodes_file ON graph_nodes(tenant_id, file_path)")
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query("CREATE INDEX idx_nodes_symbol ON graph_nodes(tenant_id, symbol_name)")
            .execute(&pool)
            .await
            .unwrap();

        sqlx::query(
            "CREATE TABLE graph_edges (
                edge_id TEXT PRIMARY KEY,
                tenant_id TEXT NOT NULL,
                source_node_id TEXT NOT NULL,
                target_node_id TEXT NOT NULL,
                edge_type TEXT NOT NULL,
                source_file TEXT NOT NULL,
                weight REAL DEFAULT 1.0,
                metadata_json TEXT,
                created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
                FOREIGN KEY (source_node_id) REFERENCES graph_nodes(node_id),
                FOREIGN KEY (target_node_id) REFERENCES graph_nodes(node_id)
            )",
        )
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query("CREATE INDEX idx_edges_tenant ON graph_edges(tenant_id)")
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query("CREATE INDEX idx_edges_source ON graph_edges(source_node_id)")
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query("CREATE INDEX idx_edges_target ON graph_edges(target_node_id)")
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query("CREATE INDEX idx_edges_source_file ON graph_edges(tenant_id, source_file)")
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query("CREATE INDEX idx_edges_type ON graph_edges(edge_type)")
            .execute(&pool)
            .await
            .unwrap();

        SharedGraphStore::new(SqliteGraphStore::new(pool))
    }

    const T: &str = "test-tenant";

    #[tokio::test]
    async fn test_shared_write_then_read() {
        let store = test_shared_store().await;

        let nodes = vec![
            GraphNode::new(T, "a.rs", "a", super::super::NodeType::Function),
            GraphNode::new(T, "b.rs", "b", super::super::NodeType::Function),
        ];
        store.upsert_nodes(&nodes).await.unwrap();

        let edge = GraphEdge::new(
            T,
            &nodes[0].node_id,
            &nodes[1].node_id,
            super::super::EdgeType::Calls,
            "a.rs",
        );
        store.insert_edges(&[edge]).await.unwrap();

        let stats = store.stats(Some(T)).await.unwrap();
        assert_eq!(stats.total_nodes, 2);
        assert_eq!(stats.total_edges, 1);
    }

    #[tokio::test]
    async fn test_reingest_file_atomic() {
        let store = test_shared_store().await;

        let a = GraphNode::new(T, "a.rs", "a", super::super::NodeType::Function);
        let b = GraphNode::new(T, "b.rs", "b", super::super::NodeType::Function);
        store.upsert_nodes(&[a.clone(), b.clone()]).await.unwrap();

        let old_edge = GraphEdge::new(
            T,
            &a.node_id,
            &b.node_id,
            super::super::EdgeType::Calls,
            "a.rs",
        );
        store.insert_edges(&[old_edge]).await.unwrap();

        // Re-ingest a.rs with a new edge target
        let c = GraphNode::new(T, "c.rs", "c", super::super::NodeType::Function);
        let new_edge = GraphEdge::new(
            T,
            &a.node_id,
            &c.node_id,
            super::super::EdgeType::Calls,
            "a.rs",
        );
        store
            .reingest_file(T, "a.rs", &[a.clone(), c], &[new_edge])
            .await
            .unwrap();

        let stats = store.stats(Some(T)).await.unwrap();
        // Old a->b edge deleted, new a->c edge inserted
        assert_eq!(stats.total_edges, 1);
    }

    #[tokio::test]
    async fn test_concurrent_readers() {
        let store = test_shared_store().await;

        let a = GraphNode::new(T, "a.rs", "a", super::super::NodeType::Function);
        let b = GraphNode::new(T, "b.rs", "b", super::super::NodeType::Function);
        store.upsert_nodes(&[a.clone(), b.clone()]).await.unwrap();

        let edge = GraphEdge::new(
            T,
            &a.node_id,
            &b.node_id,
            super::super::EdgeType::Calls,
            "a.rs",
        );
        store.insert_edges(&[edge]).await.unwrap();

        // Spawn 10 concurrent readers
        let mut handles = Vec::new();
        for _ in 0..10 {
            let s = store.clone();
            let node_id = a.node_id.clone();
            handles.push(tokio::spawn(async move {
                s.query_related(T, &node_id, 1, None).await.unwrap()
            }));
        }

        for handle in handles {
            let results = handle.await.unwrap();
            assert_eq!(results.len(), 1);
        }
    }

    #[tokio::test]
    async fn test_resolve_stub_edges_by_name() {
        use super::super::{EdgeType, NodeType};
        let store = test_shared_store().await;

        // Real caller (a.rs) + real callee (b.rs) + a name-only stub "callee".
        let caller = GraphNode::new(T, "a.rs", "caller", NodeType::Function);
        let callee = GraphNode::new(T, "b.rs", "callee", NodeType::Function);
        let stub = GraphNode::stub(T, "callee", NodeType::Function);
        store
            .upsert_nodes(&[caller.clone(), callee.clone(), stub.clone()])
            .await
            .unwrap();
        // Dangling: caller -> stub("callee").
        let dangling = GraphEdge::new(T, &caller.node_id, &stub.node_id, EdgeType::Calls, "a.rs");
        store.insert_edges(&[dangling]).await.unwrap();

        // Unmatched external stub ("println") — no project node — stays dangling.
        let ext = GraphNode::stub(T, "println", NodeType::Function);
        store.upsert_nodes(&[ext.clone()]).await.unwrap();
        let ext_edge = GraphEdge::new(T, &caller.node_id, &ext.node_id, EdgeType::Calls, "a.rs");
        store.insert_edges(&[ext_edge]).await.unwrap();

        let repointed = store.resolve_stub_edges(T).await.unwrap();
        assert_eq!(repointed, 1, "only the matchable stub edge repoints");

        // Edge now reaches the REAL callee in b.rs.
        let related = store
            .query_related(T, &caller.node_id, 1, None)
            .await
            .unwrap();
        assert!(
            related.iter().any(|n| n.file_path == "b.rs"),
            "resolved edge should target the real callee node in b.rs"
        );

        // Matched stub is pruned; the unmatched external stub remains.
        let guard = store.read().await;
        let stub_count: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM graph_nodes WHERE tenant_id = ?1 AND file_path = ''",
        )
        .bind(T)
        .fetch_one(guard.pool())
        .await
        .unwrap();
        assert_eq!(stub_count, 1, "matched stub pruned; 'println' stub remains");
    }

    #[tokio::test]
    async fn test_resolve_stub_edges_scopes_by_language() {
        use super::super::{EdgeType, NodeType};
        let store = test_shared_store().await;

        // A Rust caller calling `process`; two real `process` defs of the SAME bare
        // name in different languages (Rust b.rs + TypeScript c.ts). The resolver
        // must bind ONLY to the same-language (Rust) definition — a TS def is never a
        // valid callee of a Rust call (the cross-language `relations` noise class).
        let mut caller = GraphNode::new(T, "a.rs", "caller", NodeType::Function);
        caller.language = Some("rust".into());
        let mut rust_callee = GraphNode::new(T, "b.rs", "process", NodeType::Function);
        rust_callee.language = Some("rust".into());
        let mut ts_callee = GraphNode::new(T, "c.ts", "process", NodeType::Function);
        ts_callee.language = Some("typescript".into());
        let stub = GraphNode::stub(T, "process", NodeType::Function);
        store
            .upsert_nodes(&[
                caller.clone(),
                rust_callee.clone(),
                ts_callee.clone(),
                stub.clone(),
            ])
            .await
            .unwrap();
        let dangling = GraphEdge::new(T, &caller.node_id, &stub.node_id, EdgeType::Calls, "a.rs");
        store.insert_edges(&[dangling]).await.unwrap();

        let repointed = store.resolve_stub_edges(T).await.unwrap();
        assert_eq!(repointed, 1, "the dangling call repoints");

        let related = store
            .query_related(T, &caller.node_id, 1, None)
            .await
            .unwrap();
        assert!(
            related.iter().any(|n| n.file_path == "b.rs"),
            "must bind to the same-language (Rust) definition"
        );
        assert!(
            !related.iter().any(|n| n.file_path == "c.ts"),
            "must NOT bind to the cross-language (TypeScript) definition"
        );
    }

    #[tokio::test]
    async fn test_resolve_stub_edges_keeps_ambiguous() {
        use super::super::{EdgeType, NodeType};
        let store = test_shared_store().await;

        // Two real nodes named "new" in different files → ambiguous.
        let caller = GraphNode::new(T, "a.rs", "caller", NodeType::Function);
        let new_a = GraphNode::new(T, "x.rs", "new", NodeType::Function);
        let new_b = GraphNode::new(T, "y.rs", "new", NodeType::Function);
        let stub = GraphNode::stub(T, "new", NodeType::Function);
        store
            .upsert_nodes(&[caller.clone(), new_a.clone(), new_b.clone(), stub.clone()])
            .await
            .unwrap();
        let dangling = GraphEdge::new(T, &caller.node_id, &stub.node_id, EdgeType::Calls, "a.rs");
        store.insert_edges(&[dangling]).await.unwrap();

        // R1: the one dangling stub edge is RESOLVED (not skipped) — it fans out to
        // BOTH same-named candidates with confidence 1/N, restoring recall.
        let repointed = store.resolve_stub_edges(T).await.unwrap();
        assert_eq!(repointed, 1, "ambiguous stub must resolve (keep-all-candidates)");

        // Both candidates are now reachable from the caller (impact/usages recall).
        let related = store
            .query_related(T, &caller.node_id, 1, Some(&[EdgeType::Calls]))
            .await
            .unwrap();
        let targets: std::collections::HashSet<&str> =
            related.iter().map(|n| n.node_id.as_str()).collect();
        assert!(
            targets.contains(new_a.node_id.as_str()) && targets.contains(new_b.node_id.as_str()),
            "ambiguous call should reach every candidate, got {:?}",
            targets
        );
    }

    #[tokio::test]
    async fn test_resolve_stub_edges_proximity_collapses_fanout() {
        use super::super::{EdgeType, NodeType};
        let store = test_shared_store().await;

        // A caller in `pkg/a/` calls the ambiguous name `save`. Three real `save`
        // defs exist: one in the caller's OWN package (`pkg/a/`), two in a far
        // package (`pkg/z/`). R2.5 proximity precedence must collapse the fan-out
        // to the single same-package definition, dropping the far ones — cutting
        // the CALLS inflation while keeping the most-likely target.
        let caller = GraphNode::new(T, "pkg/a/caller.rs", "caller", NodeType::Function);
        let near = GraphNode::new(T, "pkg/a/repo.rs", "save", NodeType::Function);
        let far1 = GraphNode::new(T, "pkg/z/one.rs", "save", NodeType::Function);
        let far2 = GraphNode::new(T, "pkg/z/two.rs", "save", NodeType::Function);
        let stub = GraphNode::stub(T, "save", NodeType::Function);
        store
            .upsert_nodes(&[
                caller.clone(),
                near.clone(),
                far1.clone(),
                far2.clone(),
                stub.clone(),
            ])
            .await
            .unwrap();
        let dangling = GraphEdge::new(
            T,
            &caller.node_id,
            &stub.node_id,
            EdgeType::Calls,
            "pkg/a/caller.rs",
        );
        store.insert_edges(&[dangling]).await.unwrap();

        let repointed = store.resolve_stub_edges(T).await.unwrap();
        assert_eq!(repointed, 1, "the ambiguous stub resolves");

        let ids: std::collections::HashSet<String> = store
            .query_related(T, &caller.node_id, 1, Some(&[EdgeType::Calls]))
            .await
            .unwrap()
            .into_iter()
            .map(|n| n.node_id)
            .collect();
        assert!(
            ids.contains(&near.node_id),
            "must resolve to the same-package definition (pkg/a/repo.rs)"
        );
        assert!(
            !ids.contains(&far1.node_id) && !ids.contains(&far2.node_id),
            "far-package same-name candidates must be pruned by proximity, got {ids:?}"
        );

        // The collapsed edge is high-confidence (0.85) — a real resolution that
        // enters centrality, not a diluted 1/N fan-out.
        let guard = store.read().await;
        let weight: f64 = sqlx::query_scalar(
            "SELECT weight FROM graph_edges
             WHERE tenant_id = ?1 AND source_node_id = ?2 AND target_node_id = ?3",
        )
        .bind(T)
        .bind(&caller.node_id)
        .bind(&near.node_id)
        .fetch_one(guard.pool())
        .await
        .unwrap();
        assert!(
            (weight - 0.85).abs() < 1e-9,
            "proximity-unique resolution should carry confidence 0.85, got {weight}"
        );
    }

    #[tokio::test]
    async fn test_resolve_stub_edges_proximity_keeps_same_package_overloads() {
        use super::super::{EdgeType, NodeType};
        let store = test_shared_store().await;

        // Both `save` defs live in the caller's OWN package (`pkg/a/`) — a genuine
        // same-package overload set. Proximity gives no tiebreak (equal depth), so
        // the fan-out is preserved: impact/usages still reach both (recall floor).
        let caller = GraphNode::new(T, "pkg/a/caller.rs", "caller", NodeType::Function);
        let s1 = GraphNode::new(T, "pkg/a/one.rs", "save", NodeType::Function);
        let s2 = GraphNode::new(T, "pkg/a/two.rs", "save", NodeType::Function);
        let stub = GraphNode::stub(T, "save", NodeType::Function);
        store
            .upsert_nodes(&[caller.clone(), s1.clone(), s2.clone(), stub.clone()])
            .await
            .unwrap();
        store
            .insert_edges(&[GraphEdge::new(
                T,
                &caller.node_id,
                &stub.node_id,
                EdgeType::Calls,
                "pkg/a/caller.rs",
            )])
            .await
            .unwrap();

        store.resolve_stub_edges(T).await.unwrap();

        let ids: std::collections::HashSet<String> = store
            .query_related(T, &caller.node_id, 1, Some(&[EdgeType::Calls]))
            .await
            .unwrap()
            .into_iter()
            .map(|n| n.node_id)
            .collect();
        assert!(
            ids.contains(&s1.node_id) && ids.contains(&s2.node_id),
            "same-package overloads must both remain reachable, got {ids:?}"
        );

        // Keep-all fan-out weights each edge 1/N (=0.5 here): below the 0.6
        // centrality gate, so a same-package overload set is NOT promoted into
        // PageRank/betweenness — the invariant the fan-out normalization protects.
        let guard = store.read().await;
        let w: f64 = sqlx::query_scalar(
            "SELECT weight FROM graph_edges
             WHERE tenant_id = ?1 AND source_node_id = ?2 AND target_node_id = ?3",
        )
        .bind(T)
        .bind(&caller.node_id)
        .bind(&s1.node_id)
        .fetch_one(guard.pool())
        .await
        .unwrap();
        assert!(
            w < 0.6 && (w - 0.5).abs() < 1e-9,
            "same-package overload edge must stay at 1/N=0.5 (< 0.6 centrality gate), got {w}"
        );
    }

    #[tokio::test]
    async fn test_resolve_stub_edges_proximity_keeps_all_when_bucket_not_unique() {
        use super::super::{EdgeType, NodeType};
        let store = test_shared_store().await;

        // Two `save` defs in the caller's package (`pkg/a/`, depth 2) + one far
        // (`pkg/z/`, depth 1). The deepest bucket is NOT unique (two at depth 2),
        // so proximity must NOT narrow: the conservative choice keeps ALL candidates
        // (incl. the far one) reachable rather than dropping cross-package recall on
        // a non-decisive signal. (This is the branch the aggressive `1/k` tier used
        // to collapse; the review removed it.)
        let caller = GraphNode::new(T, "pkg/a/caller.rs", "caller", NodeType::Function);
        let near1 = GraphNode::new(T, "pkg/a/one.rs", "save", NodeType::Function);
        let near2 = GraphNode::new(T, "pkg/a/two.rs", "save", NodeType::Function);
        let far = GraphNode::new(T, "pkg/z/svc.rs", "save", NodeType::Function);
        let stub = GraphNode::stub(T, "save", NodeType::Function);
        store
            .upsert_nodes(&[
                caller.clone(),
                near1.clone(),
                near2.clone(),
                far.clone(),
                stub.clone(),
            ])
            .await
            .unwrap();
        store
            .insert_edges(&[GraphEdge::new(
                T,
                &caller.node_id,
                &stub.node_id,
                EdgeType::Calls,
                "pkg/a/caller.rs",
            )])
            .await
            .unwrap();

        store.resolve_stub_edges(T).await.unwrap();

        let ids: std::collections::HashSet<String> = store
            .query_related(T, &caller.node_id, 1, Some(&[EdgeType::Calls]))
            .await
            .unwrap()
            .into_iter()
            .map(|n| n.node_id)
            .collect();
        assert!(
            ids.contains(&near1.node_id)
                && ids.contains(&near2.node_id)
                && ids.contains(&far.node_id),
            "non-unique deepest bucket must NOT narrow — all candidates kept, got {ids:?}"
        );
    }

    #[tokio::test]
    async fn test_resolve_stub_edges_proximity_not_applied_to_contains() {
        use super::super::{EdgeType, NodeType};
        let store = test_shared_store().await;

        // A file-less CONTAINS parent stub `Config` whose member `load` lives in
        // `pkg/a/sub/helpers.rs`. Two real `Config` containers exist: one uniquely
        // deepest by directory (`pkg/a/sub/config.rs`, depth 3) and one far
        // (`pkg/z/config.rs`, depth 1). A CALLS stub in this position WOULD collapse
        // onto the near one by proximity — but containment is structurally 1:1, so
        // the `!container_only` guard must SKIP proximity here and, finding no
        // own-file / tenant-unique match, leave the CONTAINS dangling (never guess).
        let cfg_near = GraphNode::new(T, "pkg/a/sub/config.rs", "Config", NodeType::Class);
        let cfg_far = GraphNode::new(T, "pkg/z/config.rs", "Config", NodeType::Class);
        let load = GraphNode::new(T, "pkg/a/sub/helpers.rs", "load", NodeType::Method);
        let parent_stub = GraphNode::stub(T, "Config", NodeType::Class);
        store
            .upsert_nodes(&[
                cfg_near.clone(),
                cfg_far.clone(),
                load.clone(),
                parent_stub.clone(),
            ])
            .await
            .unwrap();
        // CONTAINS authored from the file-less parent stub -> member.
        store
            .insert_edges(&[GraphEdge::new(
                T,
                &parent_stub.node_id,
                &load.node_id,
                EdgeType::Contains,
                "pkg/a/sub/helpers.rs",
            )])
            .await
            .unwrap();

        let repointed = store.resolve_stub_edges(T).await.unwrap();
        assert_eq!(
            repointed, 0,
            "an ambiguous CONTAINS container must NOT be proximity-resolved"
        );

        // Neither real Config captured the member via a proximity guess.
        for cfg in [&cfg_near, &cfg_far] {
            let reached = store
                .query_related(T, &cfg.node_id, 1, Some(&[EdgeType::Contains]))
                .await
                .unwrap();
            assert!(
                !reached.iter().any(|n| n.symbol_name == "load"),
                "container {} must not own the member via proximity",
                cfg.file_path
            );
        }
    }

    #[tokio::test]
    async fn test_resolve_stub_edges_import_anchored_resolution() {
        use super::super::{EdgeType, NodeType};
        let store = test_shared_store().await;

        // Caller in pkg/a calls the ambiguous name `handle`. Two real `handle`
        // defs share the stem `handler` in DIFFERENT packages (pkg/x, pkg/y).
        // The caller's file IMPORTS `handle` from module `pkg::x::handler`, so R4
        // must anchor the call to pkg/x's handle (@0.90) — proximity alone can't
        // (both are equally far from pkg/a).
        let caller = GraphNode::new(T, "pkg/a/caller.rs", "caller", NodeType::Function);
        let hx = GraphNode::new(T, "pkg/x/handler.rs", "handle", NodeType::Function);
        let hy = GraphNode::new(T, "pkg/y/handler.rs", "handle", NodeType::Function);
        let file_node = GraphNode::new(T, "pkg/a/caller.rs", "pkg/a/caller.rs", NodeType::File);
        let call_stub = GraphNode::stub(T, "handle", NodeType::Function);
        let import_stub = GraphNode::stub(T, "handle", NodeType::Module);
        store
            .upsert_nodes(&[
                caller.clone(),
                hx.clone(),
                hy.clone(),
                file_node.clone(),
                call_stub.clone(),
                import_stub.clone(),
            ])
            .await
            .unwrap();

        let call_edge =
            GraphEdge::new(T, &caller.node_id, &call_stub.node_id, EdgeType::Calls, "pkg/a/caller.rs");
        // IMPORTS file -> stub(handle), carrying the module locator (as the
        // extractor stamps it).
        let mut import_edge = GraphEdge::new(
            T,
            &file_node.node_id,
            &import_stub.node_id,
            EdgeType::Imports,
            "pkg/a/caller.rs",
        );
        import_edge.metadata_json = Some("{\"module\":\"pkg::x::handler\"}".to_string());
        store.insert_edges(&[call_edge, import_edge]).await.unwrap();

        store.resolve_stub_edges(T).await.unwrap();

        let ids: std::collections::HashSet<String> = store
            .query_related(T, &caller.node_id, 1, Some(&[EdgeType::Calls]))
            .await
            .unwrap()
            .into_iter()
            .map(|n| n.node_id)
            .collect();
        assert!(
            ids.contains(&hx.node_id),
            "call must anchor to the IMPORTED pkg/x handle, got {ids:?}"
        );
        assert!(
            !ids.contains(&hy.node_id),
            "call must NOT reach the non-imported pkg/y handle, got {ids:?}"
        );

        // ...at import-anchored confidence 0.90 (enters centrality as a real edge).
        let guard = store.read().await;
        let w: f64 = sqlx::query_scalar(
            "SELECT weight FROM graph_edges
             WHERE tenant_id = ?1 AND source_node_id = ?2 AND target_node_id = ?3",
        )
        .bind(T)
        .bind(&caller.node_id)
        .bind(&hx.node_id)
        .fetch_one(guard.pool())
        .await
        .unwrap();
        assert!(
            (w - 0.9).abs() < 1e-9,
            "import-anchored edge should carry confidence 0.90, got {w}"
        );
    }

    #[tokio::test]
    async fn test_resolve_stub_edges_fanout_ceiling_leaves_hyper_ambiguous_unresolved() {
        use super::super::{EdgeType, NodeType};
        let store = test_shared_store().await;

        // `over` has 17 same-language defs across unrelated packages (> the default
        // ceiling of 16), with no own-file/class/import/proximity signal. Fanning
        // out to 17 edges @ ~0.06 is noise, not recall (and would make each of the
        // 17 a false caller in impact/usages), so the call is left UNRESOLVED.
        let caller = GraphNode::new(T, "a/caller.rs", "caller", NodeType::Function);
        let stub = GraphNode::stub(T, "over", NodeType::Function);
        let mut nodes = vec![caller.clone(), stub.clone()];
        for i in 0..17 {
            nodes.push(GraphNode::new(
                T,
                format!("pkg{i}/f.rs"),
                "over",
                NodeType::Function,
            ));
        }
        store.upsert_nodes(&nodes).await.unwrap();
        store
            .insert_edges(&[GraphEdge::new(
                T,
                &caller.node_id,
                &stub.node_id,
                EdgeType::Calls,
                "a/caller.rs",
            )])
            .await
            .unwrap();

        let repointed = store.resolve_stub_edges(T).await.unwrap();
        assert_eq!(repointed, 0, "a hyper-ambiguous call must not fan out");

        // Nothing real is reached — the call rests on the file-less stub only.
        let reached = store
            .query_related(T, &caller.node_id, 1, Some(&[EdgeType::Calls]))
            .await
            .unwrap();
        assert!(
            reached.iter().all(|n| n.file_path.is_empty()),
            "hyper-ambiguous call must stay unresolved, reached: {:?}",
            reached.iter().map(|n| &n.file_path).collect::<Vec<_>>()
        );
    }

    #[tokio::test]
    async fn test_resolve_stub_edges_scope_prefers_caller_class() {
        use super::super::{EdgeType, NodeType};
        let store = test_shared_store().await;

        // Class A (y.rs) contains caller (x.rs) and method m (y.rs); class B (z.rs)
        // defines its own m. The call `m()` from caller is ambiguous by name, but
        // caller's own file has no `m`, so R1's own-file tier misses. The SCOPE
        // tier must pick A's m (same class as the caller), NOT B's m — even though
        // both are tenant-visible candidates.
        let class_a = GraphNode::new(T, "y.rs", "A", NodeType::Struct);
        let class_b = GraphNode::new(T, "z.rs", "B", NodeType::Struct);
        let caller = GraphNode::new(T, "x.rs", "caller", NodeType::Function);
        let m_a = GraphNode::new(T, "y.rs", "m", NodeType::Function);
        let m_b = GraphNode::new(T, "z.rs", "m", NodeType::Function);
        let stub = GraphNode::stub(T, "m", NodeType::Function);
        store
            .upsert_nodes(&[
                class_a.clone(),
                class_b.clone(),
                caller.clone(),
                m_a.clone(),
                m_b.clone(),
                stub.clone(),
            ])
            .await
            .unwrap();
        store
            .insert_edges(&[
                // CONTAINS edges define class membership (build the scope map).
                GraphEdge::new(T, &class_a.node_id, &caller.node_id, EdgeType::Contains, "y.rs"),
                GraphEdge::new(T, &class_a.node_id, &m_a.node_id, EdgeType::Contains, "y.rs"),
                GraphEdge::new(T, &class_b.node_id, &m_b.node_id, EdgeType::Contains, "z.rs"),
                // The ambiguous, name-only call.
                GraphEdge::new(T, &caller.node_id, &stub.node_id, EdgeType::Calls, "x.rs"),
            ])
            .await
            .unwrap();

        let repointed = store.resolve_stub_edges(T).await.unwrap();
        assert_eq!(repointed, 1, "scoped stub must resolve to a single candidate");

        // The call resolves to A's m (same class) and NOT to B's m.
        let callees = store
            .query_related(T, &caller.node_id, 1, Some(&[EdgeType::Calls]))
            .await
            .unwrap();
        let ids: std::collections::HashSet<&str> =
            callees.iter().map(|n| n.node_id.as_str()).collect();
        assert!(
            ids.contains(m_a.node_id.as_str()),
            "scope should resolve to the caller's own class method (A.m)"
        );
        assert!(
            !ids.contains(m_b.node_id.as_str()),
            "scope must NOT fan out to the other class's method (B.m)"
        );
    }

    #[tokio::test]
    async fn test_query_related_by_symbol_resolves_when_node_id_unknown() {
        use super::super::{EdgeType, NodeType};
        let store = test_shared_store().await;

        // caller (a.rs) CALLS callee (b.rs).
        let caller = GraphNode::new(T, "a.rs", "caller", NodeType::Function);
        let callee = GraphNode::new(T, "b.rs", "callee", NodeType::Function);
        store
            .upsert_nodes(&[caller.clone(), callee.clone()])
            .await
            .unwrap();
        store
            .insert_edges(&[GraphEdge::new(
                T,
                &caller.node_id,
                &callee.node_id,
                EdgeType::Calls,
                "a.rs",
            )])
            .await
            .unwrap();

        // Resolve the source BY NAME (no node_id) → reaches the callee. This is
        // what makes relations robust to a client computing the wrong node_id.
        let by_name = store
            .query_related_by_symbol(T, "caller", None, 1, Some(&[EdgeType::Calls]))
            .await
            .unwrap();
        assert!(
            by_name.iter().any(|n| n.node_id == callee.node_id),
            "name resolution should reach the callee"
        );

        // A matching file_path narrows to the same node.
        let narrowed = store
            .query_related_by_symbol(T, "caller", Some("a.rs"), 1, Some(&[EdgeType::Calls]))
            .await
            .unwrap();
        assert!(narrowed.iter().any(|n| n.node_id == callee.node_id));

        // A WRONG file_path falls back to name-only — the footgun fix.
        let wrong_path = store
            .query_related_by_symbol(T, "caller", Some("nope.rs"), 1, Some(&[EdgeType::Calls]))
            .await
            .unwrap();
        assert!(
            wrong_path.iter().any(|n| n.node_id == callee.node_id),
            "a wrong file_path must fall back to name-only resolution"
        );

        // Unknown symbol → empty, not an error.
        let unknown = store
            .query_related_by_symbol(T, "does_not_exist", None, 1, None)
            .await
            .unwrap();
        assert!(unknown.is_empty());
    }

    #[tokio::test]
    async fn test_resolve_stub_edges_repoints_contains_source() {
        use super::super::{EdgeType, NodeType};
        let store = test_shared_store().await;

        // A real class + a same-named constructor (method) in the same file,
        // plus the file-less parent stub that CONTAINS edges are authored from.
        // The stub even carries the "wrong" type (Module) that Dart's parent
        // inference produces — resolution must still pick the real *container*
        // (the class), never the same-named constructor.
        let class = GraphNode::new(T, "app.dart", "Widget", NodeType::Class);
        let ctor = GraphNode::new(T, "app.dart", "Widget", NodeType::Method);
        let build = GraphNode::new(T, "app.dart", "build", NodeType::Method);
        let parent_stub = GraphNode::stub(T, "Widget", NodeType::Module);
        store
            .upsert_nodes(&[
                class.clone(),
                ctor.clone(),
                build.clone(),
                parent_stub.clone(),
            ])
            .await
            .unwrap();
        // CONTAINS authored from the file-less parent stub (stub -> member).
        let contains = GraphEdge::new(
            T,
            &parent_stub.node_id,
            &build.node_id,
            EdgeType::Contains,
            "app.dart",
        );
        store.insert_edges(&[contains]).await.unwrap();

        let repointed = store.resolve_stub_edges(T).await.unwrap();
        assert_eq!(repointed, 1, "the CONTAINS source-stub repoints");

        // The class node now reaches its member; the constructor must not have
        // captured the CONTAINS edge.
        let from_class = store
            .query_related(T, &class.node_id, 1, None)
            .await
            .unwrap();
        assert!(
            from_class.iter().any(|n| n.symbol_name == "build"),
            "class node should reach its member via the repointed CONTAINS"
        );
        let from_ctor = store
            .query_related(T, &ctor.node_id, 1, None)
            .await
            .unwrap();
        assert!(
            !from_ctor.iter().any(|n| n.symbol_name == "build"),
            "same-named constructor must not own the CONTAINS edge"
        );

        // The file-less parent stub is pruned once its edge is repointed.
        let guard = store.read().await;
        let stub_count: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM graph_nodes WHERE tenant_id = ?1 AND file_path = ''",
        )
        .bind(T)
        .fetch_one(guard.pool())
        .await
        .unwrap();
        assert_eq!(stub_count, 0, "resolved parent stub pruned");
    }

    #[tokio::test]
    async fn test_clone_is_cheap() {
        let store = test_shared_store().await;
        let clone1 = store.clone();
        let clone2 = store.clone();

        // All clones share the same underlying data
        let a = GraphNode::new(T, "a.rs", "a", super::super::NodeType::Function);
        store.upsert_nodes(&[a]).await.unwrap();

        let stats1 = clone1.stats(Some(T)).await.unwrap();
        let stats2 = clone2.stats(Some(T)).await.unwrap();
        assert_eq!(stats1.total_nodes, 1);
        assert_eq!(stats2.total_nodes, 1);
    }

    #[tokio::test]
    async fn test_make_calls_authoritative_replaces_fanout_with_precise() {
        use super::super::{EdgeType, NodeType};
        let store = test_shared_store().await;

        // caller (a/caller.rs) fuzzily fanned `save` out to two candidates.
        let caller = GraphNode::new(T, "a/caller.rs", "caller", NodeType::Function);
        let save1 = GraphNode::new(T, "pkg1/repo.rs", "save", NodeType::Function);
        let save2 = GraphNode::new(T, "pkg2/repo.rs", "save", NodeType::Function);
        store
            .upsert_nodes(&[caller.clone(), save1.clone(), save2.clone()])
            .await
            .unwrap();
        store
            .insert_edges(&[
                GraphEdge::new(T, &caller.node_id, &save1.node_id, EdgeType::Calls, "a/caller.rs"),
                GraphEdge::new(T, &caller.node_id, &save2.node_id, EdgeType::Calls, "a/caller.rs"),
            ])
            .await
            .unwrap();

        // The LSP resolved `save` to pkg1's save — make it authoritative.
        let deleted = store
            .make_calls_authoritative(
                T,
                &caller.node_id,
                "a/caller.rs",
                &["save".to_string()],
                &[save1.node_id.clone()],
            )
            .await
            .unwrap();
        assert_eq!(deleted, 2, "both fuzzy `save` edges dropped");

        // Only pkg1's save remains reachable from the caller.
        let ids: std::collections::HashSet<String> = store
            .query_related(T, &caller.node_id, 1, Some(&[EdgeType::Calls]))
            .await
            .unwrap()
            .into_iter()
            .map(|n| n.node_id)
            .collect();
        assert!(
            ids.contains(&save1.node_id) && !ids.contains(&save2.node_id),
            "authoritative edge points only at the LSP-resolved save, got {ids:?}"
        );

        // ...at precise weight 1.0 (enters centrality as a real edge).
        let guard = store.read().await;
        let w: f64 = sqlx::query_scalar(
            "SELECT weight FROM graph_edges
             WHERE tenant_id = ?1 AND source_node_id = ?2 AND target_node_id = ?3",
        )
        .bind(T)
        .bind(&caller.node_id)
        .bind(&save1.node_id)
        .fetch_one(guard.pool())
        .await
        .unwrap();
        assert!(
            (w - 1.0).abs() < 1e-9,
            "precise LSP edge should carry weight 1.0, got {w}"
        );
    }
}
