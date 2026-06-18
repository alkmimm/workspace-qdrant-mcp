//! Event handlers for branch switch and commit git events.

use std::collections::HashSet;
use std::path::Path;

use sqlx::SqlitePool;
use tracing::{debug, info, warn};

use crate::git::{diff_tree, FileChange, GitEvent, GitEventType};
use crate::queue_operations::QueueManager;
use crate::unified_queue_schema::QueueOperation;
use crate::watching_queue::get_current_branch;

use super::db::{fetch_unchanged_relative_paths, fetch_watch_folder, update_last_commit_hash};
use super::queue::{enqueue_changed_file, enqueue_tenant_scan, enqueue_unchanged_file};
use super::types::BranchSwitchStats;

/// Handle a git event by dispatching to the appropriate handler.
pub async fn handle_git_event(
    event: &GitEvent,
    pool: &SqlitePool,
    queue_manager: &QueueManager,
) -> Result<BranchSwitchStats, String> {
    let (project_root, collection, tenant_id) =
        fetch_watch_folder(pool, &event.watch_folder_id).await?;

    match &event.event_type {
        GitEventType::BranchSwitch => {
            handle_branch_switch(
                event,
                pool,
                queue_manager,
                &project_root,
                &collection,
                &tenant_id,
            )
            .await
        }
        GitEventType::Commit | GitEventType::Merge | GitEventType::Pull | GitEventType::Rebase => {
            handle_new_commit(
                event,
                pool,
                queue_manager,
                &project_root,
                &collection,
                &tenant_id,
            )
            .await
        }
        GitEventType::Reset => {
            // Reset can change arbitrary files -- enqueue a full scan
            info!(
                "Git reset detected for {}, enqueueing full scan",
                event.watch_folder_id
            );
            enqueue_tenant_scan(queue_manager, &tenant_id, &collection, &project_root).await?;
            Ok(BranchSwitchStats::default())
        }
        GitEventType::Stash | GitEventType::Unknown => {
            debug!(
                "Ignoring git event {:?} for {}",
                event.event_type, event.watch_folder_id
            );
            Ok(BranchSwitchStats::default())
        }
    }
}

/// Handle a branch switch: diff-tree for changes, enqueue unchanged files for
/// dedup re-key, enqueue changed files for full ingest.
async fn handle_branch_switch(
    event: &GitEvent,
    pool: &SqlitePool,
    queue_manager: &QueueManager,
    project_root: &str,
    collection: &str,
    tenant_id: &str,
) -> Result<BranchSwitchStats, String> {
    let root = Path::new(project_root);
    let new_branch = event.branch.as_deref().unwrap_or("default");
    let old_branch = event
        .old_branch
        .as_deref()
        .unwrap_or_else(|| get_current_branch(root).leak());

    info!(
        "Branch switch: {} -> {} for {} (old_sha={:.8}, new_sha={:.8})",
        old_branch, new_branch, event.watch_folder_id, &event.old_sha, &event.new_sha
    );

    let changes = diff_tree(root, &event.old_sha, &event.new_sha)
        .map_err(|e| format!("diff_tree failed: {}", e))?;

    let changed_paths: HashSet<String> = changes.iter().map(|c| c.path.clone()).collect();
    let mut stats = BranchSwitchStats::default();

    // 1. Re-key unchanged files onto the new branch via the dedup fast-path.
    //    Enqueuing them as Add ops lets try_branch_dedup copy the existing
    //    Qdrant points + FTS5 rows under the new branch (no re-embed). The old
    //    SQL-only re-key relabelled tracked_files but left Qdrant + search.db
    //    on the old branch, so search/grep returned empty on the new branch.
    enqueue_unchanged_files(
        pool,
        queue_manager,
        &event.watch_folder_id,
        old_branch,
        new_branch,
        tenant_id,
        collection,
        &changed_paths,
        &mut stats,
    )
    .await;

    // 2. Enqueue changed files for re-ingestion
    enqueue_all_changed_files(
        queue_manager,
        &changes,
        tenant_id,
        collection,
        project_root,
        new_branch,
        &mut stats,
    )
    .await;

    // 3. Update last_commit_hash in watch_folders
    if let Err(e) = update_last_commit_hash(pool, &event.watch_folder_id, &event.new_sha).await {
        warn!("Failed to update last_commit_hash: {}", e);
        stats.errors += 1;
    }

    info!(
        "Branch switch complete for {}: {} unchanged re-keyed, {} changed, {} added, {} deleted, {} errors",
        event.watch_folder_id, stats.enqueued_unchanged, stats.enqueued_changed,
        stats.enqueued_added, stats.enqueued_deleted, stats.errors
    );

    Ok(stats)
}

