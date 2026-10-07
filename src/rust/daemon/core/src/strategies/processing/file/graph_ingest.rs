//! Graph relationship extraction and storage during file ingestion.
//!
//! Non-blocking: graph errors are logged but never fail the ingestion pipeline.
//!
//! Tree-sitter is the always-on baseline edge source. When an LSP server is
//! already warm for the file, an additive precision pass locates each call's
//! definition site via call hierarchy (`super::graph_lsp_calls`). The pass is
//! gated on server readiness and is a no-op on a cold index, so it never adds
//! latency to the common path.

use std::path::Path;

use tracing::{debug, info, warn};

use super::graph_lsp_calls::resolve_calls_via_lsp;
use crate::context::ProcessingContext;
use crate::graph::extractor::{extract_edges_from_text_chunks, ExtractionResult};
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
}
