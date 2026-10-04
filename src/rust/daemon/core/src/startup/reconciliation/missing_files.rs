//! Startup steps 4b and 5 (F-036): tracked files missing on disk.
//!
//! "Missing on disk" is per `(path, branch)`: a row's branch set names
//! several checkouts — the main folder for the main HEAD, a linked worktree
//! for each worktree branch — and its `relative_path` is main-anchored for
//! all of them. Each branch is judged in its OWN checkout; a branch nobody
//! has checked out is left to branch pruning. Judging every branch at the
//! main root deleted every file that existed only in a worktree, and step 5
//! then dropped the rows outright (2026-10-03).

use std::collections::{HashMap, HashSet};

use sqlx::sqlite::SqliteRow;
use sqlx::{Row, SqlitePool};
use tracing::{debug, info, warn};

use crate::git::{BranchCheckouts, CheckoutPresence};
use crate::queue_operations::QueueManager;
use crate::unified_queue_schema::{FilePayload, ItemType, QueueOperation};
use crate::watching_queue::WatchManager;
use wqm_common::paths::RelativePath;

/// One `(row, branch)` pair of `tracked_files` with its routing.
const TRACKED_BRANCH_ROWS_SQL: &str =
    "SELECT tf.file_id, tf.relative_path, wf.path AS watch_path, \
            wf.tenant_id, wf.collection, je.value AS branch \
     FROM tracked_files tf \
     JOIN watch_folders wf ON tf.watch_folder_id = wf.watch_id, \
          json_each(tf.branches) je";

/// Branch checkouts per watch folder, discovered once per pass.
#[derive(Default)]
struct CheckoutCache(HashMap<String, BranchCheckouts>);

impl CheckoutCache {
    fn of(&mut self, watch_path: &str) -> &BranchCheckouts {
        self.0.entry(watch_path.to_string()).or_insert_with(|| {
            BranchCheckouts::discover(&WatchManager::resolve_local_watch_path(watch_path))
        })
    }
}

/// Step 4b (F-036): Enqueue Delete ops for tracked files that no longer exist on disk.
///
/// Iterates every `(row, branch)` pair, checks the file in that branch's own
/// checkout, and enqueues a `(File, Delete)` queue item for each pair whose
/// checkout lost it. The composite uniqueness key
/// `(tenant_id, branch, collection, item_type, op, file_path)` makes this
/// operation idempotent: re-running on the next startup will silently skip
/// files that already have a pending or in-progress Delete item.
pub(super) async fn enqueue_delete_for_missing_tracked_files(
    pool: &SqlitePool,
    queue_manager: &QueueManager,
) -> Result<u64, String> {
    info!("Enqueueing Delete ops for tracked files missing on disk (F-036)...");
    let tracked_rows = sqlx::query(TRACKED_BRANCH_ROWS_SQL)
        .fetch_all(pool)
        .await
        .map_err(|e| format!("Failed to query tracked files for delete enqueue: {}", e))?;

    let mut checkouts = CheckoutCache::default();
    let mut enqueued: u64 = 0;
    let mut unjudged: u64 = 0;
    for row in &tracked_rows {
        let relative_path: String = row.get("relative_path");
        let watch_path: String = row.get("watch_path");
        let branch: String = row.get("branch");
        let repo = checkouts.of(&watch_path);
        match repo.presence(&branch, &relative_path) {
            CheckoutPresence::Present => {}
            CheckoutPresence::NoCheckout => unjudged += 1,
            CheckoutPresence::Missing => {
                let abs_path = repo
                    .root_for(&branch)
                    .map(|root| root.join(&relative_path))
                    .unwrap_or_default();
                if enqueue_missing_delete(queue_manager, row, &abs_path.to_string_lossy()).await? {
                    enqueued += 1;
                }
            }
        }
    }

    if enqueued > 0 {
        info!(
            "Enqueued {} Delete op(s) for tracked files missing on disk",
            enqueued
        );
    } else {
        debug!("No missing tracked files require Delete enqueue");
    }
    if unjudged > 0 {
        debug!(
            "{} branch tag(s) have no checkout to judge them by; left to branch pruning",
            unjudged
        );
    }
    Ok(enqueued)
}

