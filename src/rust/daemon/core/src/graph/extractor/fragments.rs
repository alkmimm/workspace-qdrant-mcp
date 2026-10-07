//! A definition the chunker split into fragments (size limits) is ONE graph
//! node: every fragment carries the same identity and so the same node id,
//! only fragment 0 carries the call list, and each fragment holds only part of
//! the body. Read fragment by fragment, a call deep in a 700-line Dart test
//! `main()` was looked up in fragment 0's text alone and lost its receiver
//! hint (Finance, 2026-10-07: `useCases.cancel(…)` 100 lines below
//! `final useCases = InterTenantTransferUseCases(…)`), and the node row kept
//! the start line of whichever fragment was written last — so the LSP
//! backfill asked the server about a line in the middle of the body.

use std::collections::HashMap;

use crate::graph::GraphNode;
use crate::tree_sitter::types::SemanticChunk;
use crate::TextChunk;

/// A definition's identity within one file: (kind, name, container).
pub(super) type DefinitionKey = (String, String, Option<String>);

/// One fragment of a definition: its index and its text.
pub(super) struct Fragment<'a> {
    key: DefinitionKey,
    index: usize,
    content: &'a str,
}

/// The fragment a pipeline chunk is, if the chunker split its definition.
pub(super) fn text_fragment(chunk: &TextChunk) -> Option<Fragment<'_>> {
    let meta = &chunk.metadata;
    if meta.get("is_fragment").map(String::as_str) != Some("true") {
        return None;
    }
    Some(Fragment {
        key: (
            meta.get("chunk_type")?.clone(),
            meta.get("symbol_name")?.clone(),
            meta.get("parent_symbol").cloned(),
        ),
        index: meta.get("fragment_index")?.parse().ok()?,
        content: &chunk.content,
    })
}

/// The fragment a semantic chunk is, if the chunker split its definition.
pub(super) fn semantic_fragment(chunk: &SemanticChunk) -> Option<Fragment<'_>> {
    if !chunk.is_fragment {
        return None;
    }
    Some(Fragment {
        key: semantic_key(chunk),
        index: chunk.fragment_index?,
        content: &chunk.content,
    })
}

/// A semantic chunk's definition identity, as `text_fragment` keys it.
fn semantic_key(chunk: &SemanticChunk) -> DefinitionKey {
    (
        chunk.chunk_type.display_name().to_string(),
        chunk.symbol_name.clone(),
        chunk.parent_symbol.clone(),
    )
}

/// The whole text of the split definition a pipeline chunk is a fragment of.
pub(super) fn text_whole<'w>(
    whole: &'w HashMap<DefinitionKey, String>,
    chunk: &TextChunk,
) -> Option<&'w str> {
    whole.get(&text_fragment(chunk)?.key).map(String::as_str)
}

/// The whole text of the split definition a semantic chunk is a fragment of.
pub(super) fn semantic_whole<'w>(
    whole: &'w HashMap<DefinitionKey, String>,
    chunk: &SemanticChunk,
) -> Option<&'w str> {
    whole
        .get(&semantic_fragment(chunk)?.key)
        .map(String::as_str)
}

/// A later fragment of a split definition: its text is already part of the
/// definition's whole text, read through fragment 0.
pub(super) fn later_text_fragment(chunk: &TextChunk) -> bool {
    text_fragment(chunk).is_some_and(|f| f.index > 0)
}

/// `later_text_fragment` for a semantic chunk.
pub(super) fn later_semantic_fragment(chunk: &SemanticChunk) -> bool {
    semantic_fragment(chunk).is_some_and(|f| f.index > 0)
}

/// The whole text of every fragmented definition: its fragments joined in
/// fragment order. Consecutive fragments overlap by a few lines; a line the
/// overlap repeats repeats a call site with the same receiver — harmless.
pub(super) fn whole_texts<'a>(
    fragments: impl Iterator<Item = Fragment<'a>>,
) -> HashMap<DefinitionKey, String> {
    let mut parts: HashMap<DefinitionKey, Vec<(usize, &str)>> = HashMap::new();
    for f in fragments {
        parts.entry(f.key).or_default().push((f.index, f.content));
    }
    parts
        .into_iter()
        .map(|(key, mut texts)| {
            texts.sort_by_key(|(index, _)| *index);
            let joined: Vec<&str> = texts.into_iter().map(|(_, text)| text).collect();
            (key, joined.join("\n"))
        })
        .collect()
}

