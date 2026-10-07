//! Graph relationship extraction and storage during file ingestion.
//!
//! Non-blocking: graph errors are logged but never fail the ingestion pipeline.
//!
//! Tree-sitter is the always-on baseline edge source. When an LSP server is
//! already warm for the file, an additive precision pass resolves `CALLS` edges
//! via call hierarchy: tree-sitter emits a name-only stub callee (empty
//! file_path → an id that never matches the callee's real node), whereas LSP
//! knows the callee's definition site, so we add an edge to its real node_id.
//! The pass is gated on server readiness and is a no-op on a cold index, so it
//! never adds latency to the common path.

use std::path::Path;

use tracing::{debug, info, warn};

use crate::context::ProcessingContext;
use crate::graph::extractor::{
    extract_edges_from_text_chunks, node_type_from_display_name, ExtractionResult,
};
use crate::graph::{compute_member_node_id, compute_node_id, EdgeType, GraphEdge, NodeType};
use crate::lsp::{resolved_call_edges, symbol_column_in_line};
use crate::TextChunk;

/// Extract graph relationships from text chunks and store them atomically as
/// content `generation` (the file's `base_point`).
///
/// Performs:
/// 1. Extract new nodes/edges from chunk metadata (tree-sitter baseline)
/// 2. LSP precision pass for `CALLS` edges when a server is ready (additive)
/// 3. Replace the generation's rows in a single write-lock hold — other
///    versions of the same path (other branches) are untouched
///
/// An empty extraction is stored too: it records that this generation has no
/// symbols, so neither the dedup heal nor the backfill retries it.
///
/// All graph errors are logged and swallowed — graph failures must never
/// block the main ingestion pipeline.
pub(super) async fn ingest_graph_edges(
    ctx: &ProcessingContext,
    tenant_id: &str,
    file_path: &str,
    abs_file_path: &str,
    chunks: &[TextChunk],
    generation: &str,
    resolve_with_lsp: bool,
) {
    let Some(ref graph_store) = ctx.graph_store else {
        return; // Graph not initialized — skip silently
    };

    let mut extraction = extract_edges_from_text_chunks(chunks, tenant_id, file_path);

    // Additive LSP precision pass (best-effort; no-op when no server is ready).
    // Off for bytes no checkout holds: the server answers for the files it can
    // see, which belong to another branch.
    if resolve_with_lsp {
        resolve_calls_via_lsp(
            ctx,
            tenant_id,
            file_path,
            abs_file_path,
            chunks,
            &mut extraction,
        )
        .await;
    }

    let ExtractionResult { nodes, edges } = extraction;

    debug!(
        "Graph: extracting {} nodes, {} edges for {} (generation {})",
        nodes.len(),
        edges.len(),
        file_path,
        generation
    );

    match graph_store
        .reingest_file(tenant_id, file_path, generation, &nodes, &edges)
        .await
    {
        Ok(()) => {
            // Throughput metric: count freshly-written edges by type so the
            // Grafana "Code Graph" dashboard can show ingest rate per edge type.
            let mut by_type: std::collections::HashMap<&str, u64> =
                std::collections::HashMap::new();
            for edge in &edges {
                *by_type.entry(edge.edge_type.as_str()).or_default() += 1;
            }
            for (edge_type, count) in by_type {
                crate::monitoring::metrics_core::METRICS
                    .graph_edges_ingested_total
                    .with_label_values(&[tenant_id, edge_type])
                    .inc_by(count);
            }
        }
        Err(e) => {
            warn!(
                "Graph ingestion failed for {} (tenant {}): {}",
                file_path, tenant_id, e
            );
        }
    }
}

