//! Drop queued worktree work once the worktree is gone.
//!
//! A worktree reconcile ([`super::worktree_membership`]) enqueues one
//! `File/Add` per file of the worktree, flagged `worktree_membership`: a
//! shared-baseline item read from the main tree, or a `read_root` item read
//! from the worktree itself. Agents create a worktree, work in it for minutes,
//! and remove it — keeping the branch. The items stay queued. When they run,
//! the `read_root` ones read a directory that no longer exists, and the
//! baseline ones tag a branch nobody has checked out with the main tree's bytes
//! — content the read surfaces already serve that branch from the trunk
//! fill-in (an unchanged path is read from the trunk's generation).
//!
//! The branch prune (`branch_prune_queue`) only drops work of branches git no
//! longer has. These branches still exist in git; only their checkout is gone.
//! Measured 2026-11-10: 195k of the 278k pending adds belonged to 64 tecsul
//! branches whose worktree had been removed (319 local branches, 22 worktrees),
//! ~4–6k items per worktree, ahead of every real change in the queue.
//!
//! A branch without a checkout is followed by [`super::BranchTipFollower`] from
//! its git tip; its staged items (`git_stage`) are never dropped here. Only
//! PENDING items created before the checkout set was read are eligible, and
//! the trunk names keep their work: their tags are what every other branch
//! fills in from.

use std::collections::{HashMap, HashSet};
use std::path::Path;

use sqlx::SqlitePool;
use tracing::{info, warn};
use wqm_common::constants::COLLECTION_PROJECTS;

use crate::git::LinkedWorktree;
use crate::startup::reconciliation::branch_prune_queue::queued_branches;

/// The branches some checkout of `main_root` has: the main folder's HEAD and
/// every linked worktree's (detached worktrees have none).
pub(super) fn checked_out_branches(
    main_root: &Path,
    worktrees: &[LinkedWorktree],
) -> HashSet<String> {
    worktrees
        .iter()
        .filter_map(|wt| wt.branch.clone())
        .chain(crate::git::head_branch(main_root))
        .collect()
}

/// Runs the purge for a project whenever its checkout set changes — a
/// worktree removed, or a branch switched away — and once on first sight
/// (the backlog left by worktrees removed while the daemon was down).
#[derive(Debug, Default)]
pub(super) struct CheckoutlessPurge {
    /// The checkout set each main watch folder was last purged against.
    last: HashMap<String, HashSet<String>>,
}

/// The project a purge pass runs over.
pub(super) struct PurgeTarget<'a> {
    pub(super) watch_id: &'a str,
    pub(super) tenant_id: &'a str,
    pub(super) root: &'a str,
}

impl CheckoutlessPurge {
    /// Purge `t`'s queue against `checkouts` unless that set was already
    /// purged against. `cutoff` must be taken BEFORE the worktrees behind
    /// `checkouts` were listed. Returns the items removed.
    pub(super) async fn run(
        &mut self,
        pool: &SqlitePool,
        t: &PurgeTarget<'_>,
        checkouts: HashSet<String>,
        cutoff: &str,
    ) -> u64 {
        if self.last.get(t.watch_id) == Some(&checkouts) {
            return 0;
        }
        let root = Path::new(t.root);
        match purge_checkoutless_worktree_work(pool, t.tenant_id, root, &checkouts, cutoff).await {
            Ok(n) => {
                self.last.insert(t.watch_id.to_string(), checkouts);
                n
            }
            Err(e) => {
                // Not recorded: the next pass tries again.
                warn!("worktree purge: {} — {}", t.tenant_id, e);
                0
            }
        }
    }

    /// Forget projects that are no longer watched.
    pub(super) fn retain(&mut self, live: &HashSet<String>) {
        self.last.retain(|watch_id, _| live.contains(watch_id));
    }
}

/// Branch names whose queued work is never dropped: the trunk names.
fn trunk_names(main_root: &Path) -> HashSet<String> {
    ["main", "master"]
        .into_iter()
        .map(str::to_string)
        .chain(crate::git::default_branch(main_root))
        .collect()
}

/// Remove pending worktree-reconcile adds of `tenant_id`'s branches that no
/// checkout has. Returns the number of queue items removed.
pub(super) async fn purge_checkoutless_worktree_work(
    pool: &SqlitePool,
    tenant_id: &str,
    main_root: &Path,
    checkouts: &HashSet<String>,
    cutoff: &str,
) -> Result<u64, String> {
    let trunk = trunk_names(main_root);
    let mut removed = 0u64;
    for branch in queued_branches(pool, tenant_id).await? {
        if checkouts.contains(&branch) || trunk.contains(&branch) {
            continue;
        }
        let n = purge_branch(pool, tenant_id, &branch, cutoff).await?;
        if n > 0 {
            info!(
                "worktree purge: {tenant_id} — branch '{branch}' has no checkout; dropped {n} \
                 pending worktree item(s) from the queue"
            );
        }
        removed += n;
    }
    Ok(removed)
}

/// Delete one branch's pending worktree-reconcile adds (one short statement).
/// `json_valid` guards `json_extract`, which fails on malformed metadata.
async fn purge_branch(
    pool: &SqlitePool,
    tenant_id: &str,
    branch: &str,
    cutoff: &str,
) -> Result<u64, String> {
    let sql = format!(
        "DELETE FROM unified_queue \
         WHERE tenant_id = ?1 AND branch = ?2 AND collection = '{COLLECTION_PROJECTS}' \
           AND item_type = 'file' AND file_path IS NOT NULL \
           AND op = 'add' AND status = 'pending' AND created_at < ?3 \
           AND CASE WHEN json_valid(metadata) \
                    THEN json_extract(metadata, '$.worktree_membership') = 1 \
                         AND json_extract(metadata, '$.git_stage') IS NULL \
                    ELSE 0 END"
    );
    let result = sqlx::query(&sql)
        .bind(tenant_id)
        .bind(branch)
        .bind(cutoff)
        .execute(pool)
        .await
        .map_err(|e| format!("purge queued worktree work for branch '{branch}': {e}"))?;
    Ok(result.rows_affected())
}
