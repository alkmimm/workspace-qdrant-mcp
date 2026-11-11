//! Bulk branch-membership append — the `(Tenant, Scan)` items carrying a
//! [`BranchMembershipBulk`] marker.
//!
//! Two producers, two ways of certifying that a generation already IS the
//! branch's content:
//!
//! - **git no-diff** (`from_branch`): a `git checkout` lists the paths that
//!   produced no diff between the old and the new branch. The generation the
//!   old branch holds is certified for each path.
//! - **content hash** (`hashes`): a worktree reconcile hashes each working-tree
//!   file of the branch and lists the paths whose `(path, hash)` the index
//!   already holds, from whichever branch indexed it first (the trunk, the
//!   branch the worktree was created from, a sibling agent branch). The
//!   generation with that exact content is certified — no guess about where
//!   the branch came from.
//!
//! Either way the branch is appended to the three stores without reading or
//! embedding anything.

use std::sync::Arc;

use sqlx::SqlitePool;
use tracing::{info, warn};

use crate::context::ProcessingContext;
use crate::search_db::SearchDbManager;
use crate::unified_queue_processor::{UnifiedProcessorError, UnifiedProcessorResult};
use crate::unified_queue_schema::{BranchMembershipBulk, UnifiedQueueItem};
use wqm_common::timestamps;

/// Append `spec.branch` to every listed path's branch set across all three
/// stores WITHOUT re-embedding — the bulk counterpart of the per-file
/// cross-branch dedup fast-path.
///
/// Order matters for correctness: Qdrant FIRST (the proven per-base_point
/// primitive), then the two SQLite stores only for the base_points the vector
/// store actually backs. A base_point that returns 0 points is skipped (logged),
/// never tagged — so we never claim branch membership the vector store can't
/// serve; the per-file reconcile re-ingests it on a later scan. Every step is
/// idempotent (`NOT EXISTS` guards), so a retried op is a no-op once applied.
///
/// Safe to drive by path with no per-file read because the caller certified
/// each path's content — git no-diff, or a content hash — see
/// [`BranchMembershipBulk`].
pub(super) async fn handle_branch_membership_bulk(
    ctx: &ProcessingContext,
    item: &UnifiedQueueItem,
    spec: &BranchMembershipBulk,
) -> UnifiedProcessorResult<()> {
    if spec.paths.is_empty() {
        return Ok(());
    }

    // Candidate base_points: tracked on this watch folder, for one of the listed
    // paths, with a real base_point, not yet carrying the branch — scoped to the
    // certified generation: the one with the listed content hash, or (git
    // no-diff) the one holding the switch's old branch. Without that scope this
    // tagged EVERY generation of each path, stamping stale Layer-2 debris with
    // each newly created branch (#224 overlap fabricator).
    let selected = match &spec.hashes {
        Some(hashes) => {
            select_branch_bulk_candidates_by_hash(
                &ctx.pool,
                &spec.watch_folder_id,
                &spec.branch,
                &spec.paths,
                hashes,
            )
            .await
        }
        None => {
            select_branch_bulk_candidates(
                &ctx.pool,
                &spec.watch_folder_id,
                &spec.branch,
                spec.from_branch.as_deref(),
                &spec.paths,
            )
            .await
        }
    };
    let candidates = selected
        .map_err(|e| UnifiedProcessorError::ProcessingFailed(format!("branch bulk select: {e}")))?;
    if candidates.is_empty() {
        return Ok(());
    }

    // Qdrant first, then the two SQLite stores only for the base_points the
    // vector store backs (a base_point with no point is never tagged, so the
    // index never claims membership the vector store cannot serve).
    //
    // One batched append for the whole op (concurrent reads, writes grouped by
    // resulting branch set): ~1 s for 25 base_points instead of ~6 s of
    // sequential round-trips. The op is too short now for a lease to expire
    // mid-way (the lease loop that once forced per-base_point checkpoints), and
    // a crash between the two steps only re-runs an idempotent append: the
    // next pass's candidate SELECT still lists the base_points and the Qdrant
    // append skips arrays that already hold the branch.
    let tagged_bps = ctx
        .storage_client
        .add_branch_to_base_points(&item.collection, &candidates, &spec.branch)
        .await
        .map_err(|e| UnifiedProcessorError::Storage(e.to_string()))?;
    persist_branch_tag_for_base_points(
        &ctx.pool,
        ctx.search_db.as_ref(),
        &spec.watch_folder_id,
        &spec.branch,
        &tagged_bps,
    )
    .await?;
    let tagged = tagged_bps.len();
    let missing = candidates.len() - tagged;
    if missing > 0 {
        warn!(
            "branch bulk: {} base_points had 0 Qdrant points — left for per-file reconcile (branch={})",
            missing, spec.branch
        );
    }
    if tagged > 0 {
        info!(
            "branch bulk: tagged {} base_points onto branch '{}' (wf={}, {} paths in op)",
            tagged,
            spec.branch,
            spec.watch_folder_id,
            spec.paths.len()
        );
    }
    Ok(())
}

