//! Prune index documents for git branches that no longer exist.
//!
//! Branch-deletion → document purge is otherwise unwired in this deployment:
//! the file watcher excludes `.git/` (so it never observes a ref deletion), and
//! the `git::branch_lifecycle::BranchEventHandler` pipeline is test-only
//! scaffolding with no runtime consumer. Without this reconciler, every deleted
//! branch leaves its indexed documents orphaned forever — stale-branch
//! accumulation.
//!
//! This closes the gap by comparing the branches present in `tracked_files`
//! against the repository's live local branches and enqueuing `file|delete` for
//! every tracked file on a branch that no longer exists in git. It runs from the
//! same admin/startup reconciliation entry point as the ignore reconciler.
//!
//! ## Branch isolation is safe
//!
//! The file-delete processor reference-counts Qdrant points by `base_point`
//! (see `strategies/processing/file/delete.rs::delete_tracked_file`, which
//! ref-counts via `QueueManager::has_other_references`): deleting a branch's
//! `tracked_files` rows only removes Qdrant points that no other branch
//! (e.g. `main`) still references. A shared point survives until its last
//! referencing branch is gone.
//!
//! ## Safety guards (never over-prune)
//!
//! Branch labels in `tracked_files` are NOT always real branch names: at index
//! time `get_current_branch` falls back to `"main"` when a repo can't be read,
//! so a project's whole corpus can end up labeled under a non-existent branch.
//! Treating branch-absence alone as "safe to delete" once wiped two projects
//! whose content was mislabeled under a bogus `"main"`. The guards below make
//! over-pruning structurally impossible — a lingering stale branch is benign,
//! deleting a real index is not:
//!
//! 1. Path missing, repo unopenable, or zero live branches → skip the project.
//! 2. HEAD branch not in the live set → labels untrustworthy → skip the project.
//! 3. Never prune the project's largest tracked branch (its corpus).
//! 4. Never prune a branch named `main` or `master` (default-name safety net).
//!
//! Only a branch that is absent from git AND passes all guards is pruned.
//!
//! ## Live-generation coverage (issue #224 stage 3)
//!
//! Guard 5 lives on the DELETE side (`file/delete.rs`): when the pruned branch
//! is a row's ONLY tag and the file is still on disk, the delete is preserved
//! ("mislabeled, not deleted"). That guard was written for a mislabeled CORPUS
//! (guards 1-4's failure mode) but misfires on the Layer-2 multi-generation
//! model, where one path legitimately has several content rows (one per distinct
//! cross-branch content) and the on-disk file is served by the generation tagged
//! with a LIVE branch. Measured 2026-07-15: 1,034 preserves per startup, ~1.7k
//! stale generations kept alive forever while their deletes were re-enqueued
//! every boot.
//!
//! This module computes the missing fact — is this path served by ANOTHER
//! generation carrying a live branch? — while the live set is in hand, and
//! stamps it on the delete (`covered_by_live_generation`). The delete side then
//! preserves ONLY the genuinely-uncovered case (the corpus this guard exists to
//! protect). Deletion of covered generations is gated by
//! `WQM_BRANCH_PRUNE_COVERED_DELETE` (default `dry` — observe, mutate nothing).
//!
//! ## Delete-side contract (this module only enqueues; `file/delete.rs` executes)
//!
//! Each pruned file is enqueued as `file|delete` stamped with
//! [`BRANCH_PRUNE_DELETE_METADATA`] so `process_file_delete` bypasses its
//! "skip a stale delete for a file still on disk" guard — a pruned branch's
//! files usually still exist on the live branch, and without the bypass the
//! delete is skipped, the tag never clears, and every startup re-enqueues it.
//! The delete is reference-counted: it drops the dead branch's tag and removes
//! the shared Qdrant point only when no live branch references it. As a final
//! backstop, `delete_tracked_file` PRESERVES the entry (no deletion) when
//! dropping the branch would empty the set AND the file is still on disk — a
//! present-but-mislabeled file, left for reconciliation to re-tag.
//! See `docs/specs/21-cross-branch-dedup.md` §C.

use std::collections::{HashMap, HashSet};

use sqlx::SqlitePool;
use tracing::{debug, info, warn};

