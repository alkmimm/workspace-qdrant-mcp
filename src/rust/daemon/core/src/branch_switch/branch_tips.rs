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

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use sqlx::SqlitePool;
use tracing::{debug, info, warn};
use wqm_common::constants::COLLECTION_PROJECTS;
use wqm_common::hashing::compute_bytes_hash;
use wqm_common::paths::RelativePath;

use crate::allowed_extensions::AllowedExtensions;
use crate::queue_operations::QueueManager;
use crate::startup::reconciliation::branch_prune::{
    build_prune_delete_payload, BRANCH_PRUNE_COVERED_DELETE_METADATA, BRANCH_PRUNE_DELETE_METADATA,
};
use crate::unified_queue_schema::{FilePayload, ItemType, QueueOperation};

use super::db;
use super::tip_stage::{stage_dir, stage_file, sweep_stage};
use super::worktree_membership::{main_eligibility_gate, worktree_path_eligible};

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

/// Follows local branches nobody has checked out, once per tip.
#[derive(Debug)]
pub struct BranchTipFollower {
    /// Where tip versions are staged for the queue to read (the daemon's data
    /// volume: it must outlive a restart while items are pending).
    stage_root: PathBuf,
    /// The tip each (watch folder, branch) was last checked at.
    tips: HashMap<(String, String), String>,
}

/// The branch a plan is applied to.
struct Target<'a> {
    watch_id: &'a str,
    tenant_id: &'a str,
    root: &'a str,
    branch: &'a str,
    tip: &'a str,
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
        let mut total = 0usize;
        let mut live_projects = HashSet::new();
        let mut staged_now = HashSet::new();
        for (watch_id, root, tenant_id) in projects {
            live_projects.insert(watch_id.clone());
            let Some(local) = local_branch_tips(Path::new(&root)) else {
                continue; // not a git repository the daemon can read
            };
            let live: HashSet<String> = local.keys().cloned().collect();
            let checked_out = checked_out_branches(Path::new(&root));
            let tagged = match db::fetch_tagged_branches(pool, &watch_id).await {
                Ok(b) => b,
                Err(e) => {
                    warn!("branch tips: {}", e);
                    continue;
                }
            };
            for branch in tagged {
                let key = (watch_id.clone(), branch.clone());
                if checked_out.contains(&branch) {
                    // Its checkout follows it; check afresh once it has none.
                    self.tips.remove(&key);
                    continue;
                }
                // A branch git no longer has is the branch prune's business.
                let Some(tip) = local.get(&branch).copied() else {
                    continue;
                };
                let tip_str = tip.to_string();
                let previous = self.tips.get(&key).cloned();
                if previous.as_deref() == Some(tip_str.as_str()) {
                    continue;
                }
                let hashes = match db::fetch_branch_tagged_hashes(pool, &watch_id, &branch).await {
                    Ok(h) => h,
                    Err(e) => {
                        warn!("branch tips: {}", e);
                        continue;
                    }
                };
                let moved_from = previous
                    .as_deref()
                    .and_then(|p| git2::Oid::from_str(p).ok());
                let repo_root = PathBuf::from(&root);
                let plan = tokio::task::spawn_blocking(move || {
                    plan_branch(&repo_root, tip, moved_from, &hashes)
                })
                .await
                .ok()
                .flatten();
                let Some(plan) = plan else {
                    debug!(
                        "branch tips: git could not answer for '{}' at {}",
                        branch, tip_str
                    );
                    continue;
                };
                if plan.reingest.is_empty() && plan.untag.is_empty() {
                    self.tips.insert(key, tip_str);
                    continue;
                }
                let target = Target {
                    watch_id: &watch_id,
                    tenant_id: &tenant_id,
                    root: &root,
                    branch: &branch,
                    tip: &tip_str,
                };
                let dir = stage_dir(&self.stage_root, &tenant_id, &branch, &tip_str);
                let applied = apply_plan(
                    pool,
                    queue_manager,
                    allowed_extensions,
                    &target,
                    &dir,
                    plan,
                    &live,
                )
                .await;
                total += applied.enqueued;
                staged_now.insert(dir);
                // Not settled: the next pass plans this tip again (see `Applied`).
                if applied.settled {
                    self.tips.insert(key, tip_str);
                }
            }
        }
        self.tips
            .retain(|(watch_id, _), _| live_projects.contains(watch_id));
        sweep_stage(pool, &self.stage_root, &staged_now).await;
        total
    }
}

/// What applying a plan did.
struct Applied {
    enqueued: usize,
    /// Every re-ingest entered the queue. A pending item for the same
    /// `(branch, path)` from an OLDER tip makes the queue ignore the newer one
    /// (`INSERT OR IGNORE` on the per-file unique index) — the older bytes would
    /// land and the branch would rest on an intermediate version. Such a tip is
    /// left unsettled and planned again on the next pass, until the older item
    /// has run and the newer one can enter.
    settled: bool,
}

