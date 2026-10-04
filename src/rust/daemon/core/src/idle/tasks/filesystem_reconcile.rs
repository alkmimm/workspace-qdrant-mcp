//! Filesystem reconciliation — detect tracked files that no longer exist on
//! disk and enqueue delete operations to clean them up.
//!
//! "On disk" is per branch: a row's branch set names several checkouts (the
//! main folder for the main HEAD, a linked worktree for each worktree
//! branch), and its `relative_path` is main-anchored for all of them. Each
//! branch is judged in its OWN checkout; a branch nobody has checked out is
//! left to branch pruning. Judging every branch at the main root deleted
//! every file that existed only in a worktree (2026-10-03: 492 of 533
//! deletes in 27 h were such files).

use std::collections::HashMap;
use std::path::Path;

use async_trait::async_trait;
use sqlx::Row;
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};

use crate::git::{BranchCheckouts, CheckoutPresence};
use crate::idle::task::{MaintenanceContext, MaintenanceResult, MaintenanceTask};
use crate::idle::IdleState;
use crate::unified_queue_schema::{ItemType, QueueOperation};

/// Batch-checks tracked files against the filesystem.
///
/// Runs in `FullIdle` or `QdrantDownIdle` (only needs disk + SQLite).
/// Missing files get a delete operation enqueued so the normal pipeline
/// handles Qdrant cleanup when available.
pub struct FilesystemReconcileTask {
    batch_size: i64,
    offset: i64,
    total_checked: u64,
    files_missing: u64,
    branches_without_checkout: u64,
}

impl FilesystemReconcileTask {
    pub fn new() -> Self {
        Self {
            batch_size: 100,
            offset: 0,
            total_checked: 0,
            files_missing: 0,
            branches_without_checkout: 0,
        }
    }
}

/// The branches of one tracked row whose own checkout no longer has the file,
/// plus how many of its branches have no checkout to judge them by.
fn branches_missing_on_disk<'a>(
    checkouts: &BranchCheckouts,
    relative_path: &str,
    branches: &'a [String],
) -> (Vec<&'a str>, u64) {
    let mut missing = Vec::new();
    let mut unjudged = 0;
    for branch in branches {
        match checkouts.presence(branch, relative_path) {
            CheckoutPresence::Present => {}
            CheckoutPresence::Missing => missing.push(branch.as_str()),
            CheckoutPresence::NoCheckout => unjudged += 1,
        }
    }
    (missing, unjudged)
}

#[async_trait]
impl MaintenanceTask for FilesystemReconcileTask {
    fn name(&self) -> &str {
        "filesystem_reconcile"
    }

    fn required_idle_states(&self) -> &[IdleState] {
        &[IdleState::FullIdle, IdleState::QdrantDownIdle]
    }

    fn idle_delay_secs(&self) -> u64 {
        60
    }

    fn cooldown_secs(&self) -> u64 {
        1800 // 30 minutes
    }

    fn reset(&mut self) {
        self.offset = 0;
        self.total_checked = 0;
        self.files_missing = 0;
        self.branches_without_checkout = 0;
    }

    async fn run_batch(
        &mut self,
        ctx: &MaintenanceContext<'_>,
        cancel: &CancellationToken,
    ) -> MaintenanceResult {
        let rows = sqlx::query(
            "SELECT tf.file_id, tf.relative_path, tf.branches, tf.collection,
                    wf.tenant_id, wf.path AS watch_path
             FROM tracked_files tf
             JOIN watch_folders wf ON tf.watch_folder_id = wf.watch_id
             ORDER BY tf.file_id
             LIMIT ?1 OFFSET ?2",
        )
        .bind(self.batch_size)
        .bind(self.offset)
        .fetch_all(ctx.pool)
        .await;

        let rows = match rows {
            Ok(r) => r,
            Err(e) => {
                warn!("Filesystem reconcile query failed: {} — will retry", e);
                return MaintenanceResult::Yielded;
            }
        };

        if rows.is_empty() {
            self.log_completion();
            return MaintenanceResult::Done;
        }

        // One discovery per repository per batch: HEAD + the worktree admin
        // dirs, filesystem-only.
        let mut checkouts: HashMap<String, BranchCheckouts> = HashMap::new();

        for row in &rows {
            if cancel.is_cancelled() {
                return MaintenanceResult::Yielded;
            }
            self.total_checked += 1;
            self.reconcile_row(ctx, row, &mut checkouts).await;
        }

        self.offset += self.batch_size;
        MaintenanceResult::Continue
    }
}

