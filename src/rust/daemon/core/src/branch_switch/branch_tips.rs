//! Branches nobody has checked out still move.
//!
//! The index follows a branch through the checkout that has it: the git watcher
//! for the main folder's HEAD, worktree discovery for a linked worktree. A local
//! branch with NO checkout — a trunk beside a main folder on another branch, a
//! branch whose worktree was removed — moves too (a pull in a worktree since
//! removed, `git fetch origin b:b`, a reset), and nothing followed it: its tags
//! kept pointing at the versions it held when it was last checked out. Measured
//! 2026-10-05 on bws-engineer: `master` advanced on 10-01 inside a worktree, and
//! 508 of its 1,358 tagged rows held content its tip no longer had — served as
//! master's code to every read of that branch, and to every branch whose view
//! fills in from master as the trunk.
//!
//! [`BranchTipFollower`] checks each such branch once per tip. Every path the
//! branch is tagged on is compared with the blob at its tip — per PATH: holding
//! any generation that matches is current — and, when the previous tip is
//! known, every path the move touched joins in. A path whose content moved is
//! re-ingested from the tip's bytes, staged on the daemon's data volume and read
//! through `read_root`, so the ordinary update path moves the tag from the old
//! generation to the new one (a dedup hit when another branch already holds that
//! content). A path the tip no longer has drops the tag through the branch
//! prune's per-branch delete.
//!
//! The trunk is held the same way wherever it is checked out but the main
//! folder — at its `git::followed_trunk` tip, `origin/<trunk>` when that is the
//! local trunk fast-forwarded. A worktree on it fell behind unnoticed: tecsul's
//! `release/platform` worktree sat 1,406 commits behind origin (2026-11-11)
//! while every agent branch came off origin's tip. The first time the follower
//! sees such a trunk it diffs from the local tip, so what origin added joins in.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use sqlx::SqlitePool;
use tracing::{debug, warn};
use wqm_common::hashing::compute_bytes_hash;

use crate::allowed_extensions::AllowedExtensions;
use crate::queue_operations::QueueManager;

use super::db;
use super::tip_apply::{apply_plan, Target};
use super::tip_stage::{stage_dir, sweep_stage};

/// What one branch's tip asks of the index.
#[derive(Debug, Default, PartialEq, Eq)]
pub(super) struct TipPlan {
    /// Paths to ingest from the tip, with its bytes: the branch's tagged
    /// content differs from the tip, or the move touched a path the branch is
    /// not tagged on yet.
    pub(super) reingest: Vec<(String, Vec<u8>)>,
    /// Paths the branch is tagged on that its tip no longer has.
    pub(super) untag: Vec<String>,
}

/// The work `tip` asks for. `tagged` maps each path the branch is tagged on to
/// the content hashes of the generations carrying the tag: the path is current
/// when ANY of them is the tip's content (more than one is shadowed debris — the
/// overlap strip's business, never a reason to re-ingest on every pass).
/// `moved_from` is the tip the branch was last checked at, when known; the paths
/// that move changed join the plan even where the branch holds no tag yet.
/// `None` when git cannot answer (the caller retries on the next pass).
pub(super) fn plan_branch(
    repo_root: &Path,
    tip: git2::Oid,
    moved_from: Option<git2::Oid>,
    tagged: &HashMap<String, Vec<String>>,
) -> Option<TipPlan> {
    let repo = git2::Repository::open(repo_root).ok()?;
    let tree = repo.find_commit(tip).ok()?.tree().ok()?;
    let prefix = crate::git::root_prefix(&repo, repo_root);
    let in_repo = |rel: &str| {
        if prefix.is_empty() {
            rel.to_string()
        } else {
            format!("{prefix}/{rel}")
        }
    };
    let at_tip = |rel: &str| -> Option<Vec<u8>> {
        let entry = tree.get_path(Path::new(&in_repo(rel))).ok()?;
        let blob = entry.to_object(&repo).ok()?.into_blob().ok()?;
        Some(blob.content().to_vec())
    };

    let mut plan = TipPlan::default();
    let mut paths: Vec<&String> = tagged.keys().collect();
    paths.sort();
    for rel in paths {
        match at_tip(rel) {
            None => plan.untag.push(rel.clone()),
            Some(bytes) => {
                if !tagged[rel].contains(&compute_bytes_hash(&bytes)) {
                    plan.reingest.push((rel.clone(), bytes));
                }
            }
        }
    }

    if let Some(from) = moved_from.filter(|from| *from != tip) {
        let old_tree = repo.find_commit(from).ok()?.tree().ok()?;
        let diff = repo
            .diff_tree_to_tree(Some(&old_tree), Some(&tree), None)
            .ok()?;
        let strip = format!("{prefix}/");
        let mut touched: Vec<String> = diff
            .deltas()
            .filter_map(|d| {
                d.new_file()
                    .path()
                    .map(|p| p.to_string_lossy().replace('\\', "/"))
            })
            .filter_map(|p| {
                if prefix.is_empty() {
                    Some(p)
                } else {
                    p.strip_prefix(&strip).map(str::to_string)
                }
            })
            .filter(|rel| !tagged.contains_key(rel))
            .collect();
        touched.sort();
        touched.dedup();
        // A path the move deleted has no blob at the tip and no tag: nothing to do.
        for rel in touched {
            if let Some(bytes) = at_tip(&rel) {
                plan.reingest.push((rel, bytes));
            }
        }
    }
    Some(plan)
}