/// Select the base_points tracked under `watch_folder_id` for one of `paths`
/// that do NOT yet carry `branch` (with a real `base_point` and non-null
/// `branches`) — scoped, when `from_branch` is present, to generations that
/// HOLD `from_branch`.
///
/// The scope is what makes "drive by path with no per-file hash recheck"
/// sound: the caller verified the path produced no diff between `from_branch`
/// and `branch`, which certifies exactly ONE generation — the one tagged
/// `from_branch` — as the new branch's content. A path routinely has other
/// Layer-2 generations (older content still held by other branches, or stale
/// debris); tagging those too fabricated the #224 (path, branch) overlap
/// groups on every branch creation. `from_branch = None` (an op enqueued
/// before the field existed) keeps the legacy unscoped behaviour for the few
/// minutes such items can still sit in the queue.
///
/// Because it also filters on `branch NOT EXISTS`, base_points already
/// persisted by an earlier (possibly re-leased) pass are excluded — which is
/// what lets [`handle_branch_membership_bulk`] resume rather than restart on
/// re-lease.
pub(crate) async fn select_branch_bulk_candidates(
    pool: &SqlitePool,
    watch_folder_id: &str,
    branch: &str,
    from_branch: Option<&str>,
    paths: &[String],
) -> Result<Vec<String>, sqlx::Error> {
    if paths.is_empty() {
        return Ok(Vec::new());
    }
    let path_ph = vec!["?"; paths.len()].join(", ");
    let from_clause = if from_branch.is_some() {
        "AND EXISTS (SELECT 1 FROM json_each(tracked_files.branches) WHERE value = ?)"
    } else {
        ""
    };
    let select_sql = format!(
        "SELECT DISTINCT base_point FROM tracked_files
         WHERE watch_folder_id = ? AND base_point IS NOT NULL AND branches IS NOT NULL
           AND relative_path IN ({path_ph})
           AND NOT EXISTS (SELECT 1 FROM json_each(tracked_files.branches) WHERE value = ?)
           {from_clause}"
    );
    let mut sel = sqlx::query_scalar::<_, String>(&select_sql).bind(watch_folder_id);
    for p in paths {
        sel = sel.bind(p);
    }
    sel = sel.bind(branch);
    if let Some(fb) = from_branch {
        sel = sel.bind(fb);
    }
    sel.fetch_all(pool).await
}

/// Select, for each `(paths[i], hashes[i])`, the base_point of the generation
/// tracked under `watch_folder_id` with exactly that content — when `branch`
/// holds NO generation of the path yet.
///
/// The hash is what certifies the generation: it is the branch's own
/// working-tree content, whichever branch indexed it first. The generation must
/// still be held by some branch (a generation no branch holds is debris whose
/// vectors may be gone). The per-PATH `NOT EXISTS` keeps the branch at one
/// generation per path: if the live watcher already moved the branch to newer
/// bytes, the stale hash listed here is skipped instead of creating a second
/// generation for the same `(path, branch)` (the #224 overlap shape). It also
/// makes a re-leased op resume where it stopped.
pub(crate) async fn select_branch_bulk_candidates_by_hash(
    pool: &SqlitePool,
    watch_folder_id: &str,
    branch: &str,
    paths: &[String],
    hashes: &[String],
) -> Result<Vec<String>, sqlx::Error> {
    if paths.len() != hashes.len() {
        return Err(sqlx::Error::Protocol(format!(
            "branch bulk: {} paths but {} hashes",
            paths.len(),
            hashes.len()
        )));
    }
    if paths.is_empty() {
        return Ok(Vec::new());
    }
    let pairs: Vec<[&str; 2]> = paths
        .iter()
        .zip(hashes)
        .map(|(p, h)| [p.as_str(), h.as_str()])
        .collect();
    let pairs_json = serde_json::to_string(&pairs).map_err(|e| sqlx::Error::Encode(e.into()))?;
    // `json_each(?2)` drives the join: one indexed lookup per listed pair on
    // `idx_tracked_files_relative (watch_folder_id, relative_path)`.
    sqlx::query_scalar::<_, String>(
        "SELECT DISTINCT t.base_point
           FROM json_each(?2) AS c
           CROSS JOIN tracked_files t
             ON t.watch_folder_id = ?1
            AND t.relative_path = json_extract(c.value, '$[0]')
            AND t.file_hash = json_extract(c.value, '$[1]')
          WHERE t.base_point IS NOT NULL
            AND t.branches IS NOT NULL
            AND json_array_length(t.branches) > 0
            AND NOT EXISTS (
                SELECT 1 FROM tracked_files o, json_each(o.branches) ob
                 WHERE o.watch_folder_id = t.watch_folder_id
                   AND o.relative_path = t.relative_path
                   AND ob.value = ?3)",
    )
    .bind(watch_folder_id)
    .bind(pairs_json)
    .bind(branch)
    .fetch_all(pool)
    .await
}

