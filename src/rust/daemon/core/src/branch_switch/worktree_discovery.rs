//! WHEN a linked worktree's branch membership is reconciled.
//!
//! [`super::worktree_membership`] knows HOW to tag a worktree branch (its three
//! disjoint candidate sets). This module decides WHEN. Until 2026-10-05 the only
//! trigger was a full tenant scan — daemon startup, project registration, a
//! `git reset` of the main tree, an admin rebuild — so a worktree created
//! between two of those (a `/batch` or agent worktree) stayed unindexed until the
//! next one. Measured on DOC-V2: three worktrees created during a 15 h window
//! with no scan were picked up only by the next daemon restart, ~5.1k files each
//! at once. [`WorktreeDiscovery`] watches every project's linked-worktree set on
//! a short interval and reconciles a worktree once it appears, switches branch,
//! or its branch tip moves — and only that worktree.

use std::collections::{HashMap, HashSet};
use std::path::Path;

use sqlx::SqlitePool;
use tracing::{debug, info, warn};

use wqm_common::constants::COLLECTION_PROJECTS;

use crate::allowed_extensions::AllowedExtensions;
use crate::git::{canonicalize_host_path, is_leaf_worktree_root, LinkedWorktree};
use crate::queue_operations::QueueManager;

use super::worktree_membership::{
    enqueue_worktree_divergent, enqueue_worktree_membership, enqueue_worktree_new_on_branch,
};

/// Reconcile branch membership for every linked worktree of a main repository.
///
/// Enumerates the main repo's linked worktrees and reconciles each one on a
/// concrete branch ([`reconcile_linked_worktree`]). Detached-HEAD worktrees and
/// trees the daemon cannot read are skipped. `projects`-only (the only
/// branch-scoped collection). Runs on every tenant scan, right after the main
/// branch's own `reconcile_branch_membership`. Returns the total files enqueued.
pub async fn reconcile_worktree_branches(
    pool: &SqlitePool,
    queue_manager: &QueueManager,
    main_watch_folder_id: &str,
    tenant_id: &str,
    collection: &str,
    main_project_root: &str,
    allowed_extensions: &AllowedExtensions,
) -> usize {
    if collection != COLLECTION_PROJECTS {
        return 0;
    }
    let mut total = 0usize;
    for wt in crate::git::list_linked_worktrees(Path::new(main_project_root)) {
        total += reconcile_linked_worktree(
            pool,
            queue_manager,
            main_watch_folder_id,
            tenant_id,
            collection,
            main_project_root,
            &wt,
            allowed_extensions,
        )
        .await;
    }
    total
}

