//! Graph database module for code relationship storage and querying.
//!
//! Provides a `GraphStore` trait abstracting graph operations, with
//! `SqliteGraphStore` (recursive CTEs) and `LadybugGraphStore` (Kuzu
//! fork, behind `ladybug` feature flag) implementations.
//!
//! Use `factory::create_sqlite_graph_store` or the LadybugDB variant
//! to instantiate the appropriate backend based on configuration.
//! The graph is stored in a dedicated `graph.db` file separate from
//! `state.db` to avoid lock contention with queue processing.
//!
//! Every row belongs to one content GENERATION of its file — the tracked
//! file's `base_point`, `SHA256(tenant|relative_path|file_hash)` — and a
//! generation's rows are written and deleted together. Which generations a
//! branch holds is NOT stored here: `tracked_files.branches` is the authority,
//! read at query time (`branch_scope`) and applied as a [`GraphScope`], so a
//! branch sees exactly the versions of its files and nothing a sibling branch
//! indexed later. Rows of the file-less stub nodes carry the empty generation
//! and are visible in every scope.

pub mod algorithms;
pub mod branch_scope;
pub mod extractor;
pub mod factory;
pub mod lsp_backfill;
pub mod lsp_sites;
pub mod maintenance;
pub mod migrator;
mod schema;
mod schema_v7;
mod schema_v8;
mod scope;
mod shared;
mod sqlite_store;
mod store;

#[cfg(feature = "ladybug")]
pub mod ladybug_store;

#[cfg(test)]
mod tests;

#[cfg(feature = "ladybug")]
pub use factory::create_ladybug_graph_store;
pub use factory::{create_sqlite_graph_store, GraphBackend, GraphConfig};
#[cfg(feature = "ladybug")]
pub use ladybug_store::{LadybugConfig, LadybugGraphStore};
pub use schema::{
    GraphDbError, GraphDbManager, GraphDbResult, GRAPH_DB_FILENAME, GRAPH_SCHEMA_VERSION,
};
pub use scope::{ExtractedGeneration, GenerationBranches, GraphScope};
pub use shared::{stamp_generation, SharedGraphStore};
pub use sqlite_store::SqliteGraphStore;
pub use store::GraphStore;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::fmt::Write;

/// Node types in the code graph, mapping to semantic chunk types.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NodeType {
    File,
    Function,
    AsyncFunction,
    Class,
    Method,
    Struct,
    Trait,
    Interface,
    Enum,
    Impl,
    Module,
    Constant,
    TypeAlias,
    Macro,
}

impl NodeType {
    pub fn as_str(&self) -> &'static str {
        match self {
            NodeType::File => "file",
            NodeType::Function => "function",
            NodeType::AsyncFunction => "async_function",
            NodeType::Class => "class",
            NodeType::Method => "method",
            NodeType::Struct => "struct",
            NodeType::Trait => "trait",
            NodeType::Interface => "interface",
            NodeType::Enum => "enum",
            NodeType::Impl => "impl",
            NodeType::Module => "module",
            NodeType::Constant => "constant",
            NodeType::TypeAlias => "type_alias",
            NodeType::Macro => "macro",
        }
    }

    pub fn from_str(s: &str) -> Option<Self> {
        match s {
            "file" => Some(NodeType::File),
            "function" => Some(NodeType::Function),
            "async_function" => Some(NodeType::AsyncFunction),
            "class" => Some(NodeType::Class),
            "method" => Some(NodeType::Method),
            "struct" => Some(NodeType::Struct),
            "trait" => Some(NodeType::Trait),
            "interface" => Some(NodeType::Interface),
            "enum" => Some(NodeType::Enum),
            "impl" => Some(NodeType::Impl),
            "module" => Some(NodeType::Module),
            "constant" => Some(NodeType::Constant),
            "type_alias" => Some(NodeType::TypeAlias),
            "macro" => Some(NodeType::Macro),
            _ => None,
        }
    }
}

