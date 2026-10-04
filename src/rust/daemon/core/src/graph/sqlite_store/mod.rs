//! SQLite-backed graph store with bounded breadth-first traversal.
//!
//! Rows are keyed by content generation (see `graph` module docs):
//! `writes` replaces and deletes one generation at a time, `traversal`
//! reads through a branch's [`GraphScope`], and `stub_resolution` repoints
//! name-only references within the branches that hold the referring file.

mod impact;
mod stub_resolution;
mod traversal;
mod writes;

use async_trait::async_trait;
use sqlx::SqlitePool;

use super::{
    EdgeType, ExtractedGeneration, GenerationBranches, GraphDbResult, GraphEdge, GraphNode,
    GraphScope, GraphStats, GraphStore, ImpactReport, TraversalNode,
};

/// SQLite-backed implementation of `GraphStore`.
///
/// Uses a dedicated `graph.db` with WAL mode. Multi-hop traversal is a bounded
/// breadth-first walk (one index-seeking query per hop, visited-set dedup, node
/// budget) — no graph database engine required.
#[derive(Clone)]
pub struct SqliteGraphStore {
    pool: SqlitePool,
}

impl SqliteGraphStore {
    /// Create a new store from an existing connection pool.
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }

    /// Whether a `symbol_type` string names a *container* kind — one that can
    /// hold members via a CONTAINS edge (class, struct, interface, trait, impl,
    /// module, enum). Used when resolving a CONTAINS parent stub so a same-named
    /// constructor/method never wins over the enclosing type.
    fn is_container_node_type(symbol_type: &str) -> bool {
        matches!(
            symbol_type,
            "class" | "struct" | "interface" | "trait" | "impl" | "module" | "enum"
        )
    }

    /// Number of leading DIRECTORY components two file paths share (the file
    /// name — the final path segment — is ignored, so two files in the same
    /// directory share their full directory depth). `resolve_stub_edges` uses
    /// this for proximity precedence (R2.5): among otherwise-equal ambiguous
    /// candidates, the one in the caller's own package is overwhelmingly the
    /// true callee of an unqualified same-name call, so the 1/N fan-out is
    /// collapsed toward it. Repo-root files (no separator) share depth 0, which
    /// leaves the keep-all fan-out untouched when there is no directory signal.
    /// Both `/` and `\` are accepted so a Windows-authored `file_path` is not a
    /// silent no-op (the ingest path normalizes elsewhere, this is defence in depth).
    fn shared_dir_depth(a: &str, b: &str) -> usize {
        fn dirs(p: &str) -> Vec<&str> {
            let is_sep = |c: char| c == '/' || c == '\\';
            match p.rsplit_once(is_sep) {
                Some((dir, _file)) => dir.split(is_sep).filter(|s| !s.is_empty()).collect(),
                None => Vec::new(),
            }
        }
        dirs(a)
            .iter()
            .zip(dirs(b).iter())
            .take_while(|(x, y)| x == y)
            .count()
    }

    /// Does an import `module` locator anchor a candidate `file`? (R4)
    ///
    /// An import path names WHERE a symbol comes from; in almost every language
    /// that path maps onto a file path (`crate::graph::sqlite_store` ↔
    /// `…/graph/sqlite_store.rs`, `graph.store` ↔ `…/graph/store.py`,
    /// `./graph/store` ↔ `…/graph/store.ts`, `com.example.Foo` ↔ `…/Foo.java`,
    /// `src/graph/foo.dart` ↔ `…/graph/foo.dart`). Both sides are normalized to
    /// lowercase segments (trailing code extension stripped) and matched on the
    /// file's tail: the module's last segment must equal the file stem AND — when
    /// both carry a parent segment — the parent must align too (rejecting a
    /// stem-only collision like two `store` files in different packages); or the
    /// file's `[parent, stem]` appears contiguously deeper in the module path.
    /// Conservative on purpose: a miss falls through to the proximity/keep-all
    /// tiers, never a wrong anchor.
    fn import_anchors_file(module: &str, file: &str) -> bool {
        fn strip_code_ext(s: &str) -> &str {
            for ext in [
                ".dart", ".rs", ".py", ".js", ".ts", ".tsx", ".jsx", ".go", ".java", ".kt", ".mjs",
                ".cjs",
            ] {
                if let Some(p) = s.strip_suffix(ext) {
                    return p;
                }
            }
            s
        }
        let msegs: Vec<String> = strip_code_ext(module)
            .split(|c| c == ':' || c == '.' || c == '/' || c == '\\')
            .map(|s| s.trim().to_ascii_lowercase())
            .filter(|s| !s.is_empty() && s != "." && s != "..")
            .collect();
        let raw: Vec<&str> = file
            .split(|c| c == '/' || c == '\\')
            .filter(|s| !s.is_empty())
            .collect();
        let fsegs: Vec<String> = raw
            .iter()
            .enumerate()
            .map(|(i, s)| {
                if i + 1 == raw.len() {
                    strip_code_ext(s).to_ascii_lowercase()
                } else {
                    s.to_ascii_lowercase()
                }
            })
            .collect();
        let (Some(stem), Some(mlast)) = (fsegs.last(), msegs.last()) else {
            return false;
        };
        if mlast == stem {
            // Stem matches; when both sides carry a parent, require it to align
            // too so a bare stem shared across packages does not false-anchor.
            if msegs.len() >= 2 && fsegs.len() >= 2 {
                return msegs[msegs.len() - 2] == fsegs[fsegs.len() - 2];
            }
            return true;
        }
        // The file's [parent, stem] appears contiguously deeper in the module.
        if fsegs.len() >= 2 {
            let parent = &fsegs[fsegs.len() - 2];
            return msegs.windows(2).any(|w| &w[0] == parent && &w[1] == stem);
        }
        false
    }

    /// Read the `module` field out of an IMPORTS edge's `metadata_json`
    /// (`{"module":"…"}`, written by `extract_imports_from_content`). Import
    /// locators never contain an unescaped quote, so a literal scan suffices.
    fn extract_module_field(metadata_json: &str) -> Option<String> {
        let key = "\"module\":\"";
        let start = metadata_json.find(key)? + key.len();
        let rest = &metadata_json[start..];
        let end = rest.find('"')?;
        Some(rest[..end].to_string())
    }

    /// Get a reference to the pool (for advanced queries in tests).
    pub fn pool(&self) -> &SqlitePool {
        &self.pool
    }
}

