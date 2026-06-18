//! File delete processing.
//!
//! Handles `QueueOperation::Delete` for file items: reference-counted Qdrant
//! point deletion, tracked_files cleanup, FTS5 cleanup, and missing-file
//! reconciliation.

use std::time::Instant;

use sqlx::SqlitePool;
use tracing::{debug, info, warn};

use crate::context::ProcessingContext;
use crate::fts_batch_processor::{FtsBatchConfig, FtsBatchProcessor};
use crate::processing_timings::{self, PhaseTiming};
use crate::tracked_files_schema;
use crate::tree_sitter::detect_language;
use crate::unified_queue_processor::{UnifiedProcessorError, UnifiedProcessorResult};
use crate::unified_queue_schema::UnifiedQueueItem;

/// Process file delete operation with tracked_files awareness (Task 506 + Task 519).
///
/// **F-035 contract:** Qdrant delete failures block SQLite cleanup and return
/// `Err`. The tracked_files row stays intact and the queue row enters retry
/// via `mark_unified_failed`. Without this, stale Qdrant vectors stayed
/// retrievable after the local row was wiped, with no record that cleanup
/// had failed.
pub(super) async fn process_file_delete(
    ctx: &ProcessingContext,
    item: &UnifiedQueueItem,
    pool: &SqlitePool,
    watch_folder_id: &str,
    relative_path: &str,
    abs_file_path: &str,
) -> UnifiedProcessorResult<()> {
    let delete_start = Instant::now();
    let mut timings: Vec<PhaseTiming> = Vec::new();
    let detected_language = detect_language(std::path::Path::new(abs_file_path));

    if let Ok(true) = tracked_files_schema::is_incremental(pool, abs_file_path).await {
        info!(
            "Skipping delete for incremental file (safety net): {}",
            abs_file_path
        );
        return Ok(());
    }

    if let Ok(Some(existing)) = tracked_files_schema::lookup_tracked_file(
        pool,
        watch_folder_id,
        relative_path,
        Some(item.branch.as_str()),
    )
    .await
    {
        timings.push(PhaseTiming {
            phase: "lookup",
            duration_ms: delete_start.elapsed().as_millis() as u64,
        });

        let delete_result = delete_tracked_file(
            ctx,
            item,
            pool,
            watch_folder_id,
            relative_path,
            abs_file_path,
            &existing,
            &mut timings,
            delete_start,
        )
        .await;

        record_delete_timings(ctx, item, pool, detected_language, &timings).await;
        return delete_result;
    }

    // Fallback: file not in tracked_files — attempt Qdrant filter delete.
    // F-035: a fallback Qdrant delete failure is still a real failure — the
    // points may exist but cannot be deleted. Surface it so retry metadata is
    // populated.
    let fallback_result =
        fallback_qdrant_delete(ctx, item, abs_file_path, delete_start, &mut timings).await;
    record_delete_timings(ctx, item, pool, detected_language, &timings).await;
    fallback_result
}