/// Stage and enqueue one branch's plan.
async fn apply_plan(
    pool: &SqlitePool,
    queue_manager: &QueueManager,
    allowed_extensions: &AllowedExtensions,
    t: &Target<'_>,
    dir: &Path,
    plan: TipPlan,
    live: &HashSet<String>,
) -> Applied {
    let gate = main_eligibility_gate(t.root);
    let (mut reingested, mut untagged, mut settled) = (0usize, 0usize, true);
    for (rel, bytes) in plan.reingest {
        // A path the move added must pass the main scan's eligibility; a tagged
        // one already did when it was indexed.
        if !worktree_path_eligible(t.root, &gate, allowed_extensions, COLLECTION_PROJECTS, &rel) {
            continue;
        }
        if !stage_file(dir, &rel, &bytes) {
            warn!("branch tips: could not stage {} of '{}'", rel, t.branch);
            settled = false;
            continue;
        }
        match enqueue_reingest(queue_manager, t, &rel, dir).await {
            Ok(true) => reingested += 1,
            Ok(false) => settled = false,
            Err(e) => {
                warn!(
                    "branch tips: enqueue {} on '{}' failed: {}",
                    rel, t.branch, e
                );
                settled = false;
            }
        }
    }
    for rel in plan.untag {
        // An untag already pending is the same untag: nothing to retry.
        match enqueue_untag(pool, queue_manager, t, &rel, live).await {
            Ok(true) => untagged += 1,
            Ok(false) => {}
            Err(e) => {
                warn!("branch tips: untag {} on '{}' failed: {}", rel, t.branch, e);
                settled = false;
            }
        }
    }
    if reingested + untagged > 0 {
        info!(
            "branch tips: '{}' ({}) is at {} with no checkout; {} file(s) re-ingested from git, {} untagged",
            t.branch, t.tenant_id, t.tip, reingested, untagged
        );
    }
    Applied {
        enqueued: reingested + untagged,
        settled,
    }
}

/// `File/Add` of `rel` under the branch, reading the staged tip bytes. The tip
/// rides in the payload so the idempotency key differs per tip: an item still
/// pending for an older tip never swallows the newer one.
async fn enqueue_reingest(
    queue_manager: &QueueManager,
    t: &Target<'_>,
    rel: &str,
    dir: &Path,
) -> Result<bool, String> {
    let rel_path = RelativePath::from_user_input(rel).map_err(|e| e.to_string())?;
    let mut payload = serde_json::to_value(FilePayload {
        file_path: rel_path,
        file_type: None,
        file_hash: None,
        size_bytes: None,
        old_path: None,
    })
    .map_err(|e| e.to_string())?;
    payload["branch_tip"] = serde_json::Value::String(t.tip.to_string());
    // `worktree_membership` keeps the item's branch (no restamp to the main
    // HEAD) and scopes the idempotency key to the branch; `git_stage` keeps the
    // language server out of bytes it cannot see.
    let metadata = serde_json::json!({
        "worktree_membership": true,
        "read_root": dir.to_string_lossy(),
        "git_stage": true,
    })
    .to_string();
    queue_manager
        .enqueue_unified(
            ItemType::File,
            QueueOperation::Add,
            t.tenant_id,
            COLLECTION_PROJECTS,
            &payload.to_string(),
            Some(t.branch),
            Some(metadata.as_str()),
        )
        .await
        .map(|(_, new)| new)
        .map_err(|e| e.to_string())
}

/// Drop the branch's tag from `rel` — the branch prune's per-branch delete,
/// marked covered when another live generation keeps the path indexed.
async fn enqueue_untag(
    pool: &SqlitePool,
    queue_manager: &QueueManager,
    t: &Target<'_>,
    rel: &str,
    live: &HashSet<String>,
) -> Result<bool, String> {
    let rel_path = RelativePath::from_user_input(rel).map_err(|e| e.to_string())?;
    let payload_json = build_prune_delete_payload(&rel_path, t.branch)?;
    let covered = db::path_served_without_branch(pool, t.watch_id, rel, t.branch, live).await?;
    let metadata = if covered {
        BRANCH_PRUNE_COVERED_DELETE_METADATA
    } else {
        BRANCH_PRUNE_DELETE_METADATA
    };
    queue_manager
        .enqueue_unified(
            ItemType::File,
            QueueOperation::Delete,
            t.tenant_id,
            COLLECTION_PROJECTS,
            &payload_json,
            Some(t.branch),
            Some(metadata),
        )
        .await
        .map(|(_, new)| new)
        .map_err(|e| e.to_string())
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
    crate::git::list_linked_worktrees(root)
        .into_iter()
        .filter_map(|wt| wt.branch)
        .chain(crate::git::head_branch(root))
        .collect()
}
