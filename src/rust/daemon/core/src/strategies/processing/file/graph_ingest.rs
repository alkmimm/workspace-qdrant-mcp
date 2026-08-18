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
use crate::graph::{compute_node_id, EdgeType, GraphEdge, NodeType};
use crate::lsp::{resolved_call_edges, symbol_column_in_line};
use crate::TextChunk;

/// Extract graph relationships from text chunks and store them atomically.
///
/// Performs:
/// 1. Delete old edges for this file (cleanup from previous ingestion)
/// 2. Extract new nodes/edges from chunk metadata (tree-sitter baseline)
/// 3. LSP precision pass for `CALLS` edges when a server is ready (additive)
/// 4. Upsert nodes + insert edges in a single write-lock hold
///
/// All graph errors are logged and swallowed — graph failures must never
/// block the main ingestion pipeline.
pub(super) async fn ingest_graph_edges(
    ctx: &ProcessingContext,
    tenant_id: &str,
    file_path: &str,
    abs_file_path: &str,
    chunks: &[TextChunk],
) {
    let Some(ref graph_store) = ctx.graph_store else {
        return; // Graph not initialized — skip silently
    };

    let mut extraction = extract_edges_from_text_chunks(chunks, tenant_id, file_path);

    // Additive LSP precision pass (best-effort; no-op when no server is ready).
    resolve_calls_via_lsp(
        ctx,
        tenant_id,
        file_path,
        abs_file_path,
        chunks,
        &mut extraction,
    )
    .await;

    let ExtractionResult { nodes, edges } = extraction;

    if nodes.is_empty() && edges.is_empty() {
        return;
    }

    debug!(
        "Graph: extracting {} nodes, {} edges for {}",
        nodes.len(),
        edges.len(),
        file_path
    );

    match graph_store
        .reingest_file(tenant_id, file_path, &nodes, &edges)
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

        let caller_id = compute_node_id(tenant_id, file_path, symbol, node_type);
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

/// Rebuild a file's graph edges after a branch-dedup hit left them wiped
/// (issue #235).
///
/// When an update changes a file's hash, the preamble GCs the old content-row,
/// and that delete wipes the file's edges by path. If the NEW content then
/// dedups against an existing content-row (revert, branch switch, merge
/// restoring a known hash), `try_branch_dedup` returns before the graph phase,
/// so nothing rewrites them. One indexed probe detects the gap; the rebuild
/// re-parses with tree-sitter only — the embed skip that makes dedup cheap is
/// preserved. Files wiped before this fix heal on their next dedup touch.
///
/// Best-effort like all graph work: errors are logged, never failing the
/// pipeline.
pub(super) async fn heal_edges_after_dedup(
    ctx: &ProcessingContext,
    item: &crate::unified_queue_schema::UnifiedQueueItem,
    file_path: &Path,
    relative_path: &str,
    abs_file_path: &str,
    base_path: &str,
) {
    let Some(ref graph_store) = ctx.graph_store else {
        return;
    };

    // Only code files produce graph data — text chunking carries no symbols.
    let overrides = super::component::get_gitattributes(ctx, base_path).await;
    if crate::tree_sitter::detect_language_with_overrides(file_path, relative_path, &overrides)
        .is_none()
    {
        return;
    }

    match graph_store
        .file_has_edges(&item.tenant_id, relative_path)
        .await
    {
        Ok(true) => return, // edges intact — the common dedup hit
        Ok(false) => {}
        Err(e) => {
            warn!(
                "graph heal: edge probe failed for {} (tenant {}): {} — skipping",
                relative_path, item.tenant_id, e
            );
            return;
        }
    }

    let provider =
        super::grammar::ensure_grammar_available(ctx, file_path, relative_path, &overrides).await;
    let content = match ctx
        .document_processor
        .process_file_content_with_provider(file_path, &item.collection, provider)
        .await
    {
        Ok(c) => c,
        Err(e) => {
            warn!(
                "graph heal: re-parse failed for {} ({}): {}",
                relative_path, abs_file_path, e
            );
            return;
        }
    };
    info!(
        "graph heal: rebuilding edges for {} — dedup hit found none (#235)",
        relative_path
    );
    ingest_graph_edges(
        ctx,
        &item.tenant_id,
        relative_path,
        abs_file_path,
        &content.chunks,
    )
    .await;
}

/// Delete graph edges for a file (called during file deletion).
///
/// Non-blocking: errors are logged but don't fail the deletion pipeline.
pub(super) async fn delete_graph_edges(ctx: &ProcessingContext, tenant_id: &str, file_path: &str) {
    let Some(ref graph_store) = ctx.graph_store else {
        return;
    };

    // Use reingest_file with empty nodes/edges to just delete old edges
    if let Err(e) = graph_store
        .reingest_file(tenant_id, file_path, &[], &[])
        .await
    {
        warn!(
            "Graph edge deletion failed for {} (tenant {}): {}",
            file_path, tenant_id, e
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
