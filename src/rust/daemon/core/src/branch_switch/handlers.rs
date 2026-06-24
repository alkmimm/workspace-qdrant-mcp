//! Event handlers for branch switch and commit git events.

use std::collections::HashSet;
use std::path::Path;

use sqlx::SqlitePool;
use tracing::{debug, info, warn};

use crate::git::{diff_tree, FileChange, GitEvent, GitEventType};
use crate::queue_operations::QueueManager;
use crate::unified_queue_schema::QueueOperation;
use crate::watching_queue::get_current_branch;

use super::db::{
    fetch_paths_missing_branch, fetch_unchanged_relative_paths, fetch_watch_folder,
    update_last_commit_hash,
};
use super::queue::{
    enqueue_branch_membership_bulk, enqueue_changed_file, enqueue_tenant_scan,
    enqueue_unchanged_file,
};
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
        project_root,
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
/// for cross-branch dedup re-key onto the new branch. Paths present in the diff
/// are skipped — they changed and take the full-ingest path via
/// `enqueue_all_changed_files`.
///
/// These paths produced NO diff between the two commits, so they are git-identical
/// on both branches. That lets us BULK the re-key: instead of one `Add` per file
/// (each draining individually and flooding `pending`), we enqueue a handful of
/// `(Tenant, Scan)` ops carrying the verified path list, and the processor appends
/// the branch to all three stores in batches without re-embedding.
#[allow(clippy::too_many_arguments)]
async fn enqueue_unchanged_files(
    pool: &SqlitePool,
    queue_manager: &QueueManager,
    watch_folder_id: &str,
    old_branch: &str,
    new_branch: &str,
    tenant_id: &str,
    collection: &str,
    project_root: &str,
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

    // Drop paths that genuinely changed (they take the full-ingest path); what
    // remains is byte-identical on both branches and safe to re-key by path.
    let to_rekey: Vec<String> = unchanged
        .into_iter()
        .filter(|rel| !changed_paths.contains(rel))
        .collect();

    match enqueue_branch_membership_bulk(
        queue_manager,
        tenant_id,
        collection,
        watch_folder_id,
        project_root,
        new_branch,
        to_rekey,
    )
    .await
    {
        Ok(n) => stats.enqueued_unchanged += n as u64,
        Err(e) => {
            warn!(
                "Failed to enqueue bulk branch re-key {} -> {}: {}",
                old_branch, new_branch, e
            );
            stats.errors += 1;
        }
    }

    if stats.enqueued_unchanged > 0 {
        info!(
            "Enqueued {} unchanged files for BULK dedup re-key: branch {} -> {}",
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

/// Reconcile branch membership for files the live git-watcher path missed.
///
/// `handle_branch_switch` only fires on a checkout the [`GitWatcher`] actually
/// observes (with valid old/new SHAs). Checkouts that predate the watch, happen
/// while the daemon is down, or land on a synthesized project leave already-
/// indexed files tagged only under their old branch — so a branch-scoped
/// grep/search returns empty on the current branch even though `list` (which
/// reads the filesystem/tracked structure) still shows the file. The read-side
/// auto-widen (PR #151) masks this at query time; this is the write-side fix.
///
/// Runs on every tenant scan (startup, periodic refresh, reindex): for each file
/// tracked but NOT yet tagged with `branch` AND still present in the working
/// tree, re-enqueue it as an `Add` so the cross-branch dedup fast-path appends
/// the branch to all three stores (Qdrant payload, `tracked_files.branches`,
/// FTS5 `file_metadata.branches`) WITHOUT re-embedding. Files absent from the
/// working tree (deleted on this branch) are skipped — never enqueued — so this
/// never resurrects or over-deletes content. Idempotent and cheap: once a
/// project is reconciled the candidate set is empty and this is a single SELECT.
///
/// Returns the number of files enqueued for reconciliation.
pub async fn reconcile_branch_membership(
    pool: &SqlitePool,
    queue_manager: &QueueManager,
    watch_folder_id: &str,
    tenant_id: &str,
    collection: &str,
    project_root: &str,
    branch: &str,
) -> usize {
    let candidates = match fetch_paths_missing_branch(pool, watch_folder_id, branch).await {
        Ok(v) => v,
        Err(e) => {
            warn!(
                "Branch reconcile: failed to fetch candidates for {}: {}",
                watch_folder_id, e
            );
            return 0;
        }
    };
    if candidates.is_empty() {
        return 0;
    }

    let root = Path::new(project_root);
    let mut enqueued = 0usize;
    for rel in candidates {
        // Only reconcile files that actually exist on the current branch's
        // working tree — a path deleted on this branch must NOT be re-added
        // (that would resurrect it; enqueuing it would also hit the
        // missing-file cleanup path and risk an over-delete).
        if !root.join(&rel).exists() {
            continue;
        }
        match enqueue_unchanged_file(queue_manager, tenant_id, collection, &rel, branch).await {
            Ok(()) => enqueued += 1,
            Err(e) => warn!(
                "Branch reconcile: failed to enqueue {} on {}: {}",
                rel, branch, e
            ),
        }
    }

    if enqueued > 0 {
        info!(
            "Branch reconcile: enqueued {} working-tree files missing branch '{}' (wf={})",
            enqueued, branch, watch_folder_id
        );
    }
    enqueued
}