impl FilesystemReconcileTask {
    fn log_completion(&self) {
        if self.files_missing > 0 {
            info!(
                "Filesystem reconcile complete: checked={}, missing={}, \
                 branch tags without a checkout (left to branch pruning)={}",
                self.total_checked, self.files_missing, self.branches_without_checkout
            );
        } else {
            debug!(
                "Filesystem reconcile complete: checked={}, all present, \
                 branch tags without a checkout={}",
                self.total_checked, self.branches_without_checkout
            );
        }
    }

    /// Judge one content-row: it carries the full branch set; each branch is
    /// judged in its own checkout and gets its own Delete, so the branch-set
    /// delete handler drops exactly the branches that lost the file (GC'ing the
    /// row and its shared point once the set empties).
    async fn reconcile_row(
        &mut self,
        ctx: &MaintenanceContext<'_>,
        row: &sqlx::sqlite::SqliteRow,
        checkouts: &mut HashMap<String, BranchCheckouts>,
    ) {
        let relative_path: &str = row.try_get("relative_path").unwrap_or("");
        let watch_path: &str = row.try_get("watch_path").unwrap_or("");
        if relative_path.is_empty() || watch_path.is_empty() {
            return;
        }
        let branches_json: String = row.try_get("branches").unwrap_or_default();
        let branches: Vec<String> = serde_json::from_str(&branches_json).unwrap_or_default();
        let repo = checkouts
            .entry(watch_path.to_string())
            .or_insert_with(|| BranchCheckouts::discover(Path::new(watch_path)));
        let (missing, unjudged) = branches_missing_on_disk(repo, relative_path, &branches);
        self.branches_without_checkout += unjudged;
        if missing.is_empty() {
            return;
        }

        self.files_missing += 1;
        let tenant_id: String = row.try_get("tenant_id").unwrap_or_default();
        let collection: String = row.try_get("collection").unwrap_or_default();
        // FilePayload.file_path is a validating RelativePath — ship the
        // watch-root-relative form straight from tracked_files. The checkout
        // join exists only for the on-disk check and the logs; enqueueing it
        // would fail the consumer's parse as a permanent InvalidPayload.
        let payload = build_missing_file_delete_payload(relative_path);
        for branch in missing {
            let abs_path = repo
                .root_for(branch)
                .unwrap_or(Path::new(watch_path))
                .join(relative_path);
            let abs_path_str = abs_path.to_string_lossy();
            match ctx
                .queue_manager
                .enqueue_unified(
                    ItemType::File,
                    QueueOperation::Delete,
                    &tenant_id,
                    &collection,
                    &payload,
                    Some(branch),
                    None,
                )
                .await
            {
                Err(e) => warn!(
                    "Failed to enqueue delete for missing file {} (branch {}): {}",
                    abs_path_str, branch, e
                ),
                Ok(_) => info!(
                    "Enqueued delete for missing file: {} (branch {})",
                    abs_path_str, branch
                ),
            }
        }
    }
}