/// LSP precision pass: resolve `CALLS` edges to real callee nodes.
///
/// For each function/method chunk, asks the (already-warm) LSP server for the
/// symbol's outgoing calls and adds an edge to each resolved callee's real
/// node_id. Gated on `is_server_ready_for_file`, so it is a no-op when no
/// server is running for the tenant (cold index / LSP disabled). Callee node
/// type defaults to `Function` (best-effort; free functions are the common
/// case — a mismatched method target is simply an unmatched extra node, no
/// worse than the tree-sitter stub it complements).
async fn resolve_calls_via_lsp(
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

    // Derive the project root by removing the relative suffix from the absolute
    // path; used to relativize LSP-returned callee paths back to graph keys.
    let norm_abs = abs_file_path.replace('\\', "/");
    let norm_rel = file_path.replace('\\', "/");
    let Some(project_root) = norm_abs
        .strip_suffix(&norm_rel)
        .map(|r| r.trim_end_matches('/').to_string())
    else {
        return; // Can't derive root (path layout unexpected) — skip safely.
    };

    // Open the file so the server answers call-hierarchy for it (didOpen; most
    // servers only serve open documents). One open per file, closed after.
    let _ = mgr.open_document(abs_path).await;
    // Wait for the server to finish (re)analyzing the just-opened document
    // instead of a fixed short sleep. Dart's analysis server answers
    // `callHierarchy/outgoingCalls` with an EMPTY result while a freshly opened
    // document is still being analyzed, so the old fixed 300ms sleep resolved 0
    // Dart edges on this incremental ingestion path — the exact gap the backfill
    // pass already closed with this same wait (see `lsp_backfill.rs`). For
    // servers whose only progress is background indexing (typescript-language-
    // server, pyright, rust-analyzer/gopls once indexed) this returns right after
    // the short settle, so the common path keeps its low latency. Shared-behavior
    // alignment: the incremental and backfill LSP passes now wait the same way.
    mgr.wait_for_analysis_idle(abs_path).await;

    for chunk in chunks {
        let meta = &chunk.metadata;
        let Some(chunk_type) = meta.get("chunk_type") else {
            continue;
        };
        // Only callable definitions have outgoing calls.
        let Some(node_type) = node_type_from_display_name(chunk_type) else {
            continue;
        };
        if !matches!(
            chunk_type.as_str(),
            "function" | "async_function" | "method"
        ) {
            continue;
        }
        let Some(symbol) = meta.get("symbol_name").filter(|s| !s.is_empty()) else {
            continue;
        };
        let Some(line) = meta.get("start_line").and_then(|s| s.parse::<u32>().ok()) else {
            continue;
        };

        // Column of the symbol on its definition line (UTF-16, LSP encoding).
        let first_line = chunk.content.lines().next().unwrap_or("");
        let column = symbol_column_in_line(first_line, symbol);

        // start_line is 1-indexed; LSP positions are 0-indexed.
        let calls = mgr
            .resolved_outgoing_calls(abs_path, line.saturating_sub(1), column)
            .await
            .unwrap_or_default();
        if calls.is_empty() {
            continue;
        }

        let parent = meta.get("parent_symbol").map(String::as_str);
        let caller_id = compute_member_node_id(tenant_id, file_path, parent, symbol, node_type);
        // R8.1 — the LSP is AUTHORITATIVE for the calls it resolved: drop this
        // caller's tree-sitter fuzzy stub CALLS edges for those callee names so
        // `resolve_stub_edges` cannot fan them out to every same-named method.
        // Names the LSP did NOT resolve (stdlib / unresolved) keep their fuzzy
        // stub as the fallback — precise-where-available, fuzzy-fallback per call.
        let resolved_names: std::collections::HashSet<&str> =
            calls.iter().map(|c| c.name.as_str()).collect();
        suppress_fuzzy_calls(
            &mut extraction.edges,
            tenant_id,
            &caller_id,
            &resolved_names,
        );
        let (nodes, edges) =
            resolved_call_edges(tenant_id, &caller_id, file_path, &project_root, &calls);
        debug!(
            "Graph LSP pass: {} resolved call edge(s) from {}",
            edges.len(),
            symbol
        );
        extraction.nodes.extend(nodes);
        extraction.edges.extend(edges);
    }
    // Close the document opened above.
    let _ = mgr.close_document(abs_path).await;
}

/// R8.1 — make the LSP-resolved calls authoritative for `caller_id`: remove the
/// tree-sitter fuzzy stub CALLS edges from this caller whose callee NAME the LSP
/// resolved (a precise edge to the real callee replaces them). Fuzzy stubs for
/// names the LSP could not resolve stay as the fallback. A name-only callee stub
/// is keyed `compute_node_id(tenant, "", name, Function)` (see the extractor's
/// `add_calls_edges`), so the same id reconstructs the edge target to drop.
fn suppress_fuzzy_calls(
    edges: &mut Vec<GraphEdge>,
    tenant_id: &str,
    caller_id: &str,
    resolved_names: &std::collections::HashSet<&str>,
) {
    if resolved_names.is_empty() {
        return;
    }
    let stub_ids: std::collections::HashSet<String> = resolved_names
        .iter()
        .map(|n| compute_node_id(tenant_id, "", n, NodeType::Function))
        .collect();
    edges.retain(|e| {
        !(e.edge_type == EdgeType::Calls
            && e.source_node_id == caller_id
            && stub_ids.contains(&e.target_node_id))
    });
}

