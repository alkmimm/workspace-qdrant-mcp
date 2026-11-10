//! Drop queued indexing work for branches git no longer has.
//!
//! Branch pruning reconciles the INDEX: it enqueues a `file|delete` for every
//! tracked file of a dead branch. It never looked at the QUEUE, so the
//! `add`/`update`/`uplift` items still pending for that branch survived it. When
//! they run, they index a branch that no longer exists. Each one either reads a
//! worktree that is gone, or reads the main tree and tags the dead branch
//! again, which the next prune cycle undoes with yet another round of deletes.
//!
//! Measured 2026-11-10: 224k of 470k pending adds (48%) belonged to 84 branches
//! already deleted from git (tecsul 132k, DOC-V2 92k). They had been enqueued
//! days earlier for short-lived agent worktrees, then waited behind a delete
//! backlog until their branch was gone.
//!
//! Only PENDING items are dropped, so an item a worker already holds runs to
//! completion. Deletes are kept: they are what cleans the dead branch out of
//! the index. Items created after the live branch set was read are kept too,
//! so a branch created during the cycle never loses work.

use std::collections::HashSet;

use sqlx::SqlitePool;
use tracing::info;
use wqm_common::constants::COLLECTION_PROJECTS;

/// Ops whose only effect is to (re)index a branch's content. Pointless once
/// the branch is gone; `delete` is deliberately absent.
const INDEXING_OPS: &str = "'add', 'update', 'uplift'";

/// Remove pending indexing work queued for branches absent from `live`.
///
/// `protected` names branches that must keep their work even when git does not
/// list them: the default names and the project's corpus branch, the same
/// guards branch pruning applies (a repo read at enqueue time can fall back to
/// a branch label git never had). `cutoff` is a timestamp taken BEFORE `live`
/// was read; only items created earlier are eligible. Returns the number of
/// queue items removed.
pub(crate) async fn purge_dead_branch_work(
    pool: &SqlitePool,
    tenant_id: &str,
    live: &HashSet<String>,
    protected: &[&str],
    cutoff: &str,
) -> Result<u64, String> {
    let mut removed = 0u64;
    for branch in queued_branches(pool, tenant_id).await? {
        if live.contains(&branch) || protected.contains(&branch.as_str()) {
            continue;
        }
        let n = purge_branch(pool, tenant_id, &branch, cutoff).await?;
        if n > 0 {
            info!(
                "[branch_prune] {tenant_id} — branch '{branch}' no longer in git; dropped {n} \
                 pending indexing item(s) from the queue"
            );
        }
        removed += n;
    }
    Ok(removed)
}

/// Distinct branches with queued file work for this tenant.
///
/// Reads the `(tenant_id, branch, …)` prefix of the per-file uniqueness index,
/// so it never touches the table rows, even with hundreds of thousands queued.
async fn queued_branches(pool: &SqlitePool, tenant_id: &str) -> Result<Vec<String>, String> {
    sqlx::query_scalar(
        "SELECT DISTINCT branch FROM unified_queue \
         WHERE tenant_id = ?1 AND file_path IS NOT NULL AND branch IS NOT NULL",
    )
    .bind(tenant_id)
    .fetch_all(pool)
    .await
    .map_err(|e| format!("query queued branches: {e}"))
}

/// Delete one dead branch's pending indexing items (one short statement).
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
           AND op IN ({INDEXING_OPS}) AND status = 'pending' AND created_at < ?3"
    );
    let result = sqlx::query(&sql)
        .bind(tenant_id)
        .bind(branch)
        .bind(cutoff)
        .execute(pool)
        .await
        .map_err(|e| format!("purge queued work for branch '{branch}': {e}"))?;
    Ok(result.rows_affected())
}