/// Append `branch` to the given `base_points` in state.db (`tracked_files`) and,
/// when a search.db is present, in `file_metadata` (`code_lines` are shared by
/// `file_id`, so nothing is re-written there). Idempotent via the `branch NOT
/// EXISTS` guard.
///
/// The `tracked_files` update is scoped to `watch_folder_id`; the `file_metadata`
/// mirror is scoped to the SAME rows by resolving their `file_id`s first, NOT by
/// `base_point`. A `base_point` is shared across every clone/worktree of a
/// project holding that content, so a base_point-keyed `file_metadata` tag
/// leaked this branch onto sibling worktrees' generations (whose authority never
/// carried it) — the mirror drifted wider than `tracked_files` and a
/// branch-scoped `grep` surfaced those stale generations as duplicate hits.
///
/// The bulk handler calls this PER base_point so that each Qdrant `set_payload`
/// is immediately checkpointed: a lease expiry leaves committed work behind and
/// the op resumes from the next un-tagged base_point.
pub(crate) async fn persist_branch_tag_for_base_points(
    pool: &SqlitePool,
    search_db: Option<&Arc<SearchDbManager>>,
    watch_folder_id: &str,
    branch: &str,
    base_points: &[String],
) -> UnifiedProcessorResult<()> {
    if base_points.is_empty() {
        return Ok(());
    }
    let now = timestamps::now_utc();
    let bp_ph = vec!["?"; base_points.len()].join(", ");
    let update_sql = format!(
        "UPDATE tracked_files
         SET branches = json_insert(branches, '$[#]', ?), updated_at = ?
         WHERE watch_folder_id = ? AND base_point IN ({bp_ph})
           AND NOT EXISTS (SELECT 1 FROM json_each(tracked_files.branches) WHERE value = ?)"
    );
    let mut upd = sqlx::query(&update_sql)
        .bind(branch)
        .bind(&now)
        .bind(watch_folder_id);
    for bp in base_points {
        upd = upd.bind(bp);
    }
    upd.bind(branch)
        .execute(pool)
        .await
        .map_err(|e| UnifiedProcessorError::ProcessingFailed(format!("branch bulk update: {e}")))?;

    if let Some(search_db) = search_db {
        // Resolve the EXACT file_ids the tracked_files tag above targeted — this
        // watch folder's generations of these base_points — and mirror the
        // branch onto those rows only. See the fn-level note: keying search.db
        // by file_id (not base_point) keeps the mirror in lockstep with the
        // watch-folder-scoped authority instead of leaking onto sibling clones.
        let bp_ph = vec!["?"; base_points.len()].join(", ");
        let select_ids = format!(
            "SELECT file_id FROM tracked_files
             WHERE watch_folder_id = ? AND base_point IN ({bp_ph})"
        );
        let mut sel = sqlx::query_scalar::<_, i64>(&select_ids).bind(watch_folder_id);
        for bp in base_points {
            sel = sel.bind(bp);
        }
        let file_ids = sel.fetch_all(pool).await.map_err(|e| {
            UnifiedProcessorError::ProcessingFailed(format!("branch bulk file_id resolve: {e}"))
        })?;
        search_db
            .add_branch_to_file_metadata_by_file_ids(&file_ids, branch)
            .await
            .map_err(|e| {
                UnifiedProcessorError::ProcessingFailed(format!("branch bulk fts5: {e}"))
            })?;
    }
    Ok(())
}

#[cfg(test)]
#[path = "branch_bulk_tests.rs"]
mod tests;