/// One row per definition: rows sharing a node id (a split definition's
/// fragments) merge into the first, spanning the earliest start to the latest
/// end. File-less stubs are left as they are.
pub(super) fn merge_fragment_nodes(nodes: Vec<GraphNode>) -> Vec<GraphNode> {
    let mut merged: Vec<GraphNode> = Vec::with_capacity(nodes.len());
    let mut at: HashMap<String, usize> = HashMap::new();
    for node in nodes {
        if node.file_path.is_empty() {
            merged.push(node);
            continue;
        }
        let Some(&i) = at.get(&node.node_id) else {
            at.insert(node.node_id.clone(), merged.len());
            merged.push(node);
            continue;
        };
        let kept = &mut merged[i];
        kept.start_line = match (kept.start_line, node.start_line) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (a, b) => a.or(b),
        };
        kept.end_line = kept.end_line.max(node.end_line);
        if kept.signature.is_none() {
            kept.signature = node.signature;
        }
        kept.is_test_symbol |= node.is_test_symbol;
    }
    merged
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::graph::extractor::extract_edges_from_text_chunks;
    use crate::graph::{EdgeType, NodeType};

    fn fragment_chunk(index: usize, lines: (u32, u32), content: &str, calls: &str) -> TextChunk {
        let mut metadata: HashMap<String, String> = [
            ("chunk_type", "function"),
            ("symbol_name", "main"),
            ("language", "dart"),
            ("is_fragment", "true"),
            ("total_fragments", "3"),
        ]
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();
        metadata.insert("fragment_index".into(), index.to_string());
        metadata.insert("start_line".into(), lines.0.to_string());
        metadata.insert("end_line".into(), lines.1.to_string());
        if !calls.is_empty() {
            metadata.insert("calls".into(), calls.into());
        }
        TextChunk {
            content: content.to_string(),
            chunk_index: index,
            start_char: 0,
            end_char: content.len(),
            metadata,
        }
    }

    /// Finance's `inter_tenant_transfer_test.dart` shape: `main()` split in
    /// three, the call list on fragment 0, the declaration in fragment 1 and
    /// the calls in fragment 2.
    #[test]
    fn a_split_definitions_calls_are_typed_from_its_whole_text() {
        let chunks = [
            fragment_chunk(
                0,
                (20, 120),
                "void main() {\n  test('a', () {",
                "cancel,fromJson",
            ),
            fragment_chunk(
                1,
                (118, 230),
                "    final useCases = InterTenantTransferUseCases(repo);",
                "",
            ),
            fragment_chunk(
                2,
                (228, 311),
                "    useCases.cancel(uid: u);\n    CashFlowBackup.fromJson(m);\n  });\n}",
                "",
            ),
        ];
        let result = extract_edges_from_text_chunks(&chunks, "t", "test/a_test.dart");
        let meta = |callee: &str| {
            let stub = GraphNode::stub("t", callee, NodeType::Function).node_id;
            result
                .edges
                .iter()
                .find(|e| e.edge_type == EdgeType::Calls && e.target_node_id == stub)
                .and_then(|e| e.metadata_json.clone())
                .unwrap_or_default()
        };
        assert_eq!(
            meta("cancel"),
            r#"{"receiver_types":["InterTenantTransferUseCases"]}"#
        );
        assert_eq!(
            meta("fromJson"),
            r#"{"static_receivers":["CashFlowBackup"]}"#
        );

        let main: Vec<_> = result
            .nodes
            .iter()
            .filter(|n| n.symbol_name == "main")
            .collect();
        assert_eq!(main.len(), 1, "one node for the split definition");
        assert_eq!(
            (main[0].start_line, main[0].end_line),
            (Some(20), Some(311))
        );
    }

    #[test]
    fn fragments_join_in_order_whatever_order_they_arrive_in() {
        let key = || ("function".to_string(), "main".to_string(), None);
        let whole = whole_texts(
            [
                Fragment {
                    key: key(),
                    index: 1,
                    content: "  useCases.cancel();\n}",
                },
                Fragment {
                    key: key(),
                    index: 0,
                    content: "void main() {\n  final useCases = UseCases();",
                },
            ]
            .into_iter(),
        );
        assert_eq!(
            whole[&key()],
            "void main() {\n  final useCases = UseCases();\n  useCases.cancel();\n}"
        );
    }

    #[test]
    fn a_split_definition_is_one_node_spanning_every_fragment() {
        let fragment = |start, end| {
            let mut n = GraphNode::new("t", "test/a_test.dart", "main", NodeType::Function);
            n.start_line = Some(start);
            n.end_line = Some(end);
            n
        };
        let mut first = fragment(20, 120);
        first.signature = Some("void main()".to_string());
        let stub = GraphNode::stub("t", "cancel", NodeType::Function);
        let nodes = vec![
            first,
            stub.clone(),
            fragment(118, 230),
            stub,
            fragment(228, 311),
        ];
        let merged = merge_fragment_nodes(nodes);
        let main: Vec<_> = merged.iter().filter(|n| n.symbol_name == "main").collect();
        assert_eq!(main.len(), 1);
        assert_eq!(
            (main[0].start_line, main[0].end_line),
            (Some(20), Some(311))
        );
        assert_eq!(main[0].signature.as_deref(), Some("void main()"));
        assert_eq!(merged.len(), 3, "stubs are left as they are");
    }
}