#[async_trait]
impl GraphStore for SqliteGraphStore {
    async fn upsert_node(&self, node: &GraphNode) -> GraphDbResult<()> {
        self.write_nodes(std::slice::from_ref(node)).await
    }

    async fn upsert_nodes(&self, nodes: &[GraphNode]) -> GraphDbResult<()> {
        self.write_nodes(nodes).await
    }

    async fn insert_edge(&self, edge: &GraphEdge) -> GraphDbResult<()> {
        self.write_edges(std::slice::from_ref(edge)).await
    }

    async fn insert_edges(&self, edges: &[GraphEdge]) -> GraphDbResult<()> {
        self.write_edges(edges).await
    }

    async fn replace_generation(
        &self,
        tenant_id: &str,
        file_path: &str,
        generation: &str,
        nodes: &[GraphNode],
        edges: &[GraphEdge],
    ) -> GraphDbResult<()> {
        self.replace_generation_rows(tenant_id, file_path, generation, nodes, edges)
            .await
    }

    async fn delete_generation(&self, tenant_id: &str, generation: &str) -> GraphDbResult<u64> {
        self.delete_generation_rows(tenant_id, generation).await
    }

    async fn generation_extracted(&self, tenant_id: &str, generation: &str) -> GraphDbResult<bool> {
        self.is_generation_extracted(tenant_id, generation).await
    }

    async fn extracted_generations(
        &self,
        tenant_id: &str,
    ) -> GraphDbResult<Vec<ExtractedGeneration>> {
        self.list_extracted_generations(tenant_id).await
    }

    async fn delete_tenant(&self, tenant_id: &str) -> GraphDbResult<u64> {
        self.delete_tenant_rows(tenant_id).await
    }

    async fn query_related(
        &self,
        tenant_id: &str,
        node_id: &str,
        max_hops: u32,
        edge_types: Option<&[EdgeType]>,
        scope: &GraphScope,
    ) -> GraphDbResult<Vec<TraversalNode>> {
        self.traverse_forward(tenant_id, node_id, max_hops, edge_types, scope)
            .await
    }