/// Follows local branches nobody has checked out, and the trunk outside the
/// main folder, once per tip.
#[derive(Debug)]
pub struct BranchTipFollower {
    /// Where tip versions are staged for the queue to read (the daemon's data
    /// volume: it must outlive a restart while items are pending).
    stage_root: PathBuf,
    /// The tip each (watch folder, branch) was last checked at.
    tips: HashMap<(String, String), String>,
}

impl BranchTipFollower {
    pub fn new(stage_root: PathBuf) -> Self {
        Self {
            stage_root,
            tips: HashMap::new(),
        }
    }

    /// One pass over every enabled git project. Returns the items enqueued.
    pub async fn tick(
        &mut self,
        pool: &SqlitePool,
        queue_manager: &QueueManager,
        allowed_extensions: &AllowedExtensions,
    ) -> usize {
        let projects = match db::fetch_main_project_folders(pool).await {
            Ok(rows) => rows,
            Err(e) => {
                warn!("branch tips: {}", e);
                return 0;
            }
        };
        let io = Io {
            pool,
            queue_manager,
            allowed_extensions,
        };
        let mut total = 0usize;
        let mut live_projects = HashSet::new();
        let mut staged_now = HashSet::new();
        for (watch_id, root, tenant_id) in projects {
            live_projects.insert(watch_id.clone());
            total += self
                .follow_project(&io, &watch_id, &root, &tenant_id, &mut staged_now)
                .await;
        }
        self.tips
            .retain(|(watch_id, _), _| live_projects.contains(watch_id));
        sweep_stage(pool, &self.stage_root, &staged_now).await;
        total
    }

    /// Every branch of one project this follower holds: the ones nobody has
    /// checked out, and the trunk wherever it is checked out but the main folder.
    async fn follow_project(
        &mut self,
        io: &Io<'_>,
        watch_id: &str,
        root: &str,
        tenant_id: &str,
        staged_now: &mut HashSet<PathBuf>,
    ) -> usize {
        let Some(local) = local_branch_tips(Path::new(root)) else {
            return 0; // not a git repository the daemon can read
        };
        let checked_out = checked_out_branches(Path::new(root));
        let trunk = crate::git::followed_trunk(Path::new(root));
        let tagged = match db::fetch_tagged_branches(io.pool, watch_id).await {
            Ok(b) => b,
            Err(e) => {
                warn!("branch tips: {}", e);
                return 0;
            }
        };
        let project = Project {
            watch_id,
            tenant_id,
            root,
            live: local.keys().cloned().collect(),
        };
        let mut total = 0usize;
        for branch in tagged {
            match follow_target(&branch, trunk.as_ref(), &local, &checked_out) {
                Some(target) => {
                    total += self
                        .follow_branch(io, &project, &branch, target, staged_now)
                        .await
                }
                // Its checkout follows it, or git no longer has it (the branch
                // prune's business): check afresh if it comes back to us.
                None => {
                    self.tips.remove(&(watch_id.to_string(), branch));
                }
            }
        }
        total
    }

