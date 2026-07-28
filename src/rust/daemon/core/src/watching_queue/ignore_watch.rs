//! Ignore file (.gitignore / .wqmignore) change detection and reconciliation.
//!
//! When the file watcher detects a Create or Modify event on .gitignore or
//! .wqmignore, this handler compares the file's mtime against the stored
//! value in `ignore_file_mtimes`, updates the stored mtime, and enqueues a
//! folder-level reconciliation scan for the project.

use std::path::Path;
use std::sync::Arc;

use tokio::sync::{Mutex, RwLock};
use tracing::{debug, info, warn};

use crate::queue_operations::QueueManager;

use super::file_watcher::FileWatcherQueue;
use super::types::WatchConfig;

impl FileWatcherQueue {
    /// Handle a .gitignore or .wqmignore change by comparing mtime and
    /// enqueueing a folder-level reconciliation scan if the file is newer.
    pub(super) async fn handle_ignore_file_change(
        ignore_path: &Path,
        config: &Arc<RwLock<WatchConfig>>,
        queue_manager: &Arc<QueueManager>,
        events_processed: &Arc<Mutex<u64>>,
    ) {
        let mtime_unix = match read_mtime_unix(ignore_path) {
            Some(t) => t,
            None => return,
        };

        let (tenant_id, project_root, collection) = {
            let c = config.read().await;
            (c.tenant_id.clone(), c.path.clone(), c.collection.clone())
        };

        let project_root_str = project_root.to_string_lossy().to_string();

        // Key the mtime slot by the ignore file's path relative to the project
        // root, never by its basename. A project holds one ignore file per
        // package plus one per checked-out git worktree (example-monorepo: 360 of them),
        // and `ignore_file_mtimes` is keyed (project_root, file_path) — so a
        // basename key collapses every one of them into a single `.gitignore`
        // row. Whichever file was touched last then owns the slot, and any
        // OTHER ignore file with a newer mtime looks like a change and re-fires
        // full reconciliation. Measured on the live stack 2026-07-16: 49
        // reconciles in 50 minutes against a tree whose `.wqmignore` had not
        // been modified in three weeks (#224).
        let ignore_key = ignore_path
            .strip_prefix(&project_root)
            .unwrap_or(ignore_path)
            .to_string_lossy()
            .to_string();

        if !should_reconcile(queue_manager, &project_root_str, &ignore_key, mtime_unix).await {
            return;
        }

        update_stored_mtime(queue_manager, &project_root_str, &ignore_key, mtime_unix).await;

        // #284: an ignore file that is ITSELF excluded from the eligibility walk
        // (anything under an ignored subtree — agent worktrees in
        // `.claude/worktrees/`, vendored trees, build output) cannot change the
        // walk's outcome, because the walk never descends into its directory.
        // Reconciling on its changes is a guaranteed no-op that still pays a
        // full project walk + diff + enqueue round. Measured live 2026-07-16:
        // ONE new agent worktree materialized 54 ignore files and fired 54
        // tenant-wide reconciles in 15 minutes. The gate here is the SAME
        // oracle `walk_eligible_files` post-filters with, so "can this file
        // affect the diff" and "does the walk read this file" cannot disagree.
        let global_ignore_path = resolve_global_ignore_path();
        if !ignore_file_can_affect_eligibility(
            &project_root,
            ignore_path,
            global_ignore_path.as_deref(),
        ) {
            debug!(
                "[ignore_watch] {} — '{}' is walk-excluded; skipping reconciliation \
                 (cannot affect eligibility)",
                tenant_id, ignore_key
            );
            return;
        }

        run_reconciliation(
            &project_root,
            &tenant_id,
            &collection,
            queue_manager,
            events_processed,
            global_ignore_path.as_deref(),
        )
        .await;
    }
}

/// Resolve `<data_dir>/global.wqmignore` at call time so admin-UI edits are
/// reflected on the next trigger.
fn resolve_global_ignore_path() -> Option<std::path::PathBuf> {
    wqm_common::paths::get_database_path()
        .ok()
        .and_then(|p| p.parent().map(|dir| dir.join("global.wqmignore")))
}

/// True when a change to `ignore_path` can alter the eligibility walk's
/// output — i.e. the file is not itself inside a walk-excluded subtree.
///
/// Uses the same `IgnoreGate` (project cascade + global.wqmignore,
/// `matched_path_or_any_parents`) that `walk_eligible_files` post-filters
/// with, so this predicate and the walk cannot disagree (#284).
pub(crate) fn ignore_file_can_affect_eligibility(
    project_root: &Path,
    ignore_path: &Path,
    global_ignore_path: Option<&Path>,
) -> bool {
    // The project root's own ignore files are the walk's INPUT — always
    // relevant, never subject to their own rules.
    if ignore_path.parent() == Some(project_root) {
        return true;
    }
    let gate = crate::patterns::ignore_gate::IgnoreGate::for_dir(
        project_root,
        Some(project_root),
        global_ignore_path,
    );
    // Ancestor-aware: this file is reached directly from a watcher event, not
    // through a pruning walk, so a `dir/`-style rule on an ancestor must count
    // (see `IgnoreGate::is_ignored_with_ancestors`).
    !gate.is_ignored_with_ancestors(project_root, ignore_path)
}

/// Read the file's mtime as a Unix timestamp, logging on failure.
fn read_mtime_unix(ignore_path: &Path) -> Option<i64> {
    match std::fs::metadata(ignore_path)
        .and_then(|m| m.modified())
        .map(|t| {
            t.duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs() as i64
        }) {
        Ok(t) => Some(t),
        Err(e) => {
            debug!(
                "Cannot read mtime for {}: {} — skipping reconciliation",
                ignore_path.display(),
                e
            );
            None
        }
    }
}