/// Enqueue unchanged files (tracked on the old branch, byte-identical content)
/// as `Add` ops on the new branch so the cross-branch dedup fast-path re-keys
/// their Qdrant points + FTS5 rows. Paths present in the diff are skipped —
/// they changed and take the full-ingest path via `enqueue_all_changed_files`.
#[allow(clippy::too_many_arguments)]
async fn enqueue_unchanged_files(
    pool: &SqlitePool,
    queue_manager: &QueueManager,
    watch_folder_id: &str,
    old_branch: &str,
    new_branch: &str,
    tenant_id: &str,
    collection: &str,
    changed_paths: &HashSet<String>,
    stats: &mut BranchSwitchStats,
) {
    let unchanged =
        match fetch_unchanged_relative_paths(pool, watch_folder_id, old_branch, new_branch).await {
            Ok(v) => v,
            Err(e) => {
                warn!("Failed to fetch unchanged paths for branch switch: {}", e);
                stats.errors += 1;
                return;
            }
        };

    for rel in unchanged {
        if changed_paths.contains(&rel) {
            continue;
        }
        match enqueue_unchanged_file(queue_manager, tenant_id, collection, &rel, new_branch).await {
            Ok(()) => stats.enqueued_unchanged += 1,
            Err(e) => {
                warn!(
                    "Failed to enqueue unchanged file {} on {}: {}",
                    rel, new_branch, e
                );
                stats.errors += 1;
            }
        }
    }

    if stats.enqueued_unchanged > 0 {
        info!(
            "Enqueued {} unchanged files for dedup re-key: branch {} -> {}",
            stats.enqueued_unchanged, old_branch, new_branch
        );
    }
}

/// Enqueue all changed files for re-ingestion and accumulate stats.
async fn enqueue_all_changed_files(
    queue_manager: &QueueManager,
    changes: &[FileChange],
    tenant_id: &str,
    collection: &str,
    project_root: &str,
    branch: &str,
    stats: &mut BranchSwitchStats,
) {
    for change in changes {
        let result = enqueue_changed_file(
            queue_manager,
            change,
            tenant_id,
            collection,
            project_root,
            branch,
        )
        .await;
        match result {
            Ok(op) => match op {
                QueueOperation::Update => stats.enqueued_changed += 1,
                QueueOperation::Add => stats.enqueued_added += 1,
                QueueOperation::Delete => stats.enqueued_deleted += 1,
                _ => {}
            },
            Err(e) => {
                warn!("Failed to enqueue changed file {}: {}", change.path, e);
                stats.errors += 1;
            }
        }
    }
}

/// Handle a new commit on the same branch: diff-tree vs parent, enqueue changed files.
async fn handle_new_commit(
    event: &GitEvent,
    pool: &SqlitePool,
    queue_manager: &QueueManager,
    project_root: &str,
    collection: &str,
    tenant_id: &str,
) -> Result<BranchSwitchStats, String> {
    let root = Path::new(project_root);
    let branch = event.branch.as_deref().unwrap_or("default");

    info!(
        "New commit on branch {} for {} (old_sha={:.8}, new_sha={:.8})",
        branch, event.watch_folder_id, &event.old_sha, &event.new_sha
    );

    let changes = diff_tree(root, &event.old_sha, &event.new_sha)
        .map_err(|e| format!("diff_tree failed: {}", e))?;

    let mut stats = BranchSwitchStats::default();

    enqueue_all_changed_files(
        queue_manager,
        &changes,
        tenant_id,
        collection,
        project_root,
        branch,
        &mut stats,
    )
    .await;

    // Update last_commit_hash
    if let Err(e) = update_last_commit_hash(pool, &event.watch_folder_id, &event.new_sha).await {
        warn!("Failed to update last_commit_hash: {}", e);
        stats.errors += 1;
    }

    info!(
        "Commit processed for {}: {} changed, {} added, {} deleted, {} errors",
        event.watch_folder_id,
        stats.enqueued_changed,
        stats.enqueued_added,
        stats.enqueued_deleted,
        stats.errors
    );

    Ok(stats)
}