/// Reconcile ONE linked worktree: enqueue its three disjoint candidate sets
/// under its branch, all flagged `worktree_membership` so the process-time
/// branch-restamp (#224) does NOT rewrite the worktree branch back to the main
/// HEAD (without the flag, every worktree file re-stamps to main and the tag
/// never lands):
///
/// 1. **Shared baseline** — paths the main folder tracks that also exist in the
///    worktree tree AND are byte-identical to main (not in the divergent set),
///    read from the SHARED main tree (storage stays keyed to the main
///    watch_folder, so cross-branch dedup merges identical content — no
///    duplicate point).
/// 2. **New-on-branch** — files with no `tracked_files` row under the main
///    folder (content that exists only on the worktree branch), read from the
///    worktree tree via a `read_root` on the item; storage is still keyed to
///    the main watch_folder.
/// 3. **Divergent** — shared files whose content DIFFERS on the branch (git
///    `Modified` between the main HEAD and the branch tip), read from the
///    worktree tree via `read_root` so the branch is indexed with its OWN bytes
///    (a new content-generation), not main's. The baseline skips these.
///
/// All three intersect the worktree's working tree, so a path deleted on the
/// worktree branch is never tagged. Returns the files enqueued (0 for a
/// detached HEAD or a tree the daemon cannot read).
#[allow(clippy::too_many_arguments)]
pub(super) async fn reconcile_linked_worktree(
    pool: &SqlitePool,
    queue_manager: &QueueManager,
    main_watch_folder_id: &str,
    tenant_id: &str,
    collection: &str,
    main_project_root: &str,
    wt: &LinkedWorktree,
    allowed_extensions: &AllowedExtensions,
) -> usize {
    let Some(branch) = wt.branch.as_deref() else {
        return 0; // detached HEAD: no branch to tag
    };

    // Fold the git-recorded path (possibly a `\\wsl.localhost\...` UNC form
    // from a Windows-host `git worktree add`) to the daemon's native view.
    // CATEGORY-B: used only for the process-local `is_dir()`/existence check;
    // never persisted (reads happen from the main tree, not this path).
    let wt_root = canonicalize_host_path(&wt.root.to_string_lossy());

    // Defensive: the main tree is reconciled by its own caller; skip it.
    if wt_root == main_project_root {
        return 0;
    }

    // The daemon must be able to read the worktree tree to check per-file
    // existence; if not (e.g. an unfolded UNC path, or a stale admin entry
    // for a removed worktree), there is nothing to reconcile.
    if !Path::new(&wt_root).is_dir() {
        debug!(
            "worktree membership: tree {} for branch '{}' not readable by the daemon; skipping",
            wt_root, branch
        );
        return 0;
    }

    // Guard: the root must be a genuine linked-worktree *checkout*, not a
    // stale/malformed admin entry whose `gitdir` resolved to a non-worktree
    // directory. Observed 2026-08-06: an admin entry whose gitdir parent was
    // the `.claude/worktrees` *container* (an ancestor of every sub-worktree)
    // passed both guards above — `is_dir()` is true and it is not the main
    // root — so the walk below enumerated EVERY file of EVERY sub-worktree,
    // enqueuing ~89k phantom "new-on-branch" items (relative paths prefixed
    // with the worktree name, which can never match the main tracked set)
    // under one wrong branch. A real linked worktree always has a `.git`
    // gitlink FILE (`gitdir: <main>/.git/worktrees/<name>`); the container
    // has no `.git`, and a main repo has `.git` as a directory — so
    // `is_leaf_worktree_root` rejects both.
    if !is_leaf_worktree_root(Path::new(&wt_root)) {
        warn!(
            "worktree membership: {} for branch '{}' is not a leaf worktree checkout \
             (no `.git` gitlink file); skipping to avoid a container-wide phantom walk",
            wt_root, branch
        );
        return 0;
    }

    // Committed divergence: paths git reports as Modified between the main
    // HEAD and this branch's tip. The baseline SKIPS these (reading them from
    // the main tree would tag the branch with main's bytes); (c) below reads
    // them from the worktree so the branch is indexed with its OWN content.
    let divergent = crate::git::modified_paths_head_vs_branch(Path::new(main_project_root), branch);

    // (a) Shared baseline: paths the main folder already tracks that also
    // exist in the worktree tree AND are byte-identical to main (not in
    // `divergent`), read from the MAIN tree (dedup merges — no new vectors).
    let n_baseline = enqueue_worktree_membership(
        pool,
        queue_manager,
        main_watch_folder_id,
        tenant_id,
        collection,
        &wt_root,
        branch,
        &divergent,
    )
    .await;
    if n_baseline > 0 {
        info!(
            "worktree membership: enqueued {} baseline file(s) under branch '{}' (worktree {})",
            n_baseline, branch, wt_root
        );
        crate::monitoring::metrics_core::METRICS
            .worktree_membership_enqueued_total
            .with_label_values(&[tenant_id, branch])
            .inc_by(n_baseline as u64);
    }

    // (b) New-on-branch: files that exist ONLY on the worktree branch (no
    // tracked_files row under the main folder). Their bytes live solely in
    // the worktree tree, so these items carry a `read_root` and the
    // processor reads from the worktree (storage still keyed to the main
    // folder). This is what the shared-baseline path cannot do — reading
    // from the main tree would resolve to a missing file.
    let n_new = enqueue_worktree_new_on_branch(
        pool,
        queue_manager,
        main_watch_folder_id,
        tenant_id,
        collection,
        &wt_root,
        main_project_root,
        branch,
        allowed_extensions,
    )
    .await;
    if n_new > 0 {
        info!(
            "worktree membership: enqueued {} new-on-branch file(s) under branch '{}' (worktree {})",
            n_new, branch, wt_root
        );
        crate::monitoring::metrics_core::METRICS
            .worktree_membership_new_on_branch_enqueued_total
            .with_label_values(&[tenant_id, branch])
            .inc_by(n_new as u64);
    }

    // (c) Divergent: shared files whose content DIFFERS on the worktree
    // branch (the `divergent` set), read from the worktree tree so the branch
    // is indexed with its own bytes instead of inheriting main's (baseline
    // inheritance / #151 auto-widen). Creates a new content-generation tagged
    // with the branch; the main generation (a different hash) is untouched.
    let n_div = enqueue_worktree_divergent(
        queue_manager,
        tenant_id,
        collection,
        &wt_root,
        main_project_root,
        branch,
        &divergent,
        allowed_extensions,
    )
    .await;
    if n_div > 0 {
        info!(
            "worktree membership: enqueued {} divergent file(s) under branch '{}' (worktree {})",
            n_div, branch, wt_root
        );
        crate::monitoring::metrics_core::METRICS
            .worktree_membership_divergent_enqueued_total
            .with_label_values(&[tenant_id, branch])
            .inc_by(n_div as u64);
    }

    n_baseline + n_new + n_div
}

