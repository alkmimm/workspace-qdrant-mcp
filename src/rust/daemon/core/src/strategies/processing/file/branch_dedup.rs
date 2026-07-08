//! Cross-branch ingestion fast-path (Layer 2 — share one point across branches).
//!
//! With a branch-agnostic `base_point` (`SHA256(tenant|relative_path|file_hash)`),
//! identical content on different branches maps to ONE physical Qdrant point.
//! When a `file/add` or `file/update` item is processed whose content another
//! branch already indexed (same `(watch_folder_id, relative_path, file_hash)`),
//! the expensive parse + embed is skipped AND no vectors are copied — the
//! file's existing chunk set is REUSED in place:
//!
//! 1. Add the current branch to the shared points' `branch` array payload
//!    (`set_payload`), so branch-scoped search returns them on this branch.
//! 2. Insert a `tracked_files` row for this branch pointing at the same
//!    `base_point` (+ chunk_count, source `dedup_share`).
//! 3. Copy the `qdrant_chunks` mirror rows from the source row (same point_ids,
//!    since the base_point — hence every point_id — is shared).
//! 4. Enqueue FTS5 work so search.db gets `code_lines` + `file_metadata` for the
//!    current branch (FTS5 stays per-branch; search filters by `fm.branch = ?`).
//! 5. Flip qdrant_status=done, search_status=in_progress.
//!
//! This makes `git checkout` between branches near-free on the indexed-data
//! side and — unlike Layer 1 — adds NO Qdrant storage per branch.
//!
//! See [docs/specs/21-cross-branch-dedup.md](../../../../../../../docs/specs/21-cross-branch-dedup.md).

use std::path::Path;

use tracing::{debug, info, warn};

use crate::context::ProcessingContext;
use crate::fts_batch_processor::FileChange;
use crate::search_db::Fts5WorkItem;
use crate::tracked_files_schema::{self, ProcessingStatus};
use crate::unified_queue_processor::UnifiedProcessorError;
use crate::unified_queue_schema::{DestinationStatus, FilePayload, QueueOperation, UnifiedQueueItem};
use wqm_common::hashing::{compute_base_point, compute_content_hash, normalize_line_endings};

/// Outcome of [`try_branch_dedup`] — `Some` means the dedup fast-path completed
/// and the caller must return early; `None` means the file is novel (or the
/// shared points are missing) and the normal ingest pipeline should run.
pub(super) struct DedupHit;