impl std::fmt::Display for NodeType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Edge types representing relationships between code entities.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum EdgeType {
    /// Function/method call relationship.
    Calls,
    /// Parent-child containment (class contains method, impl contains fn).
    Contains,
    /// Import/use statement dependency.
    Imports,
    /// Type reference in signature (parameter types, return types).
    UsesType,
    /// Class/trait inheritance.
    Extends,
    /// Trait/interface implementation.
    Implements,
    /// A symbol named in argument position without being invoked —
    /// `ref.watch(activeContextProvider)`, `find.byType(HomePage)`.
    ///
    /// This is neither a call (the call is to `watch`) nor a type use, so
    /// before this edge existed such a symbol had NO incoming edge at all and
    /// `usages` answered 0 for it. Measured on DOC-V2: 1 of 506 Dart top-level
    /// constants had any inbound edge, while `activeContextProvider` alone is
    /// named in argument position 16 times.
    ///
    /// Deliberately NOT part of the test-gap edge defaults: "referenced" is a
    /// weaker claim than "exercised", and widening the coverage denominator is
    /// a separate judgement from fixing `usages`.
    References,
}

impl EdgeType {
    pub fn as_str(&self) -> &'static str {
        match self {
            EdgeType::Calls => "CALLS",
            EdgeType::Contains => "CONTAINS",
            EdgeType::Imports => "IMPORTS",
            EdgeType::UsesType => "USES_TYPE",
            EdgeType::Extends => "EXTENDS",
            EdgeType::Implements => "IMPLEMENTS",
            EdgeType::References => "REFERENCES",
        }
    }

    pub fn from_str(s: &str) -> Option<Self> {
        match s {
            "CALLS" => Some(EdgeType::Calls),
            "CONTAINS" => Some(EdgeType::Contains),
            "IMPORTS" => Some(EdgeType::Imports),
            "USES_TYPE" => Some(EdgeType::UsesType),
            "EXTENDS" => Some(EdgeType::Extends),
            "IMPLEMENTS" => Some(EdgeType::Implements),
            "REFERENCES" => Some(EdgeType::References),
            _ => None,
        }
    }
}

impl std::fmt::Display for EdgeType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// A node in the code graph representing a code entity.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GraphNode {
    pub node_id: String,
    pub tenant_id: String,
    pub symbol_name: String,
    pub symbol_type: NodeType,
    pub file_path: String,
    pub start_line: Option<u32>,
    pub end_line: Option<u32>,
    pub signature: Option<String>,
    pub language: Option<String>,
    /// True when this symbol is TEST code independent of its file path — a Rust
    /// inline unit test (`#[cfg(test)]` module or `#[test]`-family attribute).
    /// Set from the chunk's `is_test` during extraction; lets call-graph
    /// test-gap detection seed the BFS from inline tests that live in a
    /// production `.rs` file. Stubs and file nodes are always `false`.
    #[serde(default)]
    pub is_test_symbol: bool,
    /// The content generation (`base_point`) whose extraction defined this
    /// node; empty for a file-less stub and for a node another file's
    /// extraction only referred to. Stamped by `replace_generation`.
    #[serde(default)]
    pub generation: String,
    /// The container (class, struct, impl, …) a member is declared in; `None`
    /// for top-level symbols, files and stubs. Part of the member's identity:
    /// see [`compute_member_node_id`].
    #[serde(default)]
    pub parent_symbol: Option<String>,
}

impl GraphNode {
    /// Create a new graph node, computing the node_id deterministically.
    pub fn new(
        tenant_id: impl Into<String>,
        file_path: impl Into<String>,
        symbol_name: impl Into<String>,
        symbol_type: NodeType,
    ) -> Self {
        Self::member(tenant_id, file_path, symbol_name, None, symbol_type)
    }

