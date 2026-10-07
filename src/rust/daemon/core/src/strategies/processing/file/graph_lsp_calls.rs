//! The additive LSP precision pass over one file's graph extraction.
//!
//! Tree-sitter emits a name-only stub for every call. When a language server
//! is already warm for the file, call hierarchy tells where each callee is
//! defined; the pass writes those sites onto the caller's CALLS stubs and the
//! stub resolver binds each one to the definition at its site
//! (`graph::lsp_sites`) — precise where the server answered, the by-name
//! tiers where it did not. Gated on server readiness, so it is a no-op on a
//! cold index and never adds latency to the common path.

use std::collections::BTreeMap;
use std::path::Path;

use tracing::debug;

use crate::context::ProcessingContext;
use crate::graph::extractor::{node_type_from_display_name, ExtractionResult};
use crate::graph::lsp_sites::{with_lsp_sites, LspSite};
use crate::graph::{compute_member_node_id, EdgeType, GraphEdge, GraphNode, NodeType};
use crate::lsp::{call_sites_by_name, symbol_column_in_line};
use crate::TextChunk;

/// Ask the warm server for each callable chunk's outgoing calls and locate
/// them on the extraction's CALLS stubs. No-op when no server is ready for
/// the file (cold index / LSP disabled).
pub(super) async fn resolve_calls_via_lsp(
    ctx: &ProcessingContext,
    tenant_id: &str,
    file_path: &str,
    abs_file_path: &str,
    chunks: &[TextChunk],
    extraction: &mut ExtractionResult,
) {
    let Some(ref lsp_arc) = ctx.lsp_manager else {
        return;
    };
    let abs_path = Path::new(abs_file_path);
    let mgr = lsp_arc.read().await;
    if !mgr.is_server_ready_for_file(tenant_id, abs_path).await {
        return; // Server not warm for this file — tree-sitter edges stand.
    }
    let Some(project_root) = project_root_of(abs_file_path, file_path) else {
        return; // Can't derive root (path layout unexpected) — skip safely.
    };

    // Open the file so the server answers call-hierarchy for it (didOpen; most
    // servers only serve open documents). One open per file, closed after.
    let _ = mgr.open_document(abs_path).await;
    // Dart answers `outgoingCalls` with an EMPTY result while a freshly opened
    // document is still being analyzed; wait for idle as the backfill does
    // (`graph::lsp_backfill`). Servers whose only progress is background
    // indexing return right after a short settle.
    mgr.wait_for_analysis_idle(abs_path).await;

    for chunk in chunks {
        let Some(callable) = callable_at(chunk) else {
            continue;
        };
        let calls = mgr
            .resolved_outgoing_calls(abs_path, callable.line, callable.column)
            .await
            .unwrap_or_default();
        let sites = call_sites_by_name(&project_root, &calls);
        if sites.is_empty() {
            continue;
        }
        let caller_id = compute_member_node_id(
            tenant_id,
            file_path,
            callable.parent,
            callable.symbol,
            callable.node_type,
        );
        locate_calls(extraction, tenant_id, &caller_id, file_path, &sites);
        debug!(
            "Graph LSP pass: located {} called name(s) from {}",
            sites.len(),
            callable.symbol
        );
    }
    // Close the document opened above.
    let _ = mgr.close_document(abs_path).await;
}

/// The project root: the absolute path minus the project-relative suffix.
/// LSP-returned callee paths are relativized against it.
fn project_root_of(abs_file_path: &str, file_path: &str) -> Option<String> {
    let norm_abs = abs_file_path.replace('\\', "/");
    let norm_rel = file_path.replace('\\', "/");
    norm_abs
        .strip_suffix(&norm_rel)
        .map(|r| r.trim_end_matches('/').to_string())
}

/// A callable definition chunk, keyed as the extractor keys it, and where
/// its name sits for `prepareCallHierarchy`.
struct Callable<'c> {
    parent: Option<&'c str>,
    symbol: &'c str,
    node_type: NodeType,
    /// 0-indexed line and UTF-16 column of the name (LSP positions).
    line: u32,
    column: u32,
}