#[allow(clippy::too_many_arguments)]
pub(super) async fn try_branch_dedup(
    ctx: &ProcessingContext,
    item: &UnifiedQueueItem,
    payload: &FilePayload,
    file_path: &Path,
    abs_file_path: &str,
    base_path: &str,
    relative_path: &str,
    watch_folder_id: &str,
) -> Result<Option<DedupHit>, UnifiedProcessorError> {
    // Uplift demands a fresh extraction pass — reusing another branch's chunk
    // set is exactly what the capability/extractor upgrade is replacing.
    if item.op == QueueOperation::Uplift {
        return Ok(None);
    }

    let file_hash = tracked_files_schema::compute_file_hash(file_path)
        .map_err(|e| UnifiedProcessorError::ProcessingFailed(e.to_string()))?;

    // ── 1. Is this content already indexed (any branch)? ──
    // Layer 2 stage 2: one content-row per (watch, relative_path, file_hash). If
    // it exists, the shared Qdrant points exist too — we add this branch to them
    // instead of re-embedding. (prepare_update already skipped the case where
    // THIS branch holds the content with a current chunker fingerprint.)
    type DedupRow = (
        i32,
        Option<String>,
        Option<String>,
        Option<String>,
        Option<String>,
    );
    let existing: Option<DedupRow> = sqlx::query_as(
        "SELECT chunk_count, file_type, language, chunker_version, treesitter_status
             FROM tracked_files
             WHERE watch_folder_id = ?1
               AND relative_path = ?2
               AND file_hash = ?3
               AND base_point IS NOT NULL
             ORDER BY updated_at DESC
             LIMIT 1",
    )
    .bind(watch_folder_id)
    .bind(relative_path)
    .bind(&file_hash)
    .fetch_optional(&ctx.pool)
    .await
    .map_err(|e| UnifiedProcessorError::ProcessingFailed(format!("dedup lookup: {e}")))?;

    let Some((chunk_count, file_type, language, src_chunker_version, src_treesitter_status)) =
        existing
    else {
        return Ok(None);
    };

    // Carry the source row's tree-sitter status verbatim. The dedup clone shares
    // the source's `base_point` — hence the exact same Qdrant points, which were
    // semantically chunked (or not) once, at the source's real ingest. Forcing
    // `None` here would REBASE an already-`done` file back to `none`: the metric
    // (`tracked_files_by_chunking`) would under-report semantic coverage, and the
    // capability-upgrade query (`treesitter_status IN ('none','failed','skipped')`)
    // would re-enqueue the file forever with nothing to re-chunk. See
    // docs/specs/21-cross-branch-dedup.md.
    let carried_treesitter_status = src_treesitter_status
        .as_deref()
        .and_then(ProcessingStatus::from_str)
        .unwrap_or(ProcessingStatus::None);

    // Stale-generation guard: only reuse chunks produced by the CURRENT chunking
    // configuration. Reusing a pre-upgrade generation would carry stale chunks
    // (and the stale fingerprint) into this branch, and the fingerprint gate
    // would re-trigger on every later visit without converging. NULL (legacy
    // source) is grandfathered, same as the gate.
    let overrides = super::component::get_gitattributes(ctx, base_path).await;
    let detected =
        crate::tree_sitter::detect_language_with_overrides(file_path, relative_path, &overrides);
    let current_fp = crate::tree_sitter::chunker::chunking_fingerprint(detected);
    if !crate::tree_sitter::chunker::stored_fingerprint_is_current(
        src_chunker_version.as_deref(),
        &current_fp,
    ) {
        info!(
            "branch_dedup: source row for {} was chunked with stale configuration {:?} (current {}) — falling back to full ingest",
            relative_path, src_chunker_version, current_fp
        );
        return Ok(None);
    }

    // base_point is branch-agnostic: the source row's base_point IS ours.
    let base_point = compute_base_point(&item.tenant_id, relative_path, &file_hash);

    // ── 2. Add this branch to the shared points' `branch` array ──
    let point_count = ctx
        .storage_client
        .add_branch_to_base_point(&item.collection, &base_point, &item.branch)
        .await
        .map_err(|e| UnifiedProcessorError::Storage(e.to_string()))?;
    if point_count == 0 {
        // tracked_files claims chunks but Qdrant has none — stale row / partial
        // cleanup. Fall back to a full ingest so the file is embedded fresh.
        warn!(
            "branch_dedup: content row for {} has base_point {} but Qdrant returned 0 points — falling back to normal ingest",
            relative_path, base_point
        );
        return Ok(None);
    }

    // ── 3. tracked_files row + qdrant_chunks mirror for this branch ──
    let file_mtime = std::fs::metadata(file_path)
        .ok()
        .and_then(|m| m.modified().ok())
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs().to_string())
        .unwrap_or_default();
    let extension = file_path
        .extension()
        .and_then(|e| e.to_str())
        .map(|s| s.to_lowercase());
    let is_test = crate::file_classification::is_test_file(file_path);

    // BEGIN IMMEDIATE + busy retry (see db_retry) to avoid SQLITE_BUSY_SNAPSHOT.
    let mut tx = crate::db_retry::begin_immediate(&ctx.pool)
        .await
        .map_err(|e| UnifiedProcessorError::ProcessingFailed(format!("dedup tx begin: {e}")))?;
    let file_id = tracked_files_schema::insert_tracked_file_tx(
        &mut tx,
        watch_folder_id,
        relative_path,
        Some(item.branch.as_str()),
        file_type.as_deref(),
        language.as_deref(),
        &file_mtime,
        &file_hash,
        chunk_count,
        Some("dedup_share"),
        // The clone IS the source generation — carry its fingerprint verbatim
        // (the guard above proved it is current or grandfathered).
        src_chunker_version.as_deref(),
        // lsp_status: chunk-level LSP enrichment is retired; uniformly not-done.
        ProcessingStatus::None,
        // treesitter_status: carry the source's status (see note above) instead
        // of clobbering an already-`done` file back to `none`.
        carried_treesitter_status,
        Some(item.collection.as_str()),
        extension.as_deref(),
        is_test,
        Some(&base_point),
        None,
    )
    .await
    .map_err(|e| UnifiedProcessorError::ProcessingFailed(format!("insert_tracked_file: {e}")))?;

    // No qdrant_chunks copy: insert_tracked_file_tx merged this branch into the
    // EXISTING content-row, whose mirror already references the shared points.
    tx.commit()
        .await
        .map_err(|e| UnifiedProcessorError::ProcessingFailed(format!("dedup tx commit: {e}")))?;

    // ── 4. Enqueue FTS5 work (batch writer owns search.db writes) ──
    if let Some(sender) = crate::search_db::batch_writer::global_sender() {
        match tokio::fs::read_to_string(file_path).await {
            Ok(raw_content) => {
                // Normalize EOL so this branch-dedup FTS5 enqueue matches the
                // base_point identity and never re-stores a stale '\r'.
                let new_content = normalize_line_endings(&raw_content).into_owned();
                let new_hash = compute_content_hash(&new_content);
                let change = FileChange {
                    file_id,
                    size_bytes: Some(new_content.len() as i64),
                    old_content: String::new(),
                    new_content: new_content.clone(),
                    tenant_id: item.tenant_id.clone(),
                    branch: Some(item.branch.clone()),
                    file_path: abs_file_path.to_string(),
                    base_point: Some(base_point.clone()),
                    relative_path: Some(relative_path.to_string()),
                    file_hash: Some(file_hash.clone()),
                };
                let work = Fts5WorkItem {
                    change,
                    new_content_bytes: new_content.into_bytes(),
                    new_hash,
                    queue_id: item.queue_id.clone(),
                };
                let _ = ctx
                    .queue_manager
                    .update_destination_status(
                        &item.queue_id,
                        "search",
                        DestinationStatus::InProgress,
                    )
                    .await;
                if let Err(e) = sender.send(work).await {
                    warn!(
                        "branch_dedup: failed to enqueue FTS5 work for {}: {} — marking search=failed",
                        relative_path, e
                    );
                    let _ = ctx
                        .queue_manager
                        .update_destination_status(
                            &item.queue_id,
                            "search",
                            DestinationStatus::Failed,
                        )
                        .await;
                }
            }
            Err(e) => {
                // Binary or unreadable — skip search but qdrant work still
                // counts as done.
                debug!(
                    "branch_dedup: skipping FTS5 for {} ({}): {}",
                    relative_path, abs_file_path, e
                );
                let _ = ctx
                    .queue_manager
                    .update_destination_status(&item.queue_id, "search", DestinationStatus::Done)
                    .await;
            }
        }
    } else {
        // Library/test mode with no batch writer — mark search=done so the
        // orchestration-only path completes.
        let _ = ctx
            .queue_manager
            .update_destination_status(&item.queue_id, "search", DestinationStatus::Done)
            .await;
    }

    // ── 5. Destination markers + return ──
    let _ = ctx
        .queue_manager
        .update_destination_status(&item.queue_id, "qdrant", DestinationStatus::Done)
        .await;

    info!(
        "branch_dedup hit: {} (+= {}) skipped embed, shared {} points at base_point {}",
        relative_path, item.branch, point_count, base_point
    );

    // Suppress unused warnings on payload — kept in the signature to mirror the
    // normal ingest entry-point and ease future field reuse.
    let _ = payload;
    Ok(Some(DedupHit))
}

// (copy_qdrant_chunks removed in Layer 2 stage 2: the content-row is shared, so
// `insert_tracked_file_tx` merges the branch into the existing row whose mirror
// already references the shared points — there is nothing to copy.)