/// Check whether reconciliation is needed by comparing stored mtime.
async fn should_reconcile(
    queue_manager: &Arc<QueueManager>,
    project_root_str: &str,
    file_name: &str,
    mtime_unix: i64,
) -> bool {
    match crate::ignore_mtime::get_ignore_mtime(queue_manager.pool(), project_root_str, file_name)
        .await
    {
        Ok(Some(stored)) if stored >= mtime_unix => {
            debug!(
                "[ignore_watch] {} unchanged (stored={}, current={})",
                file_name, stored, mtime_unix
            );
            false
        }
        Ok(_) => true, // Newer or first time
        Err(e) => {
            warn!("[ignore_watch] mtime lookup failed: {e} — proceeding");
            true
        }
    }
}

/// Update the stored mtime for the ignore file.
async fn update_stored_mtime(
    queue_manager: &Arc<QueueManager>,
    project_root_str: &str,
    file_name: &str,
    mtime_unix: i64,
) {
    if let Err(e) = crate::ignore_mtime::set_ignore_mtime(
        queue_manager.pool(),
        project_root_str,
        file_name,
        mtime_unix,
    )
    .await
    {
        warn!("[ignore_watch] mtime update failed: {e}");
    }
}

/// Run reconciliation: diff tracked files vs eligible files,
/// enqueue stale deletions and missing additions.
async fn run_reconciliation(
    project_root: &Path,
    tenant_id: &str,
    collection: &str,
    queue_manager: &Arc<QueueManager>,
    events_processed: &Arc<Mutex<u64>>,
    global_ignore_path: Option<&Path>,
) {
    info!(
        "[ignore_watch] ignore file changed in {} — running reconciliation",
        tenant_id
    );

    // #280: resolve THIS folder's identity by the path being reconciled — a
    // tenant owns one enabled `projects` row per clone AND per registered git
    // worktree, so any tenant-scoped `LIMIT 1` can land on another folder and
    // diff one tree's rows against another tree's walk (observed live: 15
    // whole-index stale storms in one evening). Worktree folders own no
    // `tracked_files` rows — their content is served by the main folder via
    // branch tags (#167) — so reconciling them is at best a no-op and at worst
    // a mass-missing Uplift storm: skip them entirely.
    let watch_id = match crate::startup::reconciliation::ignore_sync::fetch_watch_folder_by_path(
        queue_manager.pool(),
        project_root,
    )
    .await
    {
        Ok(Some((watch_id, is_worktree))) => {
            if is_worktree {
                debug!(
                    "[ignore_watch] {} — '{}' is a registered worktree; skipping \
                     reconciliation (content is indexed via the main folder)",
                    tenant_id,
                    project_root.display()
                );
                return;
            }
            watch_id
        }
        Ok(None) => {
            warn!(
                "[ignore_watch] {} — no enabled watch folder matches '{}'; skipping \
                 reconciliation",
                tenant_id,
                project_root.display()
            );
            return;
        }
        Err(e) => {
            warn!("[ignore_watch] {} — watch folder lookup failed: {e}", tenant_id);
            return;
        }
    };

    match crate::startup::reconciliation::ignore_sync::reconcile_ignore_rules(
        project_root,
        &watch_id,
        tenant_id,
        collection,
        queue_manager.pool(),
        queue_manager,
        global_ignore_path,
    )
    .await
    {
        Ok(stats) => {
            let mut count = events_processed.lock().await;
            *count += 1;
            if stats.stale_deleted > 0 || stats.missing_added > 0 {
                info!(
                    "[ignore_watch] Reconciled {}: {} stale deleted, {} missing added",
                    tenant_id, stats.stale_deleted, stats.missing_added
                );
            } else {
                debug!("[ignore_watch] Reconciled {}: no changes needed", tenant_id);
            }
        }
        Err(e) => {
            warn!(
                "[ignore_watch] Reconciliation failed for {}: {}",
                tenant_id, e
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    /// #284: an ignore file inside a walk-excluded subtree cannot affect the
    /// eligibility diff — the trigger must be suppressed. Project-root ignore
    /// files are the walk's input and must always pass.
    #[test]
    fn walk_excluded_ignore_files_cannot_affect_eligibility() {
        let root = TempDir::new().unwrap();
        std::fs::write(root.path().join(".wqmignore"), "excluded/\n").unwrap();

        let excluded_dir = root.path().join("excluded");
        std::fs::create_dir_all(&excluded_dir).unwrap();
        std::fs::write(excluded_dir.join(".gitignore"), "*.tmp\n").unwrap();

        let included_dir = root.path().join("src");
        std::fs::create_dir_all(&included_dir).unwrap();
        std::fs::write(included_dir.join(".gitignore"), "*.o\n").unwrap();

        assert!(
            !ignore_file_can_affect_eligibility(
                root.path(),
                &excluded_dir.join(".gitignore"),
                None
            ),
            "ignore file under an excluded subtree must be suppressed"
        );
        assert!(
            ignore_file_can_affect_eligibility(root.path(), &included_dir.join(".gitignore"), None),
            "ignore file in an included subtree must trigger"
        );
        assert!(
            ignore_file_can_affect_eligibility(root.path(), &root.path().join(".wqmignore"), None),
            "the project root's own ignore files always trigger"
        );
        assert!(
            ignore_file_can_affect_eligibility(root.path(), &root.path().join(".gitignore"), None),
            "root .gitignore always triggers"
        );
    }
}