    /// Plan and apply one branch at `target`'s tip, once per tip.
    async fn follow_branch(
        &mut self,
        io: &Io<'_>,
        p: &Project<'_>,
        branch: &str,
        target: FollowTarget,
        staged_now: &mut HashSet<PathBuf>,
    ) -> usize {
        let key = (p.watch_id.to_string(), branch.to_string());
        let tip_str = target.tip.to_string();
        let previous = self.tips.get(&key).cloned();
        if previous.as_deref() == Some(tip_str.as_str()) {
            return 0;
        }
        let hashes = match db::fetch_branch_tagged_hashes(io.pool, p.watch_id, branch).await {
            Ok(h) => h,
            Err(e) => {
                warn!("branch tips: {}", e);
                return 0;
            }
        };
        let moved_from = previous
            .as_deref()
            .and_then(|seen| git2::Oid::from_str(seen).ok())
            .or(target.first_from);
        let repo_root = PathBuf::from(p.root);
        let tip = target.tip;
        let plan =
            tokio::task::spawn_blocking(move || plan_branch(&repo_root, tip, moved_from, &hashes))
                .await
                .ok()
                .flatten();
        let Some(plan) = plan else {
            debug!(
                "branch tips: git could not answer for '{}' at {}",
                branch, tip_str
            );
            return 0;
        };
        if plan.reingest.is_empty() && plan.untag.is_empty() {
            self.tips.insert(key, tip_str);
            return 0;
        }
        let t = Target {
            watch_id: p.watch_id,
            tenant_id: p.tenant_id,
            root: p.root,
            branch,
            tip: &tip_str,
        };
        let dir = stage_dir(&self.stage_root, p.tenant_id, branch, &tip_str);
        let applied = apply_plan(
            io.pool,
            io.queue_manager,
            io.allowed_extensions,
            &t,
            &dir,
            plan,
            &p.live,
        )
        .await;
        staged_now.insert(dir);
        // Not settled: the next pass plans this tip again (see `Applied`).
        if applied.settled {
            self.tips.insert(key, tip_str);
        }
        applied.enqueued
    }
}

/// What a pass reads and writes through.
struct Io<'a> {
    pool: &'a SqlitePool,
    queue_manager: &'a QueueManager,
    allowed_extensions: &'a AllowedExtensions,
}

/// One project in a pass.
struct Project<'a> {
    watch_id: &'a str,
    tenant_id: &'a str,
    root: &'a str,
    /// Its live local branches: an untag is covered only by one of these.
    live: HashSet<String>,
}

/// The tip the follower holds a branch at.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct FollowTarget {
    pub(super) tip: git2::Oid,
    /// Where to diff from when the follower has not seen the branch yet: the
    /// local trunk when origin is ahead of it, so what origin added joins in.
    pub(super) first_from: Option<git2::Oid>,
}

/// Whether this follower holds `branch`, and at which tip. The trunk is held
/// at its `git::followed_trunk` tip even where a linked worktree has it
/// checked out (worktree discovery leaves that worktree alone); any other
/// branch only while no checkout has it, at its local tip.
pub(super) fn follow_target(
    branch: &str,
    trunk: Option<&crate::git::FollowedTrunk>,
    local: &HashMap<String, git2::Oid>,
    checked_out: &HashSet<String>,
) -> Option<FollowTarget> {
    if let Some(trunk) = trunk.filter(|t| t.name == branch) {
        return Some(FollowTarget {
            tip: trunk.tip,
            first_from: trunk.from_origin().then_some(trunk.local),
        });
    }
    if checked_out.contains(branch) {
        return None;
    }
    local.get(branch).map(|tip| FollowTarget {
        tip: *tip,
        first_from: None,
    })
}

/// Each local branch's tip commit; `None` when the folder is not a repository.
fn local_branch_tips(root: &Path) -> Option<HashMap<String, git2::Oid>> {
    let repo = git2::Repository::open(root).ok()?;
    let branches = repo.branches(Some(git2::BranchType::Local)).ok()?;
    Some(
        branches
            .flatten()
            .filter_map(|(b, _)| {
                let name = b.name().ok()??.to_string();
                let tip = b.get().peel_to_commit().ok()?.id();
                Some((name, tip))
            })
            .collect(),
    )
}

/// The branches some checkout has: the main folder's HEAD and every linked
/// worktree's. Those are followed through their checkout.
fn checked_out_branches(root: &Path) -> HashSet<String> {
    super::worktree_queue_purge::checked_out_branches(
        root,
        &crate::git::list_linked_worktrees(root),
    )
}