/// Delete a file that is present in tracked_files: Qdrant (ref-counted), SQLite,
/// FTS5, graph edges, and keyword extractions.
///
/// **F-035:** if the Qdrant delete fails, `tracked_files` row is preserved and
/// `Err` is returned so the queue row gets retry metadata. Without this guard,
/// stale vectors stayed in Qdrant while local tracking was already wiped.
#[allow(clippy::too_many_arguments)]
pub(super) async fn delete_tracked_file(
    ctx: &ProcessingContext,
    item: &UnifiedQueueItem,
    pool: &SqlitePool,
    watch_folder_id: &str,
    relative_path: &str,
    abs_file_path: &str,
    existing: &tracked_files_schema::TrackedFile,
    timings: &mut Vec<PhaseTiming>,
    delete_start: Instant,
) -> UnifiedProcessorResult<()> {
    let _ = watch_folder_id; // ref-counting is keyed by file_id / base_point now
    let bp = existing.base_point.as_deref();

    // Post-delete predicates (computed before mutating; the queries exclude THIS
    // row). Layer 2 stage 2: one row per content with a `branches` set; the
    // physical point is shared across branches AND clones.
    //   - r_new_empty: dropping item.branch empties THIS row's set (row vanishes).
    //   - other_refs_bp: another row (clone) still references base_point.
    //   - other_holds_x: another row still holds item.branch for base_point.
    let r_new_empty = existing
        .branches
        .iter()
        .all(|b| b.as_str() == item.branch.as_str());
    let (other_refs_bp, other_holds_x) = match bp {
        Some(bp) => (
            ctx.queue_manager
                .has_other_references(bp, existing.file_id)
                .await
                .unwrap_or(false),
            ctx.queue_manager
                .branch_held_by_other(bp, existing.file_id, &item.branch)
                .await
                .unwrap_or(false),
        ),
        None => (false, false),
    };
    // Delete the physical points only when this row vanishes AND no other row
    // references the base_point.
    let delete_points = r_new_empty && !other_refs_bp;

    let t0 = Instant::now();
    if let Some(bp) = bp {
        if delete_points {
            if let Err(e) =
                delete_qdrant_points(ctx, item, pool, relative_path, abs_file_path, existing).await
            {
                timings.push(PhaseTiming {
                    phase: "qdrant_delete",
                    duration_ms: t0.elapsed().as_millis() as u64,
                });
                warn!(
                    "Qdrant delete failed for {} — leaving tracked_files row intact and queuing retry: {}",
                    relative_path, e
                );
                return Err(e);
            }
        } else if !other_holds_x {
            // Content stays (another row/clone references it), but this branch is
            // no longer held by any row — drop it from the shared point's array.
            if let Err(e) = ctx
                .storage_client
                .remove_branch_from_base_point(&item.collection, bp, &item.branch)
                .await
            {
                warn!(
                    "Failed to drop branch {} from base_point {} on delete of {}: {}",
                    item.branch, bp, relative_path, e
                );
            }
        }
    }
    timings.push(PhaseTiming {
        phase: "qdrant_delete",
        duration_ms: t0.elapsed().as_millis() as u64,
    });

    // SQLite: remove the branch from the content-row's set (deletes the row when
    // its set empties; CASCADE drops qdrant_chunks).
    let t0 = Instant::now();
    let remaining =
        tracked_files_schema::remove_branch_from_tracked_file(pool, existing.file_id, &item.branch)
            .await;
    timings.push(PhaseTiming {
        phase: "sqlite_cleanup",
        duration_ms: t0.elapsed().as_millis() as u64,
    });
    let row_deleted = matches!(remaining, Ok(0));
    if let Err(e) = &remaining {
        warn!(
            "SQLite branch-remove failed for {}: {}. Marked for reconciliation.",
            relative_path, e
        );
        let _ = tracked_files_schema::mark_needs_reconcile(
            pool,
            existing.file_id,
            &format!("branch_remove_failed: {}", e),
        )
        .await;
        return Ok(());
    }

    if row_deleted {
        // Content fully gone for this watch folder — purge FTS5, graph, keywords.
        let t0 = Instant::now();
        cleanup_fts5(ctx, existing).await;
        timings.push(PhaseTiming {
            phase: "fts5_cleanup",
            duration_ms: t0.elapsed().as_millis() as u64,
        });
        super::graph_ingest::delete_graph_edges(ctx, &item.tenant_id, relative_path).await;
        let doc_id = crate::generate_document_id(&item.tenant_id, abs_file_path);
        super::keyword_persist::delete_extraction(pool, &doc_id).await;
    } else {
        // Branch removed but content remains — drop the branch from FTS5 metadata
        // so `grep` on this branch no longer matches, keeping code_lines for the
        // other branches.
        remove_branch_from_fts5(ctx, existing.file_id, &item.branch).await;
    }

    info!(
        "Removed branch {} for: {} in {}ms (row_deleted={}, delete_points={})",
        item.branch,
        relative_path,
        delete_start.elapsed().as_millis(),
        row_deleted,
        delete_points
    );
    Ok(())
}