use crate::git::BranchLifecycleDetector;
use crate::queue_operations::QueueManager;
use crate::unified_queue_schema::{FilePayload, ItemType, QueueOperation};
use crate::watching_queue::WatchManager;
use wqm_common::paths::RelativePath;

/// The `reason` token carried in the `metadata` of every `file|delete` enqueued
/// by branch pruning. The file-delete processor matches this exact value.
pub(crate) const BRANCH_PRUNE_REASON: &str = "branch_prune";

/// `metadata` JSON stamped on every `file|delete` enqueued by branch pruning.
///
/// The file-delete processor (`strategies::processing::file::delete`) keys on
/// this to bypass its "file still exists on disk" skip. A pruned branch's tag
/// must be removed even though the file is still present on the live branch:
/// content is shared across branches via `base_point` reference counting, so
/// the shared Qdrant point survives — only the dead branch's tag is dropped.
/// Without the marker these deletes are skipped as "stale", the orphaned branch
/// tags never clear, and every startup re-enqueues the same no-op deletes
/// (defeating the whole purpose of this reconciler and clogging the queue).
pub(crate) const BRANCH_PRUNE_DELETE_METADATA: &str = r#"{"reason":"branch_prune"}"#;

/// `metadata` for a prune delete whose path IS served by another generation
/// carrying a live branch (issue #224 stage 3). Adds
/// `covered_by_live_generation` to the marker above so the delete side can tell
/// a stale Layer-2 generation (safe to remove — the on-disk file stays indexed
/// via the live generation) from a mislabeled corpus (must be preserved).
///
/// Computed here, at enqueue time, because this is where the git live set is
/// available; the queue processor has no repo access. The flag stays valid
/// until processing: a covering generation carries a live tag, so branch
/// pruning never enqueues a delete for it.
pub(crate) const BRANCH_PRUNE_COVERED_DELETE_METADATA: &str =
    r#"{"reason":"branch_prune","covered_by_live_generation":true}"#;

/// Totals returned by [`prune_orphaned_branches`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct BranchPruneStats {
    /// Number of orphaned branches for which deletes were enqueued.
    pub branches_pruned: u64,
    /// Number of `file|delete` items enqueued across all pruned branches.
    pub files_enqueued: u64,
    /// Of `files_enqueued`, how many are stale Layer-2 generations whose path is
    /// still served by another generation carrying a live branch (issue #224
    /// stage 3). These are the deletes the on-disk preserve guard has been
    /// no-op'ing forever; they only execute under
    /// `WQM_BRANCH_PRUNE_COVERED_DELETE=on`.
    pub files_covered: u64,
    /// Covered candidates NOT enqueued because the per-run cap was reached
    /// (`WQM_BRANCH_PRUNE_COVERED_CAP`). They are picked up by the next cycle.
    pub files_deferred: u64,
}

impl BranchPruneStats {
    /// Returns true if any branch was pruned.
    pub fn has_changes(&self) -> bool {
        self.branches_pruned > 0 || self.files_enqueued > 0
    }
}

/// Per-run ceiling on COVERED deletes (issue #224 stage 3).
///
/// Bounds the blast radius of one prune cycle: if the coverage computation is
/// ever wrong, at most this many stale generations can go before a human sees
/// the counts. Covered candidates beyond the cap are simply not enqueued — the
/// prune runs at every startup, so they are picked up by the next cycle.
/// Uncovered deletes and multi-tag shrinks are untouched (pre-existing
/// behaviour, not deletion-capable).
///
/// `WQM_BRANCH_PRUNE_COVERED_CAP`: a number (`0` = unlimited); default 500.
#[derive(Debug, Clone, Copy)]
pub(crate) struct CoveredBudget {
    /// Remaining covered enqueues this run; `None` = unlimited.
    remaining: Option<u64>,
    /// Covered candidates skipped because the cap was reached.
    deferred: u64,
}

impl CoveredBudget {
    /// Build from the env knob. Unparsable → the default cap (never unlimited:
    /// a typo must not lift the ceiling).
    pub(crate) fn from_env() -> Self {
        const DEFAULT_CAP: u64 = 500;
        let cap = std::env::var("WQM_BRANCH_PRUNE_COVERED_CAP")
            .ok()
            .and_then(|v| v.trim().parse::<u64>().ok())
            .unwrap_or(DEFAULT_CAP);
        Self::with_cap(cap)
    }

