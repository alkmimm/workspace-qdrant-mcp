//! Graph relationship extractor -- derives graph edges from SemanticChunk data.
//!
//! Takes tree-sitter `SemanticChunk` output and produces `GraphNode`/`GraphEdge`
//! pairs for CONTAINS, CALLS, IMPORTS, and USES_TYPE relationships.

mod fragments;
pub(crate) mod import_parsers;
mod kinds;
mod receivers;
mod reference_analysis;
mod type_analysis;

#[cfg(test)]
mod tests;

use std::collections::HashMap;

use crate::tree_sitter::types::{ChunkType, SemanticChunk};
use crate::TextChunk;

use super::{EdgeType, GraphEdge, GraphNode, NodeType};

use import_parsers::extract_imports_from_content;
use reference_analysis::extract_argument_references;
pub use type_analysis::{extract_type_references, parse_qualified_name};

pub(crate) use kinds::node_type_from_display_name;
use kinds::{chunk_type_to_node_type, infer_parent_node_type};
use receivers::{calls_edge, is_container, CallHints};

/// Result of extracting graph relationships from a set of semantic chunks.
#[derive(Debug, Default)]
pub struct ExtractionResult {
    pub nodes: Vec<GraphNode>,
    pub edges: Vec<GraphEdge>,
}

/// Extract all edges (CONTAINS, CALLS, USES_TYPE) for a single semantic chunk.
fn extract_chunk_edges(
    chunk: &SemanticChunk,
    node: &GraphNode,
    tenant_id: &str,
    file_path: &str,
    hints: &CallHints,
    result: &mut ExtractionResult,
) {
    if let Some(ref parent) = chunk.parent_symbol {
        let parent_type = infer_parent_node_type(parent, &chunk.language);
        let parent_node = GraphNode::stub(tenant_id, parent, parent_type);
        let edge = GraphEdge::new(
            tenant_id,
            &parent_node.node_id,
            &node.node_id,
            EdgeType::Contains,
            file_path,
        );
        result.nodes.push(parent_node);
        result.edges.push(edge);
    }

    for call in &chunk.calls {
        let (_qualifier, callee_name) = parse_qualified_name(call);
        if !is_valid_symbol_name(&callee_name) {
            // Skip tree-sitter artifacts like `<String` / `_>` that leak from
            // a turbofish/generic argument list (e.g. `query::<String, _>(...)`).
            continue;
        }
        let callee_stub = GraphNode::stub(tenant_id, &callee_name, NodeType::Function);
        let edge = calls_edge(
            tenant_id,
            node,
            &callee_stub,
            &callee_name,
            file_path,
            hints,
        );
        result.nodes.push(callee_stub);
        result.edges.push(edge);
    }

    // Symbols named in argument position without being invoked (#369). The
    // target is stubbed as a Constant because the population this addresses is
    // top-level bindings (Riverpod providers, feature flags); a stub that never
    // resolves to a file-backed symbol has its edge dropped by
    // `resolve_stub_edges` before it persists, which is what keeps this from
    // inflating the edge table — measured on DOC-V2, 51,199 candidates become
    // 1,719 persisted edges (+0.28% rather than +8.5%).
    for reference in extract_argument_references(&chunk.content, &chunk.language) {
        if !is_valid_symbol_name(&reference) {
            continue;
        }
        let ref_stub = GraphNode::stub(tenant_id, &reference, NodeType::Constant);
        let edge = GraphEdge::new(
            tenant_id,
            &node.node_id,
            &ref_stub.node_id,
            EdgeType::References,
            file_path,
        );
        result.nodes.push(ref_stub);
        result.edges.push(edge);
    }

    if let Some(ref sig) = chunk.signature {
        let type_refs = extract_type_references(sig, &chunk.language);
        for type_name in type_refs {
            if !is_valid_symbol_name(&type_name) {
                continue;
            }
            let type_stub = GraphNode::stub(tenant_id, &type_name, NodeType::Struct);
            let edge = GraphEdge::new(
                tenant_id,
                &node.node_id,
                &type_stub.node_id,
                EdgeType::UsesType,
                file_path,
            );
            result.nodes.push(type_stub);
            result.edges.push(edge);
        }
    }
}

