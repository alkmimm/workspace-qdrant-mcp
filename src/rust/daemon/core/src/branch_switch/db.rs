//! Database operations for branch switch: unchanged-file discovery and commit
//! hash tracking.

use std::collections::HashSet;

use sqlx::SqlitePool;
use wqm_common::timestamps;

/// Relative paths of files tracked on `old_branch` that are **not** yet tracked
/// on `new_branch`, each with its stored chunker fingerprint (`chunker_version`).
///
/// These are the cross-branch dedup candidates after a `git checkout`: a file
/// that did not appear in the diff has byte-identical content on both branches,
/// so re-enqueuing it as an `Add` op on `new_branch` lets the dedup fast-path
/// (`strategies::processing::file::branch_dedup`) re-key the existing Qdrant
/// points + FTS5 rows under the new branch without re-embedding. The caller drops
/// any path that genuinely changed (present in the diff), and uses the returned
/// `chunker_version` to route STALE files to the re-chunk path instead of a plain
/// branch-membership append (issue #246) — the bulk append never runs the
/// fingerprint gate, so a chunker/registry upgrade otherwise never reaches
/// content held only on a non-current branch until its content happens to change.
///
/// The `NOT EXISTS` clause makes repeated switches idempotent: a file already
/// tracked on the target branch is skipped here (and the dequeue-time hash gate
/// would Skip it anyway).
///
/// Returns `(relative_path, chunker_version, file_hash)`. The `file_hash` is the
/// stored content hash of the `old_branch` generation being considered for the
/// re-key; the caller compares it against the on-disk hash to reject re-keying a
/// generation whose content does not actually match the working tree (issue #224
/// cross-branch stale content).
pub async fn fetch_unchanged_paths_with_chunker(
    pool: &SqlitePool,
    watch_folder_id: &str,
    old_branch: &str,
    new_branch: &str,
) -> Result<Vec<(String, Option<String>, String)>, String> {
    let rows: Vec<(String, Option<String>, String)> = sqlx::query_as(
        "SELECT t.relative_path, t.chunker_version, t.file_hash
         FROM tracked_files t
         WHERE t.watch_folder_id = ?1
           AND EXISTS (SELECT 1 FROM json_each(t.branches) WHERE value = ?2)
           AND NOT EXISTS (
               SELECT 1 FROM tracked_files n, json_each(n.branches) nb
               WHERE n.watch_folder_id = t.watch_folder_id
                 AND n.relative_path = t.relative_path
                 AND nb.value = ?3
           )",
    )
    .bind(watch_folder_id)
    .bind(old_branch)
    .bind(new_branch)
    .fetch_all(pool)
    .await
    .map_err(|e| format!("Failed to fetch unchanged paths with chunker: {}", e))?;

    Ok(rows)
}

/// Relative paths tracked under SOME branch for this watch folder but NOT yet
/// under `branch` — the branch-membership reconcile candidates.
///
/// Complements [`fetch_unchanged_paths_with_chunker`], which only finds files the
/// live branch-switch path (old_branch -> new_branch, valid SHAs) can diff. This
/// query is event-independent: it catches files left tagged under an older branch
/// when the git-watcher never saw the checkout (daemon down during checkout, the
/// branch was already current when the project was first watched, or a
/// synthesized project). The caller intersects the result with the working tree
/// before re-enqueuing as an `Add` so the dedup fast-path appends the branch
/// without re-embedding. The `NOT EXISTS` makes it idempotent: a file already
/// tagged with `branch` is excluded, so a reconciled project yields an empty set.
pub async fn fetch_paths_missing_branch(
    pool: &SqlitePool,
    watch_folder_id: &str,
    branch: &str,
) -> Result<Vec<String>, String> {
    let rows: Vec<(String,)> = sqlx::query_as(
        "SELECT DISTINCT t.relative_path
         FROM tracked_files t
         WHERE t.watch_folder_id = ?1
           AND NOT EXISTS (SELECT 1 FROM json_each(t.branches) WHERE value = ?2)",
    )
    .bind(watch_folder_id)
    .bind(branch)
    .fetch_all(pool)
    .await
    .map_err(|e| format!("Failed to fetch paths missing branch: {}", e))?;

    Ok(rows.into_iter().map(|(p,)| p).collect())
}

/// Every `relative_path` tracked under `watch_folder_id`, regardless of branch.
///
/// The worktree new-on-branch discovery walks the worktree's working tree and
/// subtracts this set: any path the main folder already tracks (under any
/// branch) is either the shared baseline the membership reconcile tags by
/// reading from the main tree (`fetch_paths_missing_branch`) or a file already
/// tagged elsewhere — never a candidate for the worktree read-root path. What
/// remains is content that exists ONLY on the worktree branch, whose bytes live
/// solely in the worktree tree. Keeping the two candidate sets disjoint by
/// membership in `tracked_files` avoids an idempotency-key collision between a
/// baseline item (read from main) and a new-on-branch item (read from the
/// worktree) for the same `(path, branch)`.
pub async fn fetch_all_tracked_paths(
    pool: &SqlitePool,
    watch_folder_id: &str,
) -> Result<HashSet<String>, String> {
    let rows: Vec<(String,)> = sqlx::query_as(
        "SELECT DISTINCT relative_path FROM tracked_files WHERE watch_folder_id = ?1",
    )
    .bind(watch_folder_id)
    .fetch_all(pool)
    .await
    .map_err(|e| format!("Failed to fetch all tracked paths: {}", e))?;

    Ok(rows.into_iter().map(|(p,)| p).collect())
}

/// Update last_commit_hash in watch_folders.
pub async fn update_last_commit_hash(
    pool: &SqlitePool,
    watch_folder_id: &str,
    commit_hash: &str,
) -> Result<(), String> {
    let now = timestamps::now_utc();
    sqlx::query(
        "UPDATE watch_folders SET last_commit_hash = ?1, updated_at = ?2 WHERE watch_id = ?3",
    )
    .bind(commit_hash)
    .bind(&now)
    .bind(watch_folder_id)
    .execute(pool)
    .await
    .map_err(|e| format!("Failed to update last_commit_hash: {}", e))?;
    Ok(())
}

/// Fetch watch folder info: (path, collection, tenant_id).
pub async fn fetch_watch_folder(
    pool: &SqlitePool,
    watch_folder_id: &str,
) -> Result<(String, String, String), String> {
    sqlx::query_as::<_, (String, String, String)>(
        "SELECT path, collection, tenant_id FROM watch_folders WHERE watch_id = ?1",
    )
    .bind(watch_folder_id)
    .fetch_optional(pool)
    .await
    .map_err(|e| format!("Failed to query watch_folder: {}", e))?
    .ok_or_else(|| format!("Watch folder {} not found", watch_folder_id))
}