    /// `cap == 0` means unlimited.
    pub(crate) fn with_cap(cap: u64) -> Self {
        Self {
            remaining: (cap > 0).then_some(cap),
            deferred: 0,
        }
    }

    /// Claim one covered enqueue. `false` → the cap is exhausted; the caller
    /// must skip this candidate (counted as deferred).
    pub(crate) fn take(&mut self) -> bool {
        match self.remaining.as_mut() {
            None => true, // unlimited
            Some(0) => {
                self.deferred += 1;
                false
            }
            Some(n) => {
                *n -= 1;
                true
            }
        }
    }

    /// Covered candidates skipped because the cap was reached this run.
    pub(crate) fn deferred(&self) -> u64 {
        self.deferred
    }
}

/// Does another generation of this path carry a live branch?
///
/// `live_generations` are the `file_id`s whose branch set intersects the repo's
/// live branches, for the path being pruned; `this_file_id` is the generation
/// the delete targets. Pure — the whole stage-3 decision in one predicate.
///
/// `true`  → the on-disk file stays indexed via that other generation, so this
///           stale one can go.
/// `false` → nothing else serves the path: preserving is correct (the
///           mislabeled-corpus case guards 1-4 exist for).
pub(crate) fn covered_by_other_live_generation(
    live_generations: Option<&Vec<i64>>,
    this_file_id: i64,
) -> bool {
    live_generations.is_some_and(|gens| gens.iter().any(|&f| f != this_file_id))
}

/// Prune orphaned-branch documents for all active projects.
///
/// Iterates `watch_folders WHERE collection = 'projects' AND enabled = 1`, and
/// for each project enqueues `file|delete` for the tracked files of any branch
/// that no longer exists in the repository's local refs. Per-project failures
/// are logged and skipped — one bad repo never aborts the sweep.
pub async fn prune_orphaned_branches(
    pool: &SqlitePool,
    queue_manager: &std::sync::Arc<QueueManager>,
) -> Result<BranchPruneStats, String> {
    let rows: Vec<(String, String, String)> = sqlx::query_as(
        "SELECT watch_id, tenant_id, path FROM watch_folders \
         WHERE collection = 'projects' AND enabled = 1",
    )
    .fetch_all(pool)
    .await
    .map_err(|e| format!("query active projects: {e}"))?;

    // One budget for the whole run: the cap bounds a cycle, not a project.
    let mut budget = CoveredBudget::from_env();

    let mut totals = BranchPruneStats::default();
    for (watch_id, tenant_id, project_root) in &rows {
        match prune_project_branches(
            pool,
            queue_manager,
            watch_id,
            tenant_id,
            project_root,
            &mut budget,
        )
        .await
        {
            Ok(stats) => {
                totals.branches_pruned += stats.branches_pruned;
                totals.files_enqueued += stats.files_enqueued;
                totals.files_covered += stats.files_covered;
            }
            Err(e) => warn!("[branch_prune] reconciliation failed for {tenant_id}: {e}"),
        }
    }
    totals.files_deferred = budget.deferred();

    if totals.has_changes() {
        info!(
            "[branch_prune] Pruned {} orphaned branch(es), enqueued {} file delete(s) \
             ({} covered by a live generation — stage 3 candidates, policy={}, capped={}, \
             deferred_to_next_cycle={})",
            totals.branches_pruned,
            totals.files_enqueued,
            totals.files_covered,
            std::env::var("WQM_BRANCH_PRUNE_COVERED_DELETE").unwrap_or_else(|_| "dry".into()),
            totals.files_deferred > 0,
            totals.files_deferred,
        );
    }
    Ok(totals)
}