/// Extract graph nodes and edges from semantic chunks for a single file.
///
/// This is the main entry point called during file ingestion. It processes
/// all chunks from a file and produces the full set of nodes and edges.
pub fn extract_edges(
    chunks: &[SemanticChunk],
    tenant_id: &str,
    file_path: &str,
) -> ExtractionResult {
    let mut result = ExtractionResult::default();

    // Create a File node for import edges. Stamp its language from the first
    // chunk that carries one so file-level nodes don't inflate the "unknown"
    // language bucket in per-language graph metrics.
    let mut file_node = GraphNode::new(tenant_id, file_path, file_path, NodeType::File);
    file_node.language = chunks
        .iter()
        .map(|c| c.language.clone())
        .find(|l| !l.is_empty());
    result.nodes.push(file_node);

    // A definition split into fragments is read whole (see `fragments`).
    let whole = fragments::whole_texts(chunks.iter().filter_map(fragments::semantic_fragment));
    let fields = receivers::field_types(
        chunks
            .iter()
            .filter(|c| chunk_type_to_node_type(&c.chunk_type).is_some_and(is_container))
            .filter(|c| !fragments::later_semantic_fragment(c))
            .map(|c| {
                (
                    c.symbol_name.as_str(),
                    fragments::semantic_whole(&whole, c).unwrap_or(&c.content),
                    c.language.as_str(),
                )
            }),
    );

    for chunk in chunks {
        let Some(node_type) = chunk_type_to_node_type(&chunk.chunk_type) else {
            // Preamble and Text chunks don't become nodes, but we still
            // extract imports from Preamble content below.
            if chunk.chunk_type == ChunkType::Preamble {
                extract_imports_from_content(
                    &chunk.content,
                    &chunk.language,
                    tenant_id,
                    file_path,
                    &mut result,
                );
            }
            continue;
        };

        // Create the node for this chunk. A member is keyed by its container
        // too: two classes in one file may declare the same method name.
        let mut node = GraphNode::member(
            tenant_id,
            file_path,
            &chunk.symbol_name,
            chunk.parent_symbol.as_deref(),
            node_type,
        );
        node.start_line = Some(chunk.start_line as u32);
        node.end_line = Some(chunk.end_line as u32);
        node.signature = chunk.signature.clone();
        node.language = Some(chunk.language.clone());
        // Rust inline unit test (`#[cfg(test)]` / `#[test]`-family) — tag the
        // node so test-gap detection seeds the BFS from it (see `GraphNode`).
        node.is_test_symbol = chunk.is_test;
        result.nodes.push(node.clone());

        let hints = receivers::chunk_hints(
            fragments::semantic_whole(&whole, chunk).unwrap_or(&chunk.content),
            &chunk.language,
            &chunk.calls,
            &chunk.symbol_name,
            chunk.parent_symbol.as_deref(),
            &fields,
        );
        extract_chunk_edges(chunk, &node, tenant_id, file_path, &hints, &mut result);
    }

    result.nodes = fragments::merge_fragment_nodes(std::mem::take(&mut result.nodes));
    result
}