    async fn query_related_by_symbol(
        &self,
        tenant_id: &str,
        symbol_name: &str,
        file_path: Option<&str>,
        max_hops: u32,
        edge_types: Option<&[EdgeType]>,
        scope: &GraphScope,
    ) -> GraphDbResult<Vec<TraversalNode>> {
        self.traverse_forward_by_symbol(
            tenant_id,
            symbol_name,
            file_path,
            max_hops,
            edge_types,
            scope,
        )
        .await
    }

    async fn impact_analysis(
        &self,
        tenant_id: &str,
        symbol_name: &str,
        file_path: Option<&str>,
        scope: &GraphScope,
    ) -> GraphDbResult<ImpactReport> {
        self.impact(tenant_id, symbol_name, file_path, scope).await
    }

    async fn stats(
        &self,
        tenant_id: Option<&str>,
        scope: &GraphScope,
    ) -> GraphDbResult<GraphStats> {
        self.scoped_stats(tenant_id, scope).await
    }

    async fn prune_orphans(&self, tenant_id: &str) -> GraphDbResult<u64> {
        self.delete_orphan_nodes(tenant_id).await
    }

    async fn resolve_stub_edges(
        &self,
        tenant_id: &str,
        membership: &GenerationBranches,
    ) -> GraphDbResult<u64> {
        self.resolve_stubs(tenant_id, membership).await
    }

    async fn make_calls_authoritative(
        &self,
        tenant_id: &str,
        caller_id: &str,
        source_file: &str,
        generation: &str,
        resolved_names: &[String],
        precise_targets: &[String],
    ) -> GraphDbResult<u64> {
        self.supersede_fuzzy_calls(
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

#[cfg(test)]
mod matcher_tests {
    use super::SqliteGraphStore as S;

    #[test]
    fn import_anchors_file_matches_each_language_shape() {
        // Rust: `use crate::graph::sqlite_store::Foo` -> module `crate::graph::sqlite_store`.
        assert!(S::import_anchors_file(
            "crate::graph::sqlite_store",
            "src/rust/daemon/core/src/graph/sqlite_store.rs"
        ));
        // Python: `from graph.store import Foo`.
        assert!(S::import_anchors_file("graph.store", "app/graph/store.py"));
        // JS/TS relative import.
        assert!(S::import_anchors_file(
            "./graph/store",
            "src/graph/store.ts"
        ));
        // Java FQN -> file named after the class, package mirrors the path.
        assert!(S::import_anchors_file(
            "com.example.model.User",
            "src/main/java/com/example/model/User.java"
        ));
        // Dart URI import (extension stripped on both sides).
        assert!(S::import_anchors_file(
            "src/graph/foo.dart",
            "lib/src/graph/foo.dart"
        ));
        // Single-segment module (only a stem to go on).
        assert!(S::import_anchors_file("serde", "src/serde.rs"));
    }

    #[test]
    fn import_anchors_file_rejects_stem_only_collision() {
        // Same file stem `store` but a DIFFERENT parent package -> NOT anchored
        // (this is what keeps the R4 tier from false-anchoring across packages).
        assert!(!S::import_anchors_file("graph.store", "app/other/store.py"));
        // Unrelated import / file.
        assert!(!S::import_anchors_file(
            "react",
            "src/components/Button.tsx"
        ));
        // Module names a directory, not the file (`graph/mod.rs`) -> no stem match.
        assert!(!S::import_anchors_file("crate::graph", "src/graph/mod.rs"));
    }

    #[test]
    fn extract_module_field_reads_module_only() {
        assert_eq!(
            S::extract_module_field("{\"module\":\"crate::graph\"}").as_deref(),
            Some("crate::graph")
        );
        assert_eq!(
            S::extract_module_field("{\"module\":\"src/graph/foo.dart\"}").as_deref(),
            Some("src/graph/foo.dart")
        );
        // A resolution-metadata blob carries no module.
        assert_eq!(
            S::extract_module_field("{\"resolution\":\"import\",\"confidence\":0.9000}"),
            None
        );
    }
}