/// Parse `file_path` and store its graph as content `generation` — the path
/// shared by the dedup heal and the idle backfill. Tree-sitter only (plus the
/// LSP pass when a server is already warm); the embed the dedup path skips is
/// never paid here. A file no grammar covers is recorded as an empty
/// extraction without parsing, so it is not retried.
///
/// Best-effort like all graph work: errors are logged, never failing the
/// pipeline. `file_path`/`abs_file_path` are where the BYTES are read (the
/// branch's own checkout); `relative_path` is the identity the graph keys on.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn rebuild_generation(
    ctx: &ProcessingContext,
    tenant_id: &str,
    collection: &str,
    file_path: &Path,
    relative_path: &str,
    abs_file_path: &str,
    base_path: &str,
    generation: &str,
    resolve_with_lsp: bool,
) {
    let Some(ref graph_store) = ctx.graph_store else {
        return;
    };
    let overrides = super::component::get_gitattributes(ctx, base_path).await;
    let has_language =
        crate::tree_sitter::detect_language_with_overrides(file_path, relative_path, &overrides)
            .is_some();
    if nothing_to_extract(file_path, has_language) {
        if let Err(e) = graph_store
            .reingest_file(tenant_id, relative_path, generation, &[], &[])
            .await
        {
            warn!(
                "graph rebuild: recording {} (generation {}) failed: {}",
                relative_path, generation, e
            );
        }
        return;
    }
    let provider =
        super::grammar::ensure_grammar_available(ctx, file_path, relative_path, &overrides).await;
    let content = match ctx
        .document_processor
        .process_file_content_with_provider(file_path, collection, provider)
        .await
    {
        Ok(c) => c,
        Err(e) => {
            warn!(
                "graph rebuild: parse failed for {} ({}): {}",
                relative_path, abs_file_path, e
            );
            return;
        }
    };
    ingest_graph_edges(
        ctx,
        tenant_id,
        relative_path,
        abs_file_path,
        &content.chunks,
        generation,
        resolve_with_lsp,
    )
    .await;
}

/// Whether a file has no symbols to extract and is recorded as an empty
/// extraction without parsing: no grammar covers it, or it is empty (an empty
/// `__init__.py`). The parser refuses an empty file outright, so without this
/// the backfill retried — and warned about — every one of them on each pass.
fn nothing_to_extract(file_path: &Path, has_language: bool) -> bool {
    !has_language || std::fs::metadata(file_path).is_ok_and(|m| m.len() == 0)
}

/// Build a generation's graph on a branch-dedup hit when it has none.
///
/// The dedup fast path shares an already-indexed content generation with a
/// new branch and returns before the graph phase. The generation's graph is
/// normally there already — and shared with the branch for free, since
/// membership is read from `tracked_files` — but one indexed before graph
/// generations existed, or whose extraction failed, has none, and nothing
/// else would build it while the file stays unchanged. One indexed probe
/// detects that.
pub(super) async fn heal_generation_after_dedup(
    ctx: &ProcessingContext,
    item: &crate::unified_queue_schema::UnifiedQueueItem,
    file_path: &Path,
    relative_path: &str,
    abs_file_path: &str,
    base_path: &str,
    generation: &str,
) {
    let Some(ref graph_store) = ctx.graph_store else {
        return;
    };
    match graph_store
        .generation_extracted(&item.tenant_id, generation)
        .await
    {
        Ok(true) => return, // the common dedup hit
        Ok(false) => {}
        Err(e) => {
            warn!(
                "graph heal: generation probe failed for {} (tenant {}): {} — skipping",
                relative_path, item.tenant_id, e
            );
            return;
        }
    }
    info!(
        "graph heal: building generation {} of {} — dedup hit found none",
        generation, relative_path
    );
    rebuild_generation(
        ctx,
        &item.tenant_id,
        &item.collection,
        file_path,
        relative_path,
        abs_file_path,
        base_path,
        generation,
        !super::item_metadata::reads_git_stage(item),
    )
    .await;
}