/// Prune orphaned branches for a single project.
async fn prune_project_branches(
    pool: &SqlitePool,
    queue_manager: &std::sync::Arc<QueueManager>,
    watch_id: &str,
    tenant_id: &str,
    project_root: &str,
    budget: &mut CoveredBudget,
) -> Result<BranchPruneStats, String> {
    let root = WatchManager::resolve_local_watch_path(project_root);
    if !root.is_dir() {
        debug!("[branch_prune] Skipping {tenant_id} — path not a directory");
        return Ok(BranchPruneStats::default());
    }

    // Ground truth: the repo's live local branches. On ANY error (not a repo,
    // unreadable, etc.) skip — pruning without a confirmed live set could wipe a
    // project's entire index.
    let detector = BranchLifecycleDetector::with_defaults(root.clone());
    let live: HashSet<String> = match detector.list_all_branches() {
        Ok(branches) => branches.into_iter().map(|(name, _, _)| name).collect(),
        Err(e) => {
            debug!("[branch_prune] Skipping {tenant_id} — cannot list git branches: {e}");
            return Ok(BranchPruneStats::default());
        }
    };
    if live.is_empty() {
        // Defensive: an empty live set would mark every tracked branch orphaned.
        debug!("[branch_prune] Skipping {tenant_id} — git reported zero local branches");
        return Ok(BranchPruneStats::default());
    }

    // Consistency proof. The repo's current HEAD branch MUST appear in the live
    // set. `get_current_branch` falls back to "main" when the repo can't be read
    // or HEAD can't be resolved — the exact failure that, at index time, labels a
    // project's corpus under a non-existent branch (observed: bws-engineer's and
    // compress-mcp's content was indexed under a bogus "main" while git only had
    // master/dev-clean). If HEAD isn't in `live`, the stored branch labels are not
    // trustworthy ground truth for this repo — skip rather than risk deleting the
    // real index.
    let head = crate::watching_queue::get_current_branch(&root);
    if !live.contains(&head) {
        debug!(
            "[branch_prune] Skipping {tenant_id} — HEAD '{head}' not among live branches; \
             branch labels not trustworthy"
        );
        return Ok(BranchPruneStats::default());
    }

    // Per-branch tracked-file counts. The branch holding the most files is the
    // project's corpus; a genuinely-deleted feature branch is a minor offshoot,
    // never the bulk. Never prune the corpus branch — this is the primary guard
    // against deleting a mislabeled main index.
    let counts: Vec<(String, i64)> = sqlx::query_as(
        "SELECT je.value AS branch, COUNT(*) AS n \
         FROM tracked_files, json_each(branches) je \
         WHERE watch_folder_id = ?1 \
         GROUP BY je.value",
    )
    .bind(watch_id)
    .fetch_all(pool)
    .await
    .map_err(|e| format!("query tracked branches: {e}"))?;

    let primary = counts
        .iter()
        .max_by_key(|(_, n)| *n)
        .map(|(b, _)| b.as_str());

    // Which paths are still served by a generation on a LIVE branch (stage 3).
    // Computed once per project, here, because this is the only place holding
    // the git live set.
    let live_generations = live_generations_by_path(pool, watch_id, &live).await?;

    let mut stats = BranchPruneStats::default();
    for (branch, _count) in &counts {
        if live.contains(branch) {
            continue; // still a real git branch
        }
        // Safety nets — never delete the corpus or a default-named branch, even
        // when absent from git. A lingering stale branch is benign; deleting a
        // real index is not. Surface the skip so mislabeled corpora are visible.
        if Some(branch.as_str()) == primary {
            info!(
                "[branch_prune] {tenant_id} — branch '{branch}' absent from git but is the \
                 project's largest tracked branch; NOT pruning (likely a mislabeled corpus)"
            );
            continue;
        }
        if branch.as_str() == "main" || branch.as_str() == "master" {
            info!(
                "[branch_prune] {tenant_id} — branch '{branch}' absent from git but is a \
                 default branch name; NOT pruning (safety net)"
            );
            continue;
        }

        let enqueued = enqueue_branch_deletes(
            pool,
            queue_manager,
            watch_id,
            tenant_id,
            branch.as_str(),
            &live_generations,
            budget,
        )
        .await?;
        if enqueued.total > 0 {
            info!(
                "[branch_prune] {tenant_id} — branch '{branch}' no longer in git; \
                 enqueued {} file delete(s) ({} covered by a live generation)",
                enqueued.total, enqueued.covered
            );
            stats.branches_pruned += 1;
            stats.files_enqueued += enqueued.total;
            stats.files_covered += enqueued.covered;
        }
    }
    Ok(stats)
}