/// Build the `file|delete` payload for a tracked file missing from disk.
///
/// The payload must round-trip through the consumer's validating
/// `FilePayload` deserialization, so it carries the watch-root-relative
/// path — never the absolute join used for the existence check.
fn build_missing_file_delete_payload(relative_path: &str) -> String {
    serde_json::json!({ "file_path": relative_path }).to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::unified_queue_schema::FilePayload;

    /// Regression: this task used to enqueue the ABSOLUTE joined path,
    /// which the consumer's validating `RelativePath` deserialization
    /// rejects as a permanent `InvalidPayload` (the same failure class as
    /// the ignore-reconciliation poison items).
    #[test]
    fn delete_payload_parses_as_file_payload_with_relative_path() {
        let payload = build_missing_file_delete_payload("src/lib/missing.rs");
        let parsed: FilePayload = serde_json::from_str(&payload).unwrap();
        assert_eq!(parsed.file_path.as_str(), "src/lib/missing.rs");
    }

    #[test]
    fn absolute_path_payload_is_rejected_by_consumer_parse() {
        // Documents WHY the payload must be relative: the absolute form
        // (what this task shipped before) cannot parse.
        let payload = build_missing_file_delete_payload("/root/proj/src/missing.rs");
        assert!(serde_json::from_str::<FilePayload>(&payload).is_err());
    }

    /// A main repo on `develop` with a linked worktree on `feat/x`; returns
    /// (main root, worktree root).
    fn repo_with_worktree(temp: &tempfile::TempDir) -> (std::path::PathBuf, std::path::PathBuf) {
        use std::fs;
        let main = temp.path().join("repo");
        let git = main.join(".git");
        fs::create_dir_all(&git).unwrap();
        fs::write(git.join("HEAD"), "ref: refs/heads/develop\n").unwrap();
        let wt = main.join(".claude/worktrees/wt-x");
        fs::create_dir_all(&wt).unwrap();
        fs::write(wt.join(".git"), "gitdir: repo/.git/worktrees/wt-x\n").unwrap();
        let admin = git.join("worktrees/wt-x");
        fs::create_dir_all(&admin).unwrap();
        fs::write(admin.join("gitdir"), format!("{}/.git\n", wt.display())).unwrap();
        fs::write(admin.join("HEAD"), "ref: refs/heads/feat/x\n").unwrap();
        (main, wt)
    }

    fn put(root: &std::path::Path, rel: &str) {
        let p = root.join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, "x\n").unwrap();
    }

    fn tags(names: &[&str]) -> Vec<String> {
        names.iter().map(|s| s.to_string()).collect()
    }

    /// Regression (emnify `app/lib/actions/sync.ts`, 2026-10-03): a file that
    /// exists only in the worktree, tagged only with the worktree's branch, is
    /// NOT missing — the main folder never had it.
    #[test]
    fn worktree_only_file_is_not_reported_missing() {
        let temp = tempfile::TempDir::new().unwrap();
        let (main, wt) = repo_with_worktree(&temp);
        put(&wt, "app/lib/actions/sync.ts");
        let co = BranchCheckouts::discover(&main);
        let branches = tags(&["feat/x"]);
        let (missing, unjudged) =
            branches_missing_on_disk(&co, "app/lib/actions/sync.ts", &branches);
        assert!(missing.is_empty(), "got {missing:?}");
        assert_eq!(unjudged, 0);
    }

    /// The branch whose own checkout lost the file is the ONLY one deleted:
    /// main still has it, the worktree branch removed it.
    #[test]
    fn only_the_branch_whose_checkout_lost_the_file_is_missing() {
        let temp = tempfile::TempDir::new().unwrap();
        let (main, _wt) = repo_with_worktree(&temp);
        put(&main, "app/lib/actions/endpoint.ts");
        let co = BranchCheckouts::discover(&main);
        let branches = tags(&["develop", "feat/x"]);
        let (missing, _) = branches_missing_on_disk(&co, "app/lib/actions/endpoint.ts", &branches);
        assert_eq!(missing, vec!["feat/x"]);
    }

    /// A tag for a branch nobody has checked out is never judged from disk —
    /// that is branch pruning's job — but it is counted.
    #[test]
    fn branch_without_checkout_is_counted_not_deleted() {
        let temp = tempfile::TempDir::new().unwrap();
        let (main, _wt) = repo_with_worktree(&temp);
        let co = BranchCheckouts::discover(&main);
        let branches = tags(&["old/merged"]);
        let (missing, unjudged) = branches_missing_on_disk(&co, "gone.ts", &branches);
        assert!(missing.is_empty());
        assert_eq!(unjudged, 1);
    }

    /// A file deleted from the main checkout is still reported for the main
    /// branch — the fix must not blind the reconcile.
    #[test]
    fn file_deleted_from_main_checkout_is_still_missing() {
        let temp = tempfile::TempDir::new().unwrap();
        let (main, wt) = repo_with_worktree(&temp);
        put(&wt, "shared.ts");
        let co = BranchCheckouts::discover(&main);
        let branches = tags(&["develop", "feat/x"]);
        let (missing, _) = branches_missing_on_disk(&co, "shared.ts", &branches);
        assert_eq!(missing, vec!["develop"]);
    }
}
