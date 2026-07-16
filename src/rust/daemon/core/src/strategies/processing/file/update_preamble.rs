//! Update preamble: hash comparison and reference-counted old point deletion.
//!
//! Called when `QueueOperation::Update` is in effect. Compares the new file
//! hash against the tracked record, stores a `QueueDecision` for retry-safe
//! execution, and deletes old Qdrant points only if no other watch folder
//! references the same base_point.

use std::time::Instant;

use sqlx::SqlitePool;
use tracing::warn;

use crate::context::ProcessingContext;
use crate::processing_timings::{self, PhaseTiming};
use crate::tracked_files_schema;
use crate::tree_sitter::detect_language;
use crate::unified_queue_processor::UnifiedProcessorResult;
use crate::unified_queue_schema::{FilePayload, UnifiedQueueItem};

/// Execute the deletion part of an update operation (reference-counted).
///
/// Called after hash comparison determines the file has changed.
/// Stores a `QueueDecision` for retry-safe execution and deletes old points
/// only if no other watch folder references the same base_point.
#[allow(clippy::too_many_arguments)]
pub(super) async fn execute_update_deletion(
    ctx: &ProcessingContext,
    item: &UnifiedQueueItem,
    pool: &SqlitePool,
    watch_folder_id: &str,
    relative_path: &str,
    abs_file_path: &str,
    payload: &FilePayload,
    existing: &tracked_files_schema::TrackedFile,
    new_hash: &str,
) -> UnifiedProcessorResult<()> {
    let preamble_start = Instant::now();

    let new_base_point =
        wqm_common::hashing::compute_base_point(&item.tenant_id, relative_path, new_hash);

    // Content changed (hash differs) iff the new base_point differs from the old
    // one. A pure re-chunk (same hash) keeps the same base_point — nothing to
    // delete; the ingest re-upserts the same points.
    let delete_old = existing.base_point.as_deref() != Some(new_base_point.as_str());

    let decision = wqm_common::queue_types::QueueDecision {
        delete_old,
        old_base_point: existing.base_point.clone(),
        new_base_point: new_base_point.clone(),
        old_file_hash: Some(existing.file_hash.clone()),
        new_file_hash: new_hash.to_string(),
    };
    if let Err(e) = ctx
        .queue_manager
        .store_queue_decision(&item.queue_id, &decision)
        .await
    {
        warn!("Failed to store QueueDecision for {}: {}", item.queue_id, e);
    }

    // Layer 2 stage 2: the content changed on item.branch, so drop item.branch
    // from the OLD content-row and GC its row/points only when no branch/clone
    // still holds the old content (the unified branch-set delete). The ingest
    // pipeline then adds item.branch to the NEW content-row.
    if delete_old {
        let mut timings: Vec<PhaseTiming> = Vec::new();
        super::delete::delete_tracked_file(
            ctx,
            item,
            pool,
            watch_folder_id,
            relative_path,
            abs_file_path,
            existing,
            &mut timings,
            Instant::now(),
        )
        .await?;

        // #224: `existing` is only the NEWEST holder of item.branch — any
        // shadowed generation still carrying the tag would survive this move
        // and duplicate once the ingest below re-adds the branch to the new
        // content-row. Sweep them (policy-gated) so a tag MOVES instead of
        // copying.
        super::delete::strip_shadowed_holders(
            ctx,
            item,
            pool,
            watch_folder_id,
            relative_path,
            abs_file_path,
            existing.file_id,
            &mut timings,
        )
        .await;
    }

    record_preamble_timing(pool, item, payload, abs_file_path, preamble_start).await;
    Ok(())
}

async fn record_preamble_timing(
    pool: &SqlitePool,
    item: &UnifiedQueueItem,
    _payload: &FilePayload,
    abs_file_path: &str,
    preamble_start: Instant,
) {
    let detected_language = detect_language(std::path::Path::new(abs_file_path));
    processing_timings::record_timings(
        pool,
        &item.queue_id,
        item.item_type.as_str(),
        "update_preamble",
        &item.tenant_id,
        &item.collection,
        detected_language,
        &[PhaseTiming {
            phase: "update_preamble",
            duration_ms: preamble_start.elapsed().as_millis() as u64,
        }],
    )
    .await;
}
