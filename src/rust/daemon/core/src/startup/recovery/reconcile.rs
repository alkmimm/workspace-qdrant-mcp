//! Reconciliation of tracked_files flagged with needs_reconcile=1.

use std::collections::HashMap;
use std::path::Path;

use sqlx::SqlitePool;
use tracing::{debug, info, warn};

use crate::queue_operations::QueueManager;
use crate::tracked_files_schema;
use crate::unified_queue_schema::QueueOperation;

use super::queue::enqueue_file_op;
use super::types::FullRecoveryStats;

/// Each watch folder's HEAD branch (`None`: detached / non-git), read once
/// per pass rather than once per flagged row.
type HeadCache = HashMap<String, Option<String>>;

/// Process tracked_files flagged with needs_reconcile=1.
///
/// For each flagged file, look up its watch_folder to get routing info,
/// then re-queue it for ingestion.
///
/// The `needs_reconcile` flag is NOT cleared here (F-020). It is deferred
/// until the enqueued queue item completes successfully (i.e. is deleted from
/// the unified_queue by `delete_unified_item`). This prevents silent loss of
/// repair intent if the daemon crashes between enqueue and processing.
pub(super) async fn reconcile_flagged_files(
    pool: &SqlitePool,
    queue_manager: &QueueManager,
    stats: &mut FullRecoveryStats,
) {
    let flagged = match tracked_files_schema::get_files_needing_reconcile(pool).await {
        Ok(files) => files,
        Err(e) => {
            warn!("Failed to query needs_reconcile files: {}", e);
            stats.reconcile_errors += 1;
            return;
        }
    };

    if flagged.is_empty() {
        debug!("No files need reconciliation");
        return;
    }

    info!("Reconciling {} flagged files", flagged.len());

    let mut heads = HeadCache::new();
    let mut other_branch = 0usize;
    for file in &flagged {
        if reconcile_single_file(pool, queue_manager, stats, &mut heads, file).await {
            other_branch += 1;
        }
    }
    if other_branch > 0 {
        info!(
            "{} flagged file(s) belong only to branches their main folder does not have \
             checked out — left flagged for their own checkout's reconcile (worktree \
             membership re-ingest or branch pruning clears them)",
            other_branch
        );
    }
}

/// Reconcile a single flagged file: look up watch folder and re-queue.
/// Returns `true` when the row was left alone because it belongs only to
/// branches the main folder does not have checked out.
///
/// The `needs_reconcile` flag is left set regardless of whether the enqueue
/// succeeded or was deduplicated (F-020):
/// - If `is_new=true`: item enqueued; flag cleared when the item completes.
/// - If `is_new=false`: an identical item is already in flight; flag stays set
///   until that prior item completes.
/// - If watch folder is missing: flag cleared immediately (file is orphaned;
///   no repair is possible).
async fn reconcile_single_file(
    pool: &SqlitePool,
    queue_manager: &QueueManager,
    stats: &mut FullRecoveryStats,
    heads: &mut HeadCache,
    file: &tracked_files_schema::TrackedFile,
) -> bool {
    let Some((base_path, collection, tenant_id)) = routing_for(pool, stats, file).await else {
        return false;
    };

    // The main folder is the checkout of its HEAD branch only, and the item
    // below is stamped with that HEAD. A generation of another branch (a
    // linked worktree's, stored main-anchored) cannot be repaired from the
    // main folder: a worktree-only file reads as deleted there. The worktree
    // membership reconcile re-ingests those from their own checkout.
    let head = heads
        .entry(base_path.clone())
        .or_insert_with(|| crate::watching_queue::get_current_branch_opt(Path::new(&base_path)));
    if let Some(head) = head.as_deref() {
        if !file.branches.is_empty() && !file.branches.iter().any(|b| b == head) {
            debug!(
                "Reconcile file_id={} ({}): not on the main checkout's branch '{}' \
                 (branches={:?}) — left to its own checkout's reconcile",
                file.file_id,
                file.relative_path.as_str(),
                head,
                file.branches
            );
            return true;
        }
    }

    enqueue_repair(
        queue_manager,
        stats,
        file,
        &base_path,
        &collection,
        &tenant_id,
    )
    .await;
    false
}

/// The watch folder routing (`path`, `collection`, `tenant_id`) of a flagged
/// file, or `None` (with the error counted) when it cannot be resolved. A
/// vanished watch folder clears the flag: no repair is possible.
async fn routing_for(
    pool: &SqlitePool,
    stats: &mut FullRecoveryStats,
    file: &tracked_files_schema::TrackedFile,
) -> Option<(String, String, String)> {
    let wf = sqlx::query_as::<_, (String, String, String)>(
        "SELECT path, collection, tenant_id FROM watch_folders WHERE watch_id = ?1",
    )
    .bind(&file.watch_folder_id)
    .fetch_optional(pool)
    .await;

    match wf {
        Ok(Some(row)) => Some(row),
        Ok(None) => {
            warn!(
                "Watch folder {} not found for reconcile file_id={}, clearing flag",
                file.watch_folder_id, file.file_id
            );
            // Watch folder gone: no repair possible, clear the flag so this
            // file does not block future reconciliation passes.
            let _ = clear_reconcile_flag_direct(pool, file.file_id).await;
            stats.reconcile_errors += 1;
            None
        }
        Err(e) => {
            warn!(
                "Failed to query watch_folder {}: {}",
                file.watch_folder_id, e
            );
            stats.reconcile_errors += 1;
            None
        }
    }
}

/// Re-queue a flagged file from the main folder: Update when it is on disk,
/// Delete when it is gone.
async fn enqueue_repair(
    queue_manager: &QueueManager,
    stats: &mut FullRecoveryStats,
    file: &tracked_files_schema::TrackedFile,
    base_path: &str,
    collection: &str,
    tenant_id: &str,
) {
    let abs_path = Path::new(base_path).join(file.relative_path.as_str());
    let op = if abs_path.exists() {
        QueueOperation::Update
    } else {
        QueueOperation::Delete
    };

    match enqueue_file_op(
        queue_manager,
        tenant_id,
        collection,
        &file.relative_path,
        Path::new(base_path),
        op.clone(),
        None,
    )
    .await
    {
        Ok(is_new) => {
            // F-020: do NOT clear needs_reconcile here. The flag is cleared by
            // `QueueManager::delete_unified_item` when the queue item completes.
            if is_new {
                info!(
                    "Reconciled file_id={} ({}): enqueued for {}",
                    file.file_id,
                    file.relative_path.as_str(),
                    op.as_str()
                );
            } else {
                debug!(
                    "Reconcile file_id={} ({}): op={} already in queue, flag kept",
                    file.file_id,
                    file.relative_path.as_str(),
                    op.as_str()
                );
            }
            stats.reconciled += 1;
        }
        Err(e) => {
            warn!(
                "Failed to re-queue reconcile file {}: {}",
                file.relative_path.as_str(),
                e
            );
            stats.reconcile_errors += 1;
        }
    }
}

/// Clear the needs_reconcile flag for a tracked file directly.
///
/// Only called when reconciliation is impossible (e.g. watch folder missing).
/// Normal completions go through `QueueManager::delete_unified_item` instead.
async fn clear_reconcile_flag_direct(pool: &SqlitePool, file_id: i64) -> Result<(), sqlx::Error> {
    let now = wqm_common::timestamps::now_utc();
    sqlx::query(
        "UPDATE tracked_files SET needs_reconcile = 0, reconcile_reason = NULL, updated_at = ?1
         WHERE file_id = ?2",
    )
    .bind(&now)
    .bind(file_id)
    .execute(pool)
    .await?;
    Ok(())
}