/// Delete one content generation's graph (it left the index).
///
/// Non-blocking: errors are logged but don't fail the deletion pipeline.
pub(super) async fn delete_graph_generation(
    ctx: &ProcessingContext,
    tenant_id: &str,
    relative_path: &str,
    generation: &str,
) {
    let Some(ref graph_store) = ctx.graph_store else {
        return;
    };
    if let Err(e) = graph_store.delete_generation(tenant_id, generation).await {
        warn!(
            "Graph generation delete failed for {} (generation {}, tenant {}): {}",
            relative_path, generation, tenant_id, e
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    fn stub_id(t: &str, name: &str) -> String {
        compute_node_id(t, "", name, NodeType::Function)
    }

    #[test]
    fn suppress_fuzzy_calls_drops_only_the_callers_resolved_call_stubs() {
        let t = "t1";
        let caller = compute_node_id(t, "a.rs", "caller", NodeType::Function);
        let other = compute_node_id(t, "a.rs", "other", NodeType::Function);
        let mut edges = vec![
            // The caller's fuzzy CALLS stubs — `add`/`build` are LSP-resolved.
            GraphEdge::new(t, &caller, stub_id(t, "add"), EdgeType::Calls, "a.rs"),
            GraphEdge::new(t, &caller, stub_id(t, "build"), EdgeType::Calls, "a.rs"),
            // `localOnly` was NOT resolved by the LSP → keep it (fuzzy fallback).
            GraphEdge::new(t, &caller, stub_id(t, "localOnly"), EdgeType::Calls, "a.rs"),
            // A CONTAINS edge (different type) must be untouched.
            GraphEdge::new(t, &caller, stub_id(t, "add"), EdgeType::Contains, "a.rs"),
            // Another caller's CALLS to `add` must be untouched.
            GraphEdge::new(t, &other, stub_id(t, "add"), EdgeType::Calls, "a.rs"),
        ];
        let resolved: HashSet<&str> = ["add", "build"].into_iter().collect();
        suppress_fuzzy_calls(&mut edges, t, &caller, &resolved);

        // The caller's resolved fuzzy CALLS stubs are gone.
        assert!(!edges.iter().any(|e| e.source_node_id == caller
            && e.edge_type == EdgeType::Calls
            && (e.target_node_id == stub_id(t, "add") || e.target_node_id == stub_id(t, "build"))));
        // The unresolved call keeps its fuzzy stub (fallback).
        assert!(edges
            .iter()
            .any(|e| e.target_node_id == stub_id(t, "localOnly")));
        // The CONTAINS edge and the OTHER caller's CALLS survive.
        assert!(edges.iter().any(|e| e.edge_type == EdgeType::Contains));
        assert!(edges
            .iter()
            .any(|e| e.source_node_id == other && e.edge_type == EdgeType::Calls));
        assert_eq!(edges.len(), 3);
    }

    #[test]
    fn empty_or_grammarless_files_are_recorded_without_parsing() {
        let dir = tempfile::tempdir().unwrap();
        let empty = dir.path().join("__init__.py");
        std::fs::write(&empty, "").unwrap();
        let code = dir.path().join("lib.py");
        std::fs::write(&code, "def f():\n    pass\n").unwrap();
        assert!(
            nothing_to_extract(&empty, true),
            "the parser refuses empty files"
        );
        assert!(nothing_to_extract(&code, false), "no grammar covers it");
        assert!(!nothing_to_extract(&code, true));
    }

    #[test]
    fn suppress_fuzzy_calls_is_a_noop_when_nothing_resolved() {
        let t = "t1";
        let caller = compute_node_id(t, "a.rs", "caller", NodeType::Function);
        let mut edges = vec![GraphEdge::new(
            t,
            &caller,
            stub_id(t, "add"),
            EdgeType::Calls,
            "a.rs",
        )];
        suppress_fuzzy_calls(&mut edges, t, &caller, &HashSet::new());
        assert_eq!(edges.len(), 1, "empty LSP result must not drop fuzzy edges");
    }
}