/// For each `relative_path` in this watch folder, the `file_id`s of the
/// generations that carry at least one LIVE branch tag.
///
/// One query per project at startup (a few thousand rows on a large repo) —
/// the map answers "is this path still served by another generation?" for every
/// delete this module enqueues, without a per-file query. Rows with an
/// unparsable `branches` value are treated as carrying nothing: they can never
/// COVER another generation (a fail-closed default — the worst case is
/// preserving a stale row, never deleting a live one).
async fn live_generations_by_path(
    pool: &SqlitePool,
    watch_id: &str,
    live: &HashSet<String>,
) -> Result<HashMap<String, Vec<i64>>, String> {
    let rows: Vec<(i64, String, String)> = sqlx::query_as(
        "SELECT file_id, relative_path, branches FROM tracked_files WHERE watch_folder_id = ?1",
    )
    .bind(watch_id)
    .fetch_all(pool)
    .await
    .map_err(|e| format!("query generations: {e}"))?;

    let mut map: HashMap<String, Vec<i64>> = HashMap::new();
    for (file_id, relative_path, branches_json) in rows {
        let branches: Vec<String> = serde_json::from_str(&branches_json).unwrap_or_default();
        if branches.iter().any(|b| live.contains(b.as_str())) {
            map.entry(relative_path).or_default().push(file_id);
        }
    }
    Ok(map)
}

/// Enqueue tally for one pruned branch.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
struct EnqueueTally {
    /// All `file|delete` items enqueued for the branch.
    total: u64,
    /// Of those, the ones stamped `covered_by_live_generation` (stage 3).
    covered: u64,
}

/// Enqueue a `file|delete` for every tracked file on `branch`.
///
/// Mirrors the branch-switch / folder-delete enqueue path: `ItemType::File` +
/// `QueueOperation::Delete` with the branch set, so the unified queue processor
/// performs the branch-safe, reference-counted purge of Qdrant points,
/// `tracked_files`, FTS5 entries, graph edges, and keyword extractions.
async fn enqueue_branch_deletes(
    pool: &SqlitePool,
    queue_manager: &std::sync::Arc<QueueManager>,
    watch_id: &str,
    tenant_id: &str,
    branch: &str,
    live_generations: &HashMap<String, Vec<i64>>,
    budget: &mut CoveredBudget,
) -> Result<EnqueueTally, String> {
    let files: Vec<(i64, String)> = sqlx::query_as(
        "SELECT file_id, relative_path FROM tracked_files \
         WHERE watch_folder_id = ?1 \
           AND EXISTS (SELECT 1 FROM json_each(branches) WHERE value = ?2)",
    )
    .bind(watch_id)
    .bind(branch)
    .fetch_all(pool)
    .await
    .map_err(|e| format!("query branch files: {e}"))?;

    let mut tally = EnqueueTally::default();
    for (file_id, rel) in files {
        let rel_path = match RelativePath::from_user_input(&rel) {
            Ok(r) => r,
            Err(e) => {
                warn!("[branch_prune] invalid relative_path {rel:?}: {e}");
                continue;
            }
        };
        let payload = FilePayload {
            file_path: rel_path,
            file_type: None,
            file_hash: None,
            size_bytes: None,
            old_path: None,
        };
        let payload_json =
            serde_json::to_string(&payload).map_err(|e| format!("serialize FilePayload: {e}"))?;

        // Stage 3: tell the delete side whether another generation on a live
        // branch still serves this path. Only that fact distinguishes a stale
        // Layer-2 generation from the mislabeled corpus the preserve guard
        // protects. The `branches`-set arithmetic itself stays on the delete
        // side (it re-reads the row under its own transaction).
        let covered = covered_by_other_live_generation(live_generations.get(&rel), file_id);
        // Per-run ceiling on the deletion-capable class only: skip (defer to the
        // next cycle) rather than enqueue once the cap is spent. Uncovered
        // deletes keep flowing — they are no-ops under the preserve guard.
        if covered && !budget.take() {
            continue;
        }
        let metadata = if covered {
            BRANCH_PRUNE_COVERED_DELETE_METADATA
        } else {
            BRANCH_PRUNE_DELETE_METADATA
        };

        match queue_manager
            .enqueue_unified(
                ItemType::File,
                QueueOperation::Delete,
                tenant_id,
                "projects",
                &payload_json,
                Some(branch),
                Some(metadata),
            )
            .await
        {
            Ok(_) => {
                tally.total += 1;
                if covered {
                    tally.covered += 1;
                }
            }
            Err(e) => warn!("[branch_prune] enqueue delete failed for {rel}: {e}"),
        }
    }
    Ok(tally)
}