/// Extract graph nodes and edges from `TextChunk` metadata maps.
///
/// This is the pipeline-integrated entry point. The document processor
/// converts `SemanticChunk` data to `TextChunk` with metadata strings
/// (`chunk_type`, `symbol_name`, `parent_symbol`, `calls`, `signature`,
/// `language`, `start_line`, `end_line`). This function reconstructs
/// graph relationships from those metadata maps.
pub fn extract_edges_from_text_chunks(
    chunks: &[TextChunk],
    tenant_id: &str,
    file_path: &str,
) -> ExtractionResult {
    let mut result = ExtractionResult::default();

    // Stamp the File node's language from the first chunk that carries one so
    // file-level nodes don't inflate the "unknown" language bucket in
    // per-language graph metrics (see `extract_edges` for the same fix on the
    // `SemanticChunk` entry point).
    let mut file_node = GraphNode::new(tenant_id, file_path, file_path, NodeType::File);
    file_node.language = chunks.iter().find_map(|c| {
        c.metadata
            .get("language")
            .filter(|l| !l.is_empty())
            .cloned()
    });
    result.nodes.push(file_node);

    // A definition split into fragments is read whole (see `fragments`).
    let whole = fragments::whole_texts(chunks.iter().filter_map(fragments::text_fragment));
    let fields = receivers::field_types(
        chunks
            .iter()
            .filter(|c| {
                meta_str(c, "chunk_type")
                    .and_then(node_type_from_display_name)
                    .is_some_and(is_container)
            })
            .filter(|c| !fragments::later_text_fragment(c))
            .filter_map(|c| {
                Some((
                    meta_str(c, "symbol_name")?,
                    fragments::text_whole(&whole, c).unwrap_or(&c.content),
                    meta_str(c, "language").unwrap_or(""),
                ))
            }),
    );

    for chunk in chunks {
        let text = fragments::text_whole(&whole, chunk).unwrap_or(&chunk.content);
        process_text_chunk(chunk, text, tenant_id, file_path, &fields, &mut result);
    }

    result.nodes = fragments::merge_fragment_nodes(std::mem::take(&mut result.nodes));
    result
}

fn meta_str<'a>(chunk: &'a TextChunk, key: &str) -> Option<&'a str> {
    chunk.metadata.get(key).map(String::as_str)
}