    /// Create a node declared inside `parent` (a class member), or a top-level
    /// one when `parent` is `None` — the identity then matches [`Self::new`].
    pub fn member(
        tenant_id: impl Into<String>,
        file_path: impl Into<String>,
        symbol_name: impl Into<String>,
        parent: Option<&str>,
        symbol_type: NodeType,
    ) -> Self {
        let tenant_id = tenant_id.into();
        let file_path = file_path.into();
        let symbol_name = symbol_name.into();
        let parent_symbol = parent.filter(|p| !p.is_empty()).map(str::to_string);
        let node_id = compute_member_node_id(
            &tenant_id,
            &file_path,
            parent_symbol.as_deref(),
            &symbol_name,
            symbol_type,
        );
        Self {
            node_id,
            tenant_id,
            symbol_name,
            symbol_type,
            file_path,
            start_line: None,
            end_line: None,
            signature: None,
            language: None,
            is_test_symbol: false,
            generation: String::new(),
            parent_symbol,
        }
    }

    /// Create a stub node (unresolved target — only name and type known).
    pub fn stub(
        tenant_id: impl Into<String>,
        symbol_name: impl Into<String>,
        symbol_type: NodeType,
    ) -> Self {
        let tenant_id = tenant_id.into();
        let symbol_name = symbol_name.into();
        // Stub nodes use empty file_path — updated when the target file is processed
        let node_id = compute_node_id(&tenant_id, "", &symbol_name, symbol_type);
        Self {
            node_id,
            tenant_id,
            symbol_name,
            symbol_type,
            file_path: String::new(),
            start_line: None,
            end_line: None,
            signature: None,
            language: None,
            is_test_symbol: false,
            generation: String::new(),
            parent_symbol: None,
        }
    }
}

/// An edge in the code graph representing a relationship between entities.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GraphEdge {
    pub edge_id: String,
    pub tenant_id: String,
    pub source_node_id: String,
    pub target_node_id: String,
    pub edge_type: EdgeType,
    /// The file whose extraction produced this edge.
    pub source_file: String,
    pub weight: f64,
    pub metadata_json: Option<String>,
    /// The content generation of `source_file` that owns this edge: the edge
    /// is visible exactly where that generation is. Stamped by
    /// `replace_generation`.
    #[serde(default)]
    pub generation: String,
}

impl GraphEdge {
    /// Create a new edge, computing the edge_id deterministically.
    pub fn new(
        tenant_id: impl Into<String>,
        source_node_id: impl Into<String>,
        target_node_id: impl Into<String>,
        edge_type: EdgeType,
        source_file: impl Into<String>,
    ) -> Self {
        let source_node_id = source_node_id.into();
        let target_node_id = target_node_id.into();
        let edge_id = compute_edge_id(&source_node_id, &target_node_id, edge_type);
        Self {
            edge_id,
            tenant_id: tenant_id.into(),
            source_node_id,
            target_node_id,
            edge_type,
            source_file: source_file.into(),
            weight: 1.0,
            metadata_json: None,
            generation: String::new(),
        }
    }
}

/// A node encountered during graph traversal, with path context.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TraversalNode {
    pub node_id: String,
    pub symbol_name: String,
    pub symbol_type: String,
    pub file_path: String,
    pub edge_type: String,
    pub depth: u32,
    pub path: String,
    /// Resolution confidence of the edge(s) traversed to reach this node, in
    /// [0,1]: the product of edge weights along the best path. 1.0 = precise
    /// (own-file/pre-R1), 0.95 = same-class scope (R2), 0.7 = tenant-unique,
    /// <0.6 = one of N ambiguous same-name candidates (R1 fan-out). Lets a
    /// caller rank/filter usages by how sure the resolver was.
    pub confidence: f64,
    /// The class (struct, impl, …) the symbol is a member of, so two
    /// homonymous methods read as `Batch.set` and `Transaction.set`.
    #[serde(default)]
    pub parent_symbol: Option<String>,
}

/// Reverse-walk depth of `impact` when the caller does not ask for one.
pub const DEFAULT_IMPACT_HOPS: u32 = 3;
/// Deepest reverse walk `impact` accepts (the same ceiling as `relations`).
pub const MAX_IMPACT_HOPS: u32 = 5;