/// A worktree's indexable state: where it is, which branch it has checked out,
/// and that branch's tip. A new state means new candidates — a new worktree, a
/// branch switch inside one, or commits that moved what diverges from main.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct WorktreeState {
    root: String,
    branch: String,
    tip: Option<String>,
}

/// What one project's worktrees looked like on the previous pass.
#[derive(Debug, Default)]
struct ProjectWorktrees {
    observed: HashSet<WorktreeState>,
    reconciled: HashSet<WorktreeState>,
}

impl ProjectWorktrees {
    /// The states to reconcile now, given this pass's observation.
    ///
    /// A state is due once it was seen on two consecutive passes — stable for a
    /// whole interval — and not yet reconciled. `git worktree add` registers the
    /// worktree before its checkout finishes; reconciling a half-written tree
    /// would tag only the files that happened to exist, and nothing would come
    /// back for the rest (the state does not change again).
    fn due(&mut self, current: HashSet<WorktreeState>) -> Vec<WorktreeState> {
        let due: Vec<WorktreeState> = current
            .iter()
            .filter(|s| self.observed.contains(*s) && !self.reconciled.contains(*s))
            .cloned()
            .collect();
        self.reconciled.retain(|s| current.contains(s));
        self.reconciled.extend(due.iter().cloned());
        self.observed = current;
        due
    }

    /// First sight of a project (daemon start, or a project just registered):
    /// the tenant scan that start or registration enqueues reconciles every
    /// worktree that exists now, so only what changes afterwards is due here.
    fn seeded(current: HashSet<WorktreeState>) -> Self {
        Self {
            reconciled: current.clone(),
            observed: current,
        }
    }
}

/// Reconciles a linked worktree as soon as it appears or moves, instead of at
/// the next tenant scan. Holds the last observation per main watch folder.
#[derive(Debug, Default)]
pub struct WorktreeDiscovery {
    projects: HashMap<String, ProjectWorktrees>,
}

impl WorktreeDiscovery {
    /// One pass over every enabled git project. Returns the files enqueued.
    pub async fn tick(
        &mut self,
        pool: &SqlitePool,
        queue_manager: &QueueManager,
        allowed_extensions: &AllowedExtensions,
    ) -> usize {
        let projects = match super::db::fetch_main_project_folders(pool).await {
            Ok(rows) => rows,
            Err(e) => {
                warn!("worktree discovery: {}", e);
                return 0;
            }
        };

        let mut total = 0usize;
        let mut live = HashSet::new();
        for (watch_id, root, tenant_id) in projects {
            live.insert(watch_id.clone());
            let worktrees = crate::git::list_linked_worktrees(Path::new(&root));
            let states = observe(&root, &worktrees);
            let current: HashSet<WorktreeState> = states.iter().map(|(s, _)| s.clone()).collect();
            let due = match self.projects.get_mut(&watch_id) {
                Some(project) => project.due(current),
                None => {
                    self.projects
                        .insert(watch_id.clone(), ProjectWorktrees::seeded(current));
                    continue;
                }
            };
            for (state, wt) in states.iter().filter(|(s, _)| due.contains(s)) {
                info!(
                    "worktree discovery: '{}' at {} is new or moved (tip {}); reconciling",
                    state.branch,
                    state.root,
                    state.tip.as_deref().unwrap_or("unknown")
                );
                total += reconcile_linked_worktree(
                    pool,
                    queue_manager,
                    &watch_id,
                    &tenant_id,
                    COLLECTION_PROJECTS,
                    &root,
                    wt,
                    allowed_extensions,
                )
                .await;
            }
        }
        self.projects.retain(|watch_id, _| live.contains(watch_id));
        total
    }
}