/// Process a single `TextChunk` into graph nodes and edges. `definition` is
/// the text receiver hints are read from: the whole definition when the
/// chunker split it, else the chunk's own content.
fn process_text_chunk(
    chunk: &TextChunk,
    definition: &str,
    tenant_id: &str,
    file_path: &str,
    fields: &HashMap<String, receivers::TypeMap>,
    result: &mut ExtractionResult,
) {
    let meta = &chunk.metadata;

    let chunk_type_str = match meta.get("chunk_type") {
        Some(s) => s.as_str(),
        None => return,
    };
    let Some(node_type) = node_type_from_display_name(chunk_type_str) else {
        if chunk_type_str == "preamble" {
            let language = meta.get("language").map(|s| s.as_str()).unwrap_or("");
            extract_imports_from_content(&chunk.content, language, tenant_id, file_path, result);
        }
        return;
    };

    let symbol_name = match meta.get("symbol_name") {
        Some(s) if !s.is_empty() => s,
        _ => return,
    };
    let language = meta.get("language").cloned().unwrap_or_default();

    // Keyed by its container too: two classes in one file may declare the
    // same method name (see `compute_member_node_id`).
    let parent = meta.get("parent_symbol").map(String::as_str);
    let mut node = GraphNode::member(tenant_id, file_path, symbol_name, parent, node_type);
    node.start_line = meta.get("start_line").and_then(|s| s.parse::<u32>().ok());
    node.end_line = meta.get("end_line").and_then(|s| s.parse::<u32>().ok());
    node.signature = meta.get("signature").cloned();
    node.language = Some(language.clone());
    // Rust inline unit test flag carried through the TextChunk metadata by
    // `convert_semantic_chunks_to_text_chunks` (see `GraphNode::is_test_symbol`).
    node.is_test_symbol = meta
        .get("is_test_symbol")
        .map(|s| s == "true")
        .unwrap_or(false);
    result.nodes.push(node.clone());

    add_contains_edges(meta, &node, tenant_id, file_path, &language, result);
    let calls: Vec<String> = meta
        .get("calls")
        .map(|s| {
            s.split(',')
                .map(str::trim)
                .filter(|c| !c.is_empty())
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default();
    let hints = receivers::chunk_hints(definition, &language, &calls, symbol_name, parent, fields);
    add_calls_edges(&calls, &node, tenant_id, file_path, &hints, result);
    add_uses_type_edges(meta, &node, tenant_id, file_path, &language, result);
}

fn add_contains_edges(
    meta: &std::collections::HashMap<String, String>,
    node: &GraphNode,
    tenant_id: &str,
    file_path: &str,
    language: &str,
    result: &mut ExtractionResult,
) {
    if let Some(parent) = meta.get("parent_symbol") {
        if !parent.is_empty() {
            let parent_type = infer_parent_node_type(parent, language);
            let parent_node = GraphNode::stub(tenant_id, parent, parent_type);
            let edge = GraphEdge::new(
                tenant_id,
                &parent_node.node_id,
                &node.node_id,
                EdgeType::Contains,
                file_path,
            );
            result.nodes.push(parent_node);
            result.edges.push(edge);
        }
    }
}

fn add_calls_edges(
    calls: &[String],
    node: &GraphNode,
    tenant_id: &str,
    file_path: &str,
    hints: &CallHints,
    result: &mut ExtractionResult,
) {
    for call in calls {
        let (_qualifier, callee_name) = parse_qualified_name(call);
        if !is_valid_symbol_name(&callee_name) {
            // Skip tree-sitter artifacts like `<String` / `_>` that leak from
            // a turbofish/generic argument list (e.g. `query::<String, _>(...)`).
            continue;
        }
        let callee_stub = GraphNode::stub(tenant_id, &callee_name, NodeType::Function);
        let edge = calls_edge(
            tenant_id,
            node,
            &callee_stub,
            &callee_name,
            file_path,
            hints,
        );
        result.nodes.push(callee_stub);
        result.edges.push(edge);
    }
}

fn add_uses_type_edges(
    meta: &std::collections::HashMap<String, String>,
    node: &GraphNode,
    tenant_id: &str,
    file_path: &str,
    language: &str,
    result: &mut ExtractionResult,
) {
    if let Some(sig) = meta.get("signature") {
        let type_refs = extract_type_references(sig, language);
        for type_name in type_refs {
            if !is_valid_symbol_name(&type_name) {
                continue;
            }
            let type_stub = GraphNode::stub(tenant_id, &type_name, NodeType::Struct);
            let edge = GraphEdge::new(
                tenant_id,
                &node.node_id,
                &type_stub.node_id,
                EdgeType::UsesType,
                file_path,
            );
            result.nodes.push(type_stub);
            result.edges.push(edge);
        }
    }
}

/// Reject parser artifacts before they become graph nodes or edge targets.
///
/// Tree-sitter call extraction can leak fragments of a generic/turbofish
/// argument list into the call list — e.g. `query::<String, _>(...)` can yield
/// `<String` and `_>` instead of `query`. Those are not real symbols, so we
/// only emit a CALLS/USES_TYPE stub when the derived name is a plain identifier
/// or a `::`-qualified path of identifiers.
fn is_valid_symbol_name(name: &str) -> bool {
    !name.is_empty() && name.split("::").all(is_plain_identifier)
}

/// True if `seg` is a single identifier: starts with a letter or `_`, followed
/// only by letters, digits, or `_`. Unicode letters/digits are accepted so that
/// non-ASCII identifiers are not dropped; the point is to reject characters like
/// `<`, `>`, and `,` that mark generic-argument artifacts.
fn is_plain_identifier(seg: &str) -> bool {
    let mut chars = seg.chars();
    match chars.next() {
        Some(c) if c.is_alphabetic() || c == '_' => {}
        _ => return false,
    }
    chars.all(|c| c.is_alphanumeric() || c == '_')
}
