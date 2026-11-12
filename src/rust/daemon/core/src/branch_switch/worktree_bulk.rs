//! Worktree files whose content the index already holds: tagged in bulk.
//!
//! A worktree reconcile used to enqueue one `File/Add` per file of the
//! worktree, ~4–6k items per new worktree. Almost all of those files hold
//! content some branch has already indexed — the trunk, the branch the
//! worktree was created from, or a sibling agent branch — and each item only
//! appended the branch to an existing generation through the per-file dedup
//! fast-path. Measured 2026-11-10: every DOC-V2 worktree enqueued its whole
//! tree (~6,166 items), and a tecsul worktree ~4k, of which ~2.6k were content
//! only sibling branches created from `release/platform` had (the main
//! checkout is ~870 commits behind them).
//!
//! Comparing against the main HEAD (git no-diff) cannot certify that content:
//! where a branch was created from varies and git does not record it reliably
//! (a reflog entry, local and expiring, often just "Created from HEAD"). The
//! content hash certifies it with no guess: each working-tree file of the
//! branch is hashed exactly as the ingest hashes it, and a path whose
//! `(path, hash)` the main folder's index holds is tagged by a bulk op
//! (`BranchMembershipBulk::hashes`) — no read, no embed, ~25 paths per op.
//! Everything else (content no branch has indexed yet) still goes per file.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use sqlx::SqlitePool;
use tracing::{info, warn};

use crate::allowed_extensions::AllowedExtensions;
use crate::queue_operations::QueueManager;

use super::db::fetch_paths_missing_branch;
use super::queue::enqueue_branch_membership_bulk_by_hash;
use super::worktree_eligibility::{main_eligibility_gate, worktree_path_eligible};
use super::worktree_membership::WorktreeRef;

/// Paths per `IN (...)` lookup of indexed hashes (under SQLite's bound-param cap).
const HASH_LOOKUP_CHUNK: usize = 500;

/// Tag `target.branch` in bulk on every worktree file the branch holds no
/// generation of yet and whose working-tree content the main folder's index
/// already holds. Returns the paths enqueued, which the per-file sets skip.
pub(super) async fn enqueue_worktree_bulk(
    pool: &SqlitePool,
    queue_manager: &QueueManager,
    target: &WorktreeRef<'_>,
    allowed_extensions: &AllowedExtensions,
) -> HashSet<String> {
    let entries = match hash_matched_entries(pool, target, allowed_extensions).await {
        Ok(e) => e,
        Err(e) => {
            warn!("worktree bulk: branch '{}': {}", target.branch, e);
            return HashSet::new();
        }
    };
    if entries.is_empty() {
        return HashSet::new();
    }
    match enqueue_branch_membership_bulk_by_hash(
        queue_manager,
        target.tenant_id,
        target.collection,
        target.main_watch_folder_id,
        target.main_project_root,
        target.branch,
        &entries,
    )
    .await
    {
        Ok(n) => {
            info!(
                "worktree bulk: {} file(s) of '{}' already indexed by content; tagged in bulk \
                 (worktree {})",
                n, target.branch, target.wt_root
            );
            entries.into_iter().map(|(path, _)| path).collect()
        }
        Err(e) => {
            // Nothing covered: the per-file sets enqueue everything as before.
            warn!("worktree bulk: branch '{}': {}", target.branch, e);
            HashSet::new()
        }
    }
}

/// `(path, hash)` of each eligible worktree file the branch holds no
/// generation of, whose hash matches a live generation of the path.
async fn hash_matched_entries(
    pool: &SqlitePool,
    target: &WorktreeRef<'_>,
    allowed_extensions: &AllowedExtensions,
) -> Result<Vec<(String, String)>, String> {
    let missing =
        fetch_paths_missing_branch(pool, target.main_watch_folder_id, target.branch).await?;
    if missing.is_empty() {
        return Ok(Vec::new());
    }
    // The main scan's eligibility, as every worktree set applies it.
    let gate = main_eligibility_gate(target.main_project_root);
    let eligible: Vec<String> = missing
        .into_iter()
        .filter(|rel| {
            worktree_path_eligible(
                target.main_project_root,
                &gate,
                allowed_extensions,
                target.collection,
                rel,
            )
        })
        .collect();
    let root = PathBuf::from(target.wt_root);
    let hashed = tokio::task::spawn_blocking(move || hash_worktree_files(&root, eligible))
        .await
        .map_err(|e| format!("hash worktree files: {e}"))?;
    let indexed = indexed_hashes(pool, target.main_watch_folder_id, &hashed).await?;
    Ok(hashed
        .into_iter()
        .filter(|(path, hash)| indexed.get(path).is_some_and(|h| h.contains(hash)))
        .collect())
}