/// Enqueue the Delete for one missing `(row, branch)` pair. `Ok(true)` when a
/// new item was queued; a dedup (already queued) or a skipped invalid path is
/// `Ok(false)`; only a payload serialization failure is an `Err`.
async fn enqueue_missing_delete(
    queue_manager: &QueueManager,
    row: &SqliteRow,
    abs_path_str: &str,
) -> Result<bool, String> {
    let relative_path: String = row.get("relative_path");
    let tenant_id: String = row.get("tenant_id");
    let collection: String = row.get("collection");
    let branch: String = row.get("branch");
    let Some(payload_json) = delete_payload(&relative_path)? else {
        return Ok(false);
    };
    match queue_manager
        .enqueue_unified(
            ItemType::File,
            QueueOperation::Delete,
            &tenant_id,
            &collection,
            &payload_json,
            Some(&branch),
            None,
        )
        .await
    {
        Ok((_, true)) => {
            debug!(
                "Enqueued Delete for missing tracked file: {} (branch {})",
                abs_path_str, branch
            );
            Ok(true)
        }
        Ok((_, false)) => {
            // Idempotent dedup: Delete already queued from a previous startup.
            debug!(
                "Delete already queued for missing tracked file: {}",
                abs_path_str
            );
            Ok(false)
        }
        Err(e) => {
            warn!(
                "Failed to enqueue Delete for missing tracked file {}: {}",
                abs_path_str, e
            );
            Ok(false)
        }
    }
}

/// The `file|delete` payload for a tracked relative path, or `None` (logged)
/// when the stored path fails validation.
fn delete_payload(relative_path: &str) -> Result<Option<String>, String> {
    // `tracked_files.relative_path` is already validated; use it as the
    // anchored payload form directly.
    let relative = match RelativePath::from_user_input(relative_path) {
        Ok(r) => r,
        Err(e) => {
            warn!(
                "tracked_files.relative_path {:?} failed validation: {}",
                relative_path, e
            );
            return Ok(None);
        }
    };
    let file_payload = FilePayload {
        file_path: relative,
        file_type: None,
        file_hash: None,
        size_bytes: None,
        old_path: None,
    };
    serde_json::to_string(&file_payload)
        .map(Some)
        .map_err(|e| format!("Failed to serialize FilePayload: {}", e))
}

/// Step 5 (F-036): Remove tracked_files entries whose files no longer exist on
/// disk AND whose Delete op is not still in flight.
///
/// A row is a candidate only when EVERY branch it carries lost the file in
/// that branch's own checkout: a row still present in one checkout, or
/// tagged with a branch no checkout has (branch pruning's business), stays.
/// It is then only removed when the corresponding Delete queue item has been
/// durably processed (deleted from queue = done) or never existed. If a
/// pending or in-progress Delete item exists for the file, the row is kept so
/// that the Delete handler can still resolve the file's identity on completion.
///
/// Post-T7: the in-flight check matches on the relative path that is stored in
/// `unified_queue.file_path` and is scoped to the row's tenant + collection so
/// that two tenants sharing an identical relative path under different
/// watch-folder roots do not cross-contaminate.
pub(super) async fn remove_stale_tracked_files(pool: &SqlitePool) -> Result<u64, String> {
    info!("Checking tracked files against filesystem...");
    let tracked_rows = sqlx::query(TRACKED_BRANCH_ROWS_SQL)
        .fetch_all(pool)
        .await
        .map_err(|e| format!("Failed to query tracked files: {}", e))?;

    let mut removable_file_ids: Vec<i64> = Vec::new();
    for (file_id, relative_path, tenant_id, collection) in rows_missing_everywhere(&tracked_rows) {
        // File is missing on disk for every branch it carries. Only remove the
        // tracked_files row if no Delete for it is in flight: one in flight
        // means the downstream cleanup (Qdrant, FTS, graph) has not yet been
        // applied — removing the row now would orphan that state.
        match delete_in_flight(pool, &relative_path, &tenant_id, &collection).await {
            Some(0) => {
                debug!(
                    "Tracked file missing on disk and no Delete in flight, removing: {}",
                    relative_path
                );
                removable_file_ids.push(file_id);
            }
            Some(_) => debug!(
                "Keeping tracked_files row for {} — Delete op still in flight",
                relative_path
            ),
            None => {}
        }
    }

    if removable_file_ids.is_empty() {
        debug!("All tracked files still exist on disk or have Delete ops in flight");
        return Ok(0);
    }
    delete_rows_chunked(pool, &removable_file_ids).await?;
    let count = removable_file_ids.len() as u64;
    info!(
        "Removed {} stale tracked files (Delete not in flight)",
        count
    );
    Ok(count)
}