/// Each branch-carrying worktree's state, paired with the worktree itself. A
/// detached HEAD has no branch to tag and is left out.
fn observe(main_root: &str, worktrees: &[LinkedWorktree]) -> Vec<(WorktreeState, LinkedWorktree)> {
    let repo = git2::Repository::open(main_root).ok();
    worktrees
        .iter()
        .filter_map(|wt| {
            let branch = wt.branch.clone()?;
            let tip = repo
                .as_ref()
                .and_then(|r| r.refname_to_id(&format!("refs/heads/{branch}")).ok())
                .map(|oid| oid.to_string());
            let state = WorktreeState {
                // CATEGORY-B: identity of the observation only, never persisted.
                root: canonicalize_host_path(&wt.root.to_string_lossy()),
                branch,
                tip,
            };
            Some((state, wt.clone()))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn state(root: &str, branch: &str, tip: &str) -> WorktreeState {
        WorktreeState {
            root: root.into(),
            branch: branch.into(),
            tip: Some(tip.into()),
        }
    }

    fn set(states: &[&WorktreeState]) -> HashSet<WorktreeState> {
        states.iter().map(|s| (*s).clone()).collect()
    }

    #[test]
    fn a_project_seen_for_the_first_time_owes_nothing() {
        // Its start-up / registration tenant scan reconciles what exists now.
        let a = state("/r/.wt/a", "feat/a", "1");
        let mut p = ProjectWorktrees::seeded(set(&[&a]));
        assert!(p.due(set(&[&a])).is_empty());
    }

    #[test]
    fn a_new_worktree_is_due_once_stable_and_only_once() {
        let a = state("/r/.wt/a", "feat/a", "1");
        let b = state("/r/.wt/b", "feat/b", "1");
        let mut p = ProjectWorktrees::seeded(set(&[&a]));
        // Appears mid-checkout: not yet.
        assert!(p.due(set(&[&a, &b])).is_empty());
        // Stable for a whole interval: due — and nothing else is.
        assert_eq!(p.due(set(&[&a, &b])), vec![b.clone()]);
        // Reconciled: never again while it stays the same.
        assert!(p.due(set(&[&a, &b])).is_empty());
    }

    #[test]
    fn a_moved_tip_or_switched_branch_is_due_again() {
        let a1 = state("/r/.wt/a", "feat/a", "1");
        let a2 = state("/r/.wt/a", "feat/a", "2");
        let a_other = state("/r/.wt/a", "feat/other", "2");
        let mut p = ProjectWorktrees::seeded(set(&[&a1]));
        assert!(p.due(set(&[&a2])).is_empty());
        assert_eq!(
            p.due(set(&[&a2])),
            vec![a2.clone()],
            "a commit on the branch"
        );
        assert!(p.due(set(&[&a_other])).is_empty());
        assert_eq!(
            p.due(set(&[&a_other])),
            vec![a_other.clone()],
            "a branch switch"
        );
    }

    #[test]
    fn a_removed_then_recreated_worktree_is_due_again() {
        let a = state("/r/.wt/a", "feat/a", "1");
        let mut p = ProjectWorktrees::seeded(set(&[&a]));
        assert!(p.due(HashSet::new()).is_empty());
        assert!(p.due(set(&[&a])).is_empty());
        assert_eq!(p.due(set(&[&a])), vec![a.clone()]);
    }

    #[test]
    fn a_flickering_state_is_never_due() {
        // Seen, gone, seen: never stable across two passes (e.g. HEAD moving
        // during a rebase) — nothing is reconciled against a moving target.
        let a = state("/r/.wt/a", "feat/a", "1");
        let b = state("/r/.wt/a", "feat/a", "2");
        let mut p = ProjectWorktrees::seeded(HashSet::new());
        assert!(p.due(set(&[&a])).is_empty());
        assert!(p.due(set(&[&b])).is_empty());
        assert!(p.due(set(&[&a])).is_empty());
    }
}