/// Delete Qdrant points for a tracked file.
///
/// **F-035:** propagate Qdrant errors so the caller can short-circuit SQLite
/// cleanup. Returning `Ok(())` on failure silently dropped the local row
/// while leaving the vectors retrievable in Qdrant.
async fn delete_qdrant_points(
    ctx: &ProcessingContext,
    item: &UnifiedQueueItem,
    pool: &SqlitePool,
    relative_path: &str,
    abs_file_path: &str,
    existing: &tracked_files_schema::TrackedFile,
) -> UnifiedProcessorResult<()> {
    let point_ids = tracked_files_schema::get_chunk_point_ids(pool, existing.file_id)
        .await
        .unwrap_or_default();

    if point_ids.is_empty() {
        return Ok(());
    }

    ctx.storage_client
        .delete_points_by_filter(&item.collection, abs_file_path, &item.tenant_id)
        .await
        .map_err(|e| {
            UnifiedProcessorError::Storage(format!(
                "Qdrant delete failed for {} ({} tracked points): {}",
                relative_path,
                point_ids.len(),
                e
            ))
        })?;
    Ok(())
}

/// Clean up FTS5 code_lines for a fully-removed file (non-fatal).
async fn cleanup_fts5(ctx: &ProcessingContext, existing: &tracked_files_schema::TrackedFile) {
    if let Some(sdb) = &ctx.search_db {
        let processor = FtsBatchProcessor::new(sdb, FtsBatchConfig::default());
        if let Err(e) = processor.delete_file(existing.file_id).await {
            warn!(
                "FTS5: failed to delete code_lines for file_id={}: {} (non-fatal)",
                existing.file_id, e
            );
        } else {
            debug!("FTS5: deleted code_lines for file_id={}", existing.file_id);
        }
    }
}

/// Drop a single branch from a file's FTS5 `file_metadata` row WITHOUT deleting
/// its `code_lines` (Layer 2 stage 2 — other branches still hold the content).
async fn remove_branch_from_fts5(ctx: &ProcessingContext, file_id: i64, branch: &str) {
    if let Some(sdb) = &ctx.search_db {
        if let Err(e) = sdb.remove_branch_from_file_metadata(file_id, branch).await {
            warn!(
                "FTS5: failed to drop branch {} from file_id={}: {} (non-fatal)",
                branch, file_id, e
            );
        }
    }
}

/// Fallback delete when the file is not in tracked_files: attempt Qdrant filter delete.
///
/// **F-035:** Qdrant errors are real failures (network/auth/server fault),
/// not "points might not exist" — the Qdrant client returns Ok with zero
/// affected points if nothing matches the filter. Propagate Err so the
/// caller queues a retry instead of silently swallowing the failure.
async fn fallback_qdrant_delete(
    ctx: &ProcessingContext,
    item: &UnifiedQueueItem,
    abs_file_path: &str,
    delete_start: Instant,
    timings: &mut Vec<PhaseTiming>,
) -> UnifiedProcessorResult<()> {
    debug!(
        "File not in tracked_files, attempting Qdrant filter delete: {}",
        abs_file_path
    );
    let t0 = Instant::now();
    let result = ctx
        .storage_client
        .delete_points_by_filter(&item.collection, abs_file_path, &item.tenant_id)
        .await;
    timings.push(PhaseTiming {
        phase: "qdrant_delete",
        duration_ms: t0.elapsed().as_millis() as u64,
    });
    match result {
        Ok(_) => {
            info!(
                "Deleted points for file (fallback) in {}ms: {}",
                delete_start.elapsed().as_millis(),
                abs_file_path
            );
            Ok(())
        }
        Err(e) => Err(UnifiedProcessorError::Storage(format!(
            "Qdrant fallback delete failed for {}: {}",
            abs_file_path, e
        ))),
    }
}