/// Result of an impact analysis query.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ImpactReport {
    pub symbol_name: String,
    pub impacted_nodes: Vec<ImpactNode>,
    pub total_impacted: u32,
    /// Depth the reverse walk went to.
    #[serde(default)]
    pub max_hops: u32,
    /// Callers left out because the only edge reaching them is an ambiguous
    /// same-name guess (weight below the floor a pinned `file_path` applies).
    /// Reported because the cut is otherwise invisible: the answer simply
    /// reads as complete.
    #[serde(default)]
    pub dropped_below_floor: u32,
    /// The walk stopped at its node budget: the blast radius is truncated.
    #[serde(default)]
    pub node_budget_reached: bool,
}

/// A node impacted by a symbol change.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ImpactNode {
    pub node_id: String,
    pub symbol_name: String,
    /// The class the symbol is a member of (see [`TraversalNode`]).
    #[serde(default)]
    pub parent_symbol: Option<String>,
    pub file_path: String,
    pub impact_type: String,
    pub distance: u32,
    /// Resolution confidence of the path from the changed symbol to this node,
    /// in [0,1]: the product of edge weights along the best reverse path. 1.0 =
    /// precise, 0.95 = same-class scope (R2), 0.7 = tenant-unique, <0.6 = one of
    /// N ambiguous same-name candidates (R1 fan-out). Lets a caller distinguish
    /// a sure caller from a name-collision guess in the blast radius.
    pub confidence: f64,
}

/// Graph statistics.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct GraphStats {
    pub total_nodes: u64,
    pub total_edges: u64,
    pub nodes_by_type: std::collections::HashMap<String, u64>,
    pub edges_by_type: std::collections::HashMap<String, u64>,
}

/// Compute deterministic node ID from its identifying fields.
pub fn compute_node_id(
    tenant_id: &str,
    file_path: &str,
    symbol_name: &str,
    symbol_type: NodeType,
) -> String {
    let input = format!(
        "{}|{}|{}|{}",
        tenant_id,
        file_path,
        symbol_name,
        symbol_type.as_str()
    );
    let hash = Sha256::digest(input.as_bytes());
    let mut out = String::with_capacity(32);
    for b in &hash[..16] {
        let _ = write!(out, "{:02x}", b);
    }
    out
}

/// Node ID of a symbol declared inside `parent` (a class member), or of a
/// top-level one when `parent` is `None` (then identical to [`compute_node_id`]).
///
/// The container is part of the identity. Until graph.db v8 it was not, so two
/// classes in one file that declared a method of the same name collapsed into
/// ONE node, and the last written won. Live 2026-10-07: Finance's
/// `firestore_finance_writes.dart` declares `set`, `update`, `delete` and
/// `_touch` in both `FirestoreFinanceBatch` and `FirestoreFinanceTransaction`;
/// the graph held only the Transaction's, so every call to the Batch's `set`
/// landed on the other class and test_gaps reported it as untested.
pub fn compute_member_node_id(
    tenant_id: &str,
    file_path: &str,
    parent: Option<&str>,
    symbol_name: &str,
    symbol_type: NodeType,
) -> String {
    match parent.filter(|p| !p.is_empty()) {
        Some(parent) => compute_node_id(
            tenant_id,
            file_path,
            &format!("{parent}.{symbol_name}"),
            symbol_type,
        ),
        None => compute_node_id(tenant_id, file_path, symbol_name, symbol_type),
    }
}

/// Compute deterministic edge ID from source, target, and type.
pub fn compute_edge_id(source_node_id: &str, target_node_id: &str, edge_type: EdgeType) -> String {
    let input = format!(
        "{}|{}|{}",
        source_node_id,
        target_node_id,
        edge_type.as_str()
    );
    let hash = Sha256::digest(input.as_bytes());
    let mut out = String::with_capacity(32);
    for b in &hash[..16] {
        let _ = write!(out, "{:02x}", b);
    }
    out
}