/// `(file_id, relative_path, tenant_id, collection)` of every row whose file
/// is missing from the checkout of EVERY branch it carries — a row present in
/// one checkout, or tagged with a branch no checkout has, is not a candidate.
fn rows_missing_everywhere(rows: &[SqliteRow]) -> Vec<(i64, String, String, String)> {
    let mut checkouts = CheckoutCache::default();
    let mut keep: HashSet<i64> = HashSet::new();
    let mut seen: HashSet<i64> = HashSet::new();
    let mut candidates = Vec::new();
    for row in rows {
        let file_id: i64 = row.get("file_id");
        let relative_path: String = row.get("relative_path");
        let watch_path: String = row.get("watch_path");
        let branch: String = row.get("branch");
        if checkouts.of(&watch_path).presence(&branch, &relative_path) != CheckoutPresence::Missing
        {
            keep.insert(file_id);
        } else if seen.insert(file_id) {
            candidates.push((
                file_id,
                relative_path,
                row.get("tenant_id"),
                row.get("collection"),
            ));
        }
    }
    candidates.retain(|(file_id, ..)| !keep.contains(file_id));
    candidates
}

/// Pending/in-progress file Deletes for a path in a tenant + collection, or
/// `None` (logged) when the check itself failed — the caller then keeps the
/// row, preserving the F-036 safety guarantee.
async fn delete_in_flight(
    pool: &SqlitePool,
    relative_path: &str,
    tenant_id: &str,
    collection: &str,
) -> Option<i64> {
    match sqlx::query_scalar(
        "SELECT COUNT(*) FROM unified_queue \
         WHERE file_path = ?1 \
           AND tenant_id = ?2 \
           AND collection = ?3 \
           AND op = 'delete' \
           AND item_type = 'file' \
           AND status IN ('pending', 'in_progress')",
    )
    .bind(relative_path)
    .bind(tenant_id)
    .bind(collection)
    .fetch_one(pool)
    .await
    {
        Ok(count) => Some(count),
        Err(e) => {
            warn!(
                "Failed to check in-flight Delete for {} (tenant={}, collection={}): {} — skipping row to preserve F-036 safety guarantee",
                relative_path, tenant_id, collection, e
            );
            None
        }
    }
}

/// `DELETE FROM tracked_files` by id, 500 ids per statement.
async fn delete_rows_chunked(pool: &SqlitePool, file_ids: &[i64]) -> Result<(), String> {
    for chunk in file_ids.chunks(500) {
        let placeholders: String = chunk
            .iter()
            .map(|id| id.to_string())
            .collect::<Vec<_>>()
            .join(",");
        let delete_sql = format!(
            "DELETE FROM tracked_files WHERE file_id IN ({})",
            placeholders
        );
        sqlx::query(&delete_sql)
            .execute(pool)
            .await
            .map_err(|e| format!("Failed to delete stale tracked files: {}", e))?;
    }
    Ok(())
}