/// Record timing data for delete operations.
async fn record_delete_timings(
    ctx: &ProcessingContext,
    item: &UnifiedQueueItem,
    pool: &SqlitePool,
    language: Option<&str>,
    timings: &[PhaseTiming],
) {
    let _ = ctx; // ProcessingContext not needed for recording, but kept for consistency
    processing_timings::record_timings(
        pool,
        &item.queue_id,
        item.item_type.as_str(),
        item.op.as_str(),
        &item.tenant_id,
        &item.collection,
        language,
        timings,
    )
    .await;
}

/// Clean up tracked records and Qdrant points for a file that no longer exists on disk.
///
/// **F-035:** if Qdrant deletion fails, returns `Err` and leaves the
/// tracked_files row intact. Without this, missing-file cleanup wiped the
/// local row while Qdrant still held vectors that could surface in search.
pub(super) async fn cleanup_missing_file(
    ctx: &ProcessingContext,
    item: &UnifiedQueueItem,
    pool: &SqlitePool,
    watch_folder_id: &str,
    relative_path: &str,
    abs_file_path: &str,
) -> UnifiedProcessorResult<()> {
    if let Ok(Some(existing)) = tracked_files_schema::lookup_tracked_file(
        pool,
        watch_folder_id,
        relative_path,
        Some(item.branch.as_str()),
    )
    .await
    {
        debug!(
            "File no longer exists, removing branch {} from tracked record: {}",
            item.branch, relative_path
        );
        // Reuse the branch-set delete path (Layer 2 stage 2): drop this branch
        // from the content-row's set, GC the row + shared points only when no
        // branch/clone holds the content anymore. Preserves F-035 (a Qdrant
        // failure returns Err, leaving the row for retry).
        let mut timings: Vec<PhaseTiming> = Vec::new();
        delete_tracked_file(
            ctx,
            item,
            pool,
            watch_folder_id,
            relative_path,
            abs_file_path,
            &existing,
            &mut timings,
            Instant::now(),
        )
        .await?;
    }
    Ok(())
}

/// Handle Qdrant insert failure by cleaning up stale SQLite state.
pub(super) async fn handle_qdrant_failure(
    _ctx: &ProcessingContext,
    item: &UnifiedQueueItem,
    pool: &SqlitePool,
    watch_folder_id: &str,
    relative_path: &str,
    qdrant_err: &str,
) {
    // Old Qdrant points were deleted but new ones failed to insert.
    // Clean up stale qdrant_chunks so SQLite doesn't reference non-existent points.
    if let Ok(Some(existing)) = tracked_files_schema::lookup_tracked_file(
        pool,
        watch_folder_id,
        relative_path,
        Some(item.branch.as_str()),
    )
    .await
    {
        let cleanup_result: Result<(), String> = async {
            let mut tx = pool.begin().await.map_err(|e| format!("begin tx: {}", e))?;
            tracked_files_schema::delete_qdrant_chunks_tx(&mut tx, existing.file_id)
                .await
                .map_err(|e| format!("delete chunks: {}", e))?;
            tx.commit().await.map_err(|e| format!("commit: {}", e))?;
            Ok(())
        }
        .await;

        match cleanup_result {
            Ok(()) => {
                warn!(
                    "Qdrant insert failed for {}; cleaned up stale SQLite chunks. Error: {}",
                    relative_path, qdrant_err
                );
            }
            Err(cleanup_err) => {
                warn!(
                    "Qdrant insert failed AND chunk cleanup failed for {}: insert={}, cleanup={}",
                    relative_path, qdrant_err, cleanup_err
                );
                let _ = tracked_files_schema::mark_needs_reconcile(
                    pool,
                    existing.file_id,
                    &format!("qdrant_insert_failed_cleanup_failed: {}", cleanup_err),
                )
                .await;
            }
        }
    }
}