/// Only callable definitions have outgoing calls.
fn callable_at(chunk: &TextChunk) -> Option<Callable<'_>> {
    let meta = &chunk.metadata;
    let chunk_type = meta.get("chunk_type")?;
    if !matches!(
        chunk_type.as_str(),
        "function" | "async_function" | "method"
    ) {
        return None;
    }
    let symbol = meta.get("symbol_name").filter(|s| !s.is_empty())?;
    let start_line = meta.get("start_line")?.parse::<u32>().ok()?;
    let first_line = chunk.content.lines().next().unwrap_or("");
    Some(Callable {
        parent: meta.get("parent_symbol").map(String::as_str),
        symbol,
        node_type: node_type_from_display_name(chunk_type)?,
        // start_line is 1-indexed; LSP positions are 0-indexed.
        line: start_line.saturating_sub(1),
        column: symbol_column_in_line(first_line, symbol),
    })
}

/// R8.1 — the server is authoritative for the calls it resolved: write each
/// called name's definition sites onto `caller_id`'s CALLS stub, so the
/// resolver binds it to the definition at the site instead of fanning the
/// name out (a name resolved only into a dependency carries no site and
/// binds to nothing of ours). A call the server resolved but tree-sitter
/// never saw (a constructor the server names `__init__`) gets its own stub,
/// marked server-only. Names the server did not resolve keep their plain
/// stub — the by-name fallback. No node is minted: the server does not say
/// what kind of symbol the callee is, and a method's id includes its class.
fn locate_calls(
    extraction: &mut ExtractionResult,
    tenant_id: &str,
    caller_id: &str,
    file_path: &str,
    sites_by_name: &BTreeMap<String, Vec<LspSite>>,
) {
    for (name, sites) in sites_by_name {
        let stub = GraphNode::stub(tenant_id, name, NodeType::Function);
        let seen = extraction.edges.iter_mut().find(|e| {
            e.edge_type == EdgeType::Calls
                && e.source_node_id == caller_id
                && e.target_node_id == stub.node_id
        });
        if let Some(edge) = seen {
            edge.metadata_json = Some(with_lsp_sites(edge.metadata_json.as_deref(), sites, false));
        } else if !sites.is_empty() {
            let mut edge = GraphEdge::new(
                tenant_id,
                caller_id,
                &stub.node_id,
                EdgeType::Calls,
                file_path,
            );
            edge.metadata_json = Some(with_lsp_sites(None, sites, true));
            extraction.edges.push(edge);
            extraction.nodes.push(stub);
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::*;
    use crate::graph::compute_node_id;
    use crate::graph::lsp_sites::{lsp_sites, LspSites};

    const T: &str = "t1";
    const FILE: &str = "lib/a.dart";

    fn stub_id(name: &str) -> String {
        compute_node_id(T, "", name, NodeType::Function)
    }

    fn sites(entries: &[(&str, &[(&str, u32)])]) -> BTreeMap<String, Vec<LspSite>> {
        entries
            .iter()
            .map(|(name, at)| {
                let at = at.iter().map(|(f, l)| (f.to_string(), *l)).collect();
                (name.to_string(), at)
            })
            .collect()
    }

    fn located(extraction: &ExtractionResult, source: &str, name: &str) -> Option<LspSites> {
        extraction
            .edges
            .iter()
            .find(|e| e.source_node_id == source && e.target_node_id == stub_id(name))
            .and_then(|e| lsp_sites(e.metadata_json.as_deref()))
    }

    #[test]
    fn located_calls_annotate_the_callers_stubs_and_mint_no_node() {
        let caller = compute_member_node_id(T, FILE, Some("Repo"), "save", NodeType::Method);
        let other = compute_node_id(T, FILE, "other", NodeType::Function);
        let mut typed = GraphEdge::new(T, &caller, stub_id("set"), EdgeType::Calls, FILE);
        typed.metadata_json = Some(r#"{"receiver_types":["Batch"]}"#.to_string());
        let mut extraction = ExtractionResult {
            nodes: Vec::new(),
            edges: vec![
                typed,
                GraphEdge::new(T, &caller, stub_id("print"), EdgeType::Calls, FILE),
                GraphEdge::new(T, &caller, stub_id("local"), EdgeType::Calls, FILE),
                GraphEdge::new(T, &other, stub_id("set"), EdgeType::Calls, FILE),
            ],
        };
        let found = sites(&[
            ("set", &[("lib/writes.dart", 2), ("lib/writes.dart", 10)]),
            ("print", &[]),
        ]);
        locate_calls(&mut extraction, T, &caller, FILE, &found);

        let set = located(&extraction, &caller, "set").expect("set located");
        assert_eq!(set.sites.len(), 2);
        assert!(!set.lsp_only, "tree-sitter saw this call");
        let typed_meta = extraction.edges[0].metadata_json.as_deref().unwrap();
        assert!(typed_meta.contains("receiver_types"), "{typed_meta}");
        assert_eq!(
            located(&extraction, &caller, "print").map(|s| s.sites),
            Some(Vec::new()),
            "resolved into a dependency: known not ours"
        );
        assert_eq!(located(&extraction, &caller, "local"), None);
        assert_eq!(located(&extraction, &other, "set"), None, "another caller");
        assert!(extraction.nodes.is_empty(), "no guessed callee node");
        assert_eq!(extraction.edges.len(), 4);
    }

    #[test]
    fn a_call_only_the_server_saw_gets_its_own_marked_stub() {
        let caller = compute_node_id(T, "app/main.py", "run", NodeType::Function);
        let mut extraction = ExtractionResult::default();
        let found = sites(&[("__init__", &[("app/models.py", 7)]), ("len", &[])]);
        locate_calls(&mut extraction, T, &caller, "app/main.py", &found);

        assert!(located(&extraction, &caller, "__init__").unwrap().lsp_only);
        assert_eq!(extraction.edges.len(), 1, "no stub for a dependency call");
        assert_eq!(extraction.nodes.len(), 1);
        assert_eq!(
            extraction.nodes[0].file_path, "",
            "a stub, not a definition"
        );
    }

    fn chunk(meta: &[(&str, &str)], content: &str) -> TextChunk {
        TextChunk {
            content: content.to_string(),
            chunk_index: 0,
            start_char: 0,
            end_char: content.len(),
            metadata: meta
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect::<HashMap<_, _>>(),
        }
    }

    #[test]
    fn callables_are_keyed_like_the_extractor_and_positioned_for_the_server() {
        let method = chunk(
            &[
                ("chunk_type", "method"),
                ("symbol_name", "set"),
                ("parent_symbol", "Batch"),
                ("start_line", "12"),
            ],
            "  void set(T data) {}",
        );
        let at = callable_at(&method).expect("a method is callable");
        assert_eq!(
            (at.parent, at.symbol, at.node_type),
            (Some("Batch"), "set", NodeType::Method)
        );
        assert_eq!((at.line, at.column), (11, 7));

        let class = chunk(
            &[
                ("chunk_type", "class"),
                ("symbol_name", "Batch"),
                ("start_line", "3"),
            ],
            "class Batch {}",
        );
        assert!(callable_at(&class).is_none());
        let no_line = chunk(
            &[("chunk_type", "function"), ("symbol_name", "f")],
            "f() {}",
        );
        assert!(callable_at(&no_line).is_none());
    }

    #[test]
    fn the_project_root_is_the_absolute_path_minus_the_relative_one() {
        assert_eq!(
            project_root_of("/home/u/proj/lib/a.dart", "lib/a.dart").as_deref(),
            Some("/home/u/proj")
        );
        assert_eq!(
            project_root_of("C:\\dev\\proj\\lib\\a.dart", "lib/a.dart").as_deref(),
            Some("C:/dev/proj")
        );
        assert_eq!(project_root_of("/elsewhere/b.dart", "lib/a.dart"), None);
    }
}