/// Hash each of `rels` in the worktree tree, as the ingest hashes a file.
/// Paths that are not a regular file there (deleted on the branch) are left out.
fn hash_worktree_files(root: &Path, rels: Vec<String>) -> Vec<(String, String)> {
    rels.into_iter()
        .filter_map(|rel| {
            let abs = root.join(&rel);
            if !abs.is_file() {
                return None;
            }
            crate::tracked_files_schema::compute_file_hash(&abs)
                .ok()
                .map(|hash| (rel, hash))
        })
        .collect()
}

/// The paths of `candidates` whose worktree bytes `target.branch` ALREADY
/// holds: a generation of the path with that exact hash carries the branch.
/// Re-ingesting them changes nothing, so the divergent set skips them.
///
/// The divergent set is git's diff between the MAIN HEAD and the branch tip,
/// and it does not shrink once the branch has been indexed. A main checkout
/// behind the base the agents branch from makes it huge on every worktree.
/// On 2026-11-12 every daemon restart re-queued ~1.8k files per DOC-V2
/// worktree (main 412 commits behind origin), 38k items in all, and a sample
/// showed 398 of 400 already carried their branch with that content. A lookup
/// failure skips nothing, so the per-file path runs as before.
pub(super) async fn paths_current_on_branch(
    pool: &SqlitePool,
    target: &WorktreeRef<'_>,
    candidates: &HashSet<String>,
) -> HashSet<String> {
    if candidates.is_empty() {
        return HashSet::new();
    }
    let root = PathBuf::from(target.wt_root);
    let rels: Vec<String> = candidates.iter().cloned().collect();
    let Ok(hashed) = tokio::task::spawn_blocking(move || hash_worktree_files(&root, rels)).await
    else {
        return HashSet::new();
    };
    match held_hashes(pool, target.main_watch_folder_id, target.branch, &hashed).await {
        Ok(held) => hashed
            .into_iter()
            .filter(|(path, hash)| held.get(path).is_some_and(|h| h.contains(hash)))
            .map(|(path, _)| path)
            .collect(),
        Err(e) => {
            warn!("worktree divergent: branch '{}': {}", target.branch, e);
            HashSet::new()
        }
    }
}

/// For each path of `hashed`, the content hashes of its generations under
/// `watch_folder_id` that carry `branch`.
async fn held_hashes(
    pool: &SqlitePool,
    watch_folder_id: &str,
    branch: &str,
    hashed: &[(String, String)],
) -> Result<HashMap<String, HashSet<String>>, String> {
    let mut out: HashMap<String, HashSet<String>> = HashMap::new();
    for chunk in hashed.chunks(HASH_LOOKUP_CHUNK) {
        let ph = vec!["?"; chunk.len()].join(", ");
        let sql = format!(
            "SELECT t.relative_path, t.file_hash FROM tracked_files t, json_each(t.branches) b
              WHERE t.watch_folder_id = ? AND b.value = ? AND t.relative_path IN ({ph})"
        );
        let mut q = sqlx::query_as::<_, (String, String)>(&sql)
            .bind(watch_folder_id)
            .bind(branch);
        for (path, _) in chunk {
            q = q.bind(path);
        }
        for (path, hash) in q
            .fetch_all(pool)
            .await
            .map_err(|e| format!("lookup held hashes: {e}"))?
        {
            out.entry(path).or_default().insert(hash);
        }
    }
    Ok(out)
}

/// For each path of `hashed`, the content hashes of its generations under
/// `watch_folder_id` that some branch still holds (the same generations the
/// bulk processor may tag).
async fn indexed_hashes(
    pool: &SqlitePool,
    watch_folder_id: &str,
    hashed: &[(String, String)],
) -> Result<HashMap<String, HashSet<String>>, String> {
    let mut out: HashMap<String, HashSet<String>> = HashMap::new();
    for chunk in hashed.chunks(HASH_LOOKUP_CHUNK) {
        let ph = vec!["?"; chunk.len()].join(", ");
        let sql = format!(
            "SELECT relative_path, file_hash FROM tracked_files
              WHERE watch_folder_id = ? AND base_point IS NOT NULL
                AND branches IS NOT NULL AND json_array_length(branches) > 0
                AND relative_path IN ({ph})"
        );
        let mut q = sqlx::query_as::<_, (String, String)>(&sql).bind(watch_folder_id);
        for (path, _) in chunk {
            q = q.bind(path);
        }
        for (path, hash) in q
            .fetch_all(pool)
            .await
            .map_err(|e| format!("lookup indexed hashes: {e}"))?
        {
            out.entry(path).or_default().insert(hash);
        }
    }
    Ok(out)
}
