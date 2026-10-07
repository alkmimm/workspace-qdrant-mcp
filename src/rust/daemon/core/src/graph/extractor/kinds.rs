//! Mapping chunk kinds to graph node kinds.

use crate::graph::NodeType;
use crate::tree_sitter::types::ChunkType;

/// Convert a `ChunkType::display_name()` string back to `NodeType`.
pub(crate) fn node_type_from_display_name(name: &str) -> Option<NodeType> {
    match name {
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
        "preamble" | "text" => None,
        _ => None,
    }
}

/// Convert ChunkType to NodeType. Returns None for types that don't map
/// to graph nodes (Preamble, Text).
pub(super) fn chunk_type_to_node_type(ct: &ChunkType) -> Option<NodeType> {
    match ct {
        ChunkType::Function => Some(NodeType::Function),
        ChunkType::AsyncFunction => Some(NodeType::AsyncFunction),
        ChunkType::Class => Some(NodeType::Class),
        ChunkType::Method => Some(NodeType::Method),
        ChunkType::Struct => Some(NodeType::Struct),
        ChunkType::Trait => Some(NodeType::Trait),
        ChunkType::Interface => Some(NodeType::Interface),
        ChunkType::Enum => Some(NodeType::Enum),
        ChunkType::Impl => Some(NodeType::Impl),
        ChunkType::Module => Some(NodeType::Module),
        ChunkType::Constant => Some(NodeType::Constant),
        ChunkType::TypeAlias => Some(NodeType::TypeAlias),
        ChunkType::Macro => Some(NodeType::Macro),
        ChunkType::Preamble | ChunkType::Text => None,
    }
}

/// Infer parent node type from symbol name and language.
///
/// In Rust, a parent is typically an `impl` block or `mod`.
/// In TypeScript/Python, a parent is typically a `class`.
pub(super) fn infer_parent_node_type(parent_symbol: &str, language: &str) -> NodeType {
    match language {
        "rust" => {
            // Rust parent symbols from tree-sitter are typically impl blocks
            if parent_symbol.starts_with("impl ") || parent_symbol.contains("::") {
                NodeType::Impl
            } else {
                NodeType::Struct
            }
        }
        "python" | "javascript" | "typescript" | "tsx" | "jsx" | "java" | "kotlin" => {
            NodeType::Class
        }
        "go" => NodeType::Struct,
        _ => NodeType::Module,
    }
}
