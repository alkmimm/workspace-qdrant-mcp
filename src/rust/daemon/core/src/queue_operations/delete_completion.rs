//! F-036 post-completion cleanup of a file Delete, scoped to its branch.
//!
//! A path in `tracked_files` carries one row per content generation, each
//! tagged with the branches holding it (Layer 2). The delete handler already
//! drops exactly the item's branch and removes a row once its set empties;
//! this cleanup only sweeps what that left behind for the SAME branch.
//!
//! It used to remove EVERY row of the path whenever the file was missing from
//! the MAIN folder. A delete on one branch thereby erased the path for every
//! other branch — and a linked worktree's file, stored main-anchored, always
//! reads as missing there — stranding the other generations' Qdrant points
//! and search.db content: the next delete for another branch found no row and
//! fell through to the path-keyed Qdrant filter delete, and search.db kept the
//! orphaned `file_metadata` (2026-10-03: 135 such files in one tenant).

use std::path::Path;

use tracing::debug;

use crate::git::BranchCheckouts;

use super::{QueueManager, QueueResult};

impl QueueManager {
    /// Remove the rows of `relative_file_path` whose branch set holds nothing
    /// but `branch` (or nothing at all) — leftovers of the completed delete.
    /// Rows still tagged with any other branch belong to that branch and are
    /// never touched. With no branch on the item, only empty-set rows go.
    ///
    /// Scoped to `(tenant_id, collection)` via the owning `watch_folders` row
    /// so two tenants sharing a relative path never cross-delete.
    pub(super) async fn remove_branch_leftover_rows(
        &self,
        relative_file_path: &str,
        tenant_id: &str,
        collection: &str,
        branch: Option<&str>,
    ) -> QueueResult<()> {
        let rows = sqlx::query(
            "DELETE FROM tracked_files \
             WHERE file_id IN ( \
                 SELECT tf.file_id \
                 FROM tracked_files tf \
                 JOIN watch_folders wf ON tf.watch_folder_id = wf.watch_id \
                 WHERE tf.relative_path = ?1 \
                   AND wf.tenant_id = ?2 \
                   AND wf.collection = ?3 \
                   AND NOT EXISTS ( \
                       SELECT 1 FROM json_each(tf.branches) je \
                       WHERE je.value IS NOT ?4) \
             )",
        )
        .bind(relative_file_path)
        .bind(tenant_id)
        .bind(collection)
        .bind(branch)
        .execute(&self.pool)
        .await?
        .rows_affected();

        if rows > 0 {
            debug!(
                "Removed {} leftover tracked_files row(s) of branch {:?} after Delete completed: {}",
                rows, branch, relative_file_path
            );
        }
        Ok(())
    }

    /// Best-effort: is `relative_file_path` still on disk in `branch`'s own
    /// checkout (the main folder for its HEAD, a linked worktree for a
    /// worktree branch; the main folder when no checkout has the branch)?
    ///
    /// Used by the F-036 orphan guard (#224): a Delete op that "completes" for
    /// a file still on disk was a stale/phantom no-op the handler PRESERVED,
    /// so its tracking row must NOT be removed. Returns `false` when the root
    /// can't be resolved (row already gone / real delete) so F-036 falls back
    /// to removal.
    pub(super) async fn delete_target_on_disk(
        &self,
        relative_file_path: &str,
        tenant_id: &str,
        collection: &str,
        branch: Option<&str>,
    ) -> bool {
        let root: Option<String> = sqlx::query_scalar(
            "SELECT wf.path \
             FROM watch_folders wf \
             JOIN tracked_files tf ON tf.watch_folder_id = wf.watch_id \
             WHERE tf.relative_path = ?1 \
               AND wf.tenant_id = ?2 \
               AND wf.collection = ?3 \
             LIMIT 1",
        )
        .bind(relative_file_path)
        .bind(tenant_id)
        .bind(collection)
        .fetch_optional(&self.pool)
        .await
        .ok()
        .flatten();
        let Some(root) = root else {
            return false;
        };
        let main_root = Path::new(&root);
        let checkouts = BranchCheckouts::discover_cached(main_root);
        branch
            .and_then(|b| checkouts.root_for(b))
            .unwrap_or(main_root)
            .join(relative_file_path)
            .exists()
    }
}
