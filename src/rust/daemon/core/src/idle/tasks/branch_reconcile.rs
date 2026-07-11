//! Branch-tag reconcile — bring the shared Qdrant `branch` payload UP TO the
//! `tracked_files` authority for each base_point (issue #224, inverse sub-case).
//!
//! ## The drift this heals
//!
//! `tracked_files.branches` is the authority for which branches a base_point
//! belongs to. The Qdrant `branch` array on the shared points can fall BEHIND it
//! — a base_point that another branch tagged loses those tags when a later
//! full-ingest upsert overwrites the payload (fixed forward by #250/#256), and,
//! for content-UNCHANGED files, a `force:true` reembed never re-ingests them at
//! all, so no ingest-time fix runs. The result: `tracked_files.branches` holds
//! `main` but the Qdrant payload does not, so a branch-scoped search on `main`
//! silently misses the file (the #151 auto-widen fires only on a *total*-zero
//! result, not a partial miss inside a larger hit set).
//!
//! `reconcile_branch_membership` (branch-switch) only closes the OPPOSITE
//! direction — it adds a branch where the AUTHORITY lacks it. Nothing repairs a
//! base_point whose Qdrant array is a strict subset of the authority. This task
//! is that missing pass.
//!
//! ## What it does
//!
//! For each base_point the authority marks with 2+ branches (single-branch files
//! were ingested on that branch and already carry it — filtering to 2+ targets
//! the cross-branch/shared base_points where inverse-drift lives, and keeps the
//! per-base_point Qdrant read off the common case), it reads the live Qdrant
//! `branch` set and, if the authority holds branches Qdrant lacks, merges them in
//! via the daemon-owned `merge_branches_into_base_point`. ADDITIVE ONLY — it
//! never removes a tag (the forward-drift #224 direction, Qdrant-tagged-but-no-
//! row, is untouched) — and idempotent (a synced base_point is a no-op). A
//! base_point with zero live points is skipped: that is a missing-points /
//! empty-`[]` case for re-ingest, not a tag reconcile.
//!
//! Runs only in `FullIdle` (needs both Qdrant and SQLite), batched with a keyset
//! cursor on `file_id`, resumable and cancellable — same shape as
//! [`super::orphan_cleanup`].

use async_trait::async_trait;
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};

use crate::idle::task::{MaintenanceContext, MaintenanceResult, MaintenanceTask};
use crate::idle::IdleState;

/// One candidate row: `(file_id, base_point, branches_json, collection)`.
type CandidateRow = (i64, String, String, String);

/// Syncs the shared Qdrant `branch` array up to the `tracked_files` authority.
pub struct BranchReconcileTask {
    batch_size: i64,
    /// Keyset cursor: largest `file_id` already examined this cycle.
    cursor: i64,
    total_checked: u64,
    base_points_repaired: u64,
    tags_added: u64,
}

impl BranchReconcileTask {
    pub fn new() -> Self {
        Self {
            batch_size: 100,
            cursor: 0,
            total_checked: 0,
            base_points_repaired: 0,
            tags_added: 0,
        }
    }
}

impl Default for BranchReconcileTask {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl MaintenanceTask for BranchReconcileTask {
    fn name(&self) -> &str {
        "branch_reconcile"
    }

    fn required_idle_states(&self) -> &[IdleState] {
        &[IdleState::FullIdle]
    }

    fn idle_delay_secs(&self) -> u64 {
        120 // 2 minutes of idle
    }

    fn cooldown_secs(&self) -> u64 {
        3600 // once per hour
    }

    fn reset(&mut self) {
        self.cursor = 0;
        self.total_checked = 0;
        self.base_points_repaired = 0;
        self.tags_added = 0;
    }

    async fn run_batch(
        &mut self,
        ctx: &MaintenanceContext<'_>,
        cancel: &CancellationToken,
    ) -> MaintenanceResult {
        let rows = match fetch_candidate_batch(ctx.pool, self.cursor, self.batch_size).await {
            Ok(r) => r,
            Err(e) => {
                warn!("branch reconcile query failed: {} — will retry", e);
                return MaintenanceResult::Yielded;
            }
        };

        // Empty batch → the keyset has passed the last candidate; cycle done.
        let Some(next_cursor) = rows.last().map(|r| r.0) else {
            self.log_completion();
            return MaintenanceResult::Done;
        };

        for (_file_id, base_point, branches_json, collection) in &rows {
            if cancel.is_cancelled() {
                // Cursor not advanced: the batch is re-examined on resume, which
                // is idempotent (merge only adds missing tags).
                return MaintenanceResult::Yielded;
            }
            self.total_checked += 1;
            self.reconcile_base_point(ctx, base_point, branches_json, collection)
                .await;
        }

        self.cursor = next_cursor;
        MaintenanceResult::Continue
    }
}

impl BranchReconcileTask {
    /// Sync one base_point's Qdrant `branch` array up to its authority set.
    async fn reconcile_base_point(
        &mut self,
        ctx: &MaintenanceContext<'_>,
        base_point: &str,
        branches_json: &str,
        collection: &str,
    ) {
        let authority: Vec<String> = match serde_json::from_str(branches_json) {
            Ok(v) => v,
            Err(e) => {
                debug!(
                    "branch reconcile: unparsable branches {:?} for base_point {}: {}",
                    branches_json, base_point, e
                );
                return;
            }
        };

        let qdrant = match ctx.storage_client.read_branch_set(collection, base_point).await {
            Ok(s) => s,
            Err(e) => {
                warn!(
                    "branch reconcile: read_branch_set failed for base_point {} ({}): {}",
                    base_point, collection, e
                );
                return;
            }
        };
        if qdrant.is_empty() {
            // No live points back this base_point — a missing-points / empty-`[]`
            // case for re-ingest, not a tag reconcile. Leave it.
            return;
        }

        let missing = missing_branches(&authority, &qdrant);
        if missing.is_empty() {
            return;
        }

        match ctx
            .storage_client
            .merge_branches_into_base_point(collection, base_point, &missing)
            .await
        {
            Ok(()) => {
                self.base_points_repaired += 1;
                self.tags_added += missing.len() as u64;
                debug!(
                    "branch reconcile: added {:?} to base_point {} ({})",
                    missing, base_point, collection
                );
            }
            Err(e) => warn!(
                "branch reconcile: merge failed for base_point {} ({}): {}",
                base_point, collection, e
            ),
        }
    }

    fn log_completion(&self) {
        if self.base_points_repaired > 0 {
            info!(
                "Branch reconcile complete: checked={}, base_points_repaired={}, tags_added={}",
                self.total_checked, self.base_points_repaired, self.tags_added
            );
        } else {
            debug!(
                "Branch reconcile complete: checked={}, no drift",
                self.total_checked
            );
        }
    }
}

/// Branches the authority holds that the Qdrant `branch` array lacks — the tags
/// to add. Preserves authority order; deterministic. Pure (unit-tested).
fn missing_branches(authority: &[String], qdrant: &[String]) -> Vec<String> {
    authority
        .iter()
        .filter(|b| !qdrant.iter().any(|q| q == *b))
        .cloned()
        .collect()
}

/// Fetch the next batch of base_points whose authority marks them with 2+
/// branches, strictly after `after_file_id` (keyset pagination — same rationale
/// as [`super::orphan_cleanup::fetch_cleanup_batch`]). Single-branch files are
/// excluded: they were ingested on that branch and already carry it, so they are
/// not inverse-drift candidates and would only cost a redundant Qdrant read.
async fn fetch_candidate_batch(
    pool: &sqlx::SqlitePool,
    after_file_id: i64,
    limit: i64,
) -> Result<Vec<CandidateRow>, sqlx::Error> {
    sqlx::query_as(
        "SELECT file_id, base_point, branches, collection
         FROM tracked_files
         WHERE file_id > ?1
           AND base_point IS NOT NULL
           AND branches IS NOT NULL
           AND collection IS NOT NULL
           AND json_array_length(branches) >= 2
         ORDER BY file_id
         LIMIT ?2",
    )
    .bind(after_file_id)
    .bind(limit)
    .fetch_all(pool)
    .await
}

#[cfg(test)]
mod tests {
    use super::super::orphan_cleanup::tests_support::{seed_file, test_pool};
    use super::{fetch_candidate_batch, missing_branches};

    #[test]
    fn missing_branches_returns_authority_minus_qdrant_in_order() {
        let authority = vec!["main".to_string(), "featA".to_string(), "featB".to_string()];
        let qdrant = vec!["featA".to_string()];
        assert_eq!(
            missing_branches(&authority, &qdrant),
            vec!["main".to_string(), "featB".to_string()]
        );
        // Qdrant already holds every authority branch → nothing to add.
        let synced = vec!["featB".to_string(), "main".to_string(), "featA".to_string()];
        assert!(missing_branches(&authority, &synced).is_empty());
        // Empty authority → nothing to add.
        assert!(missing_branches(&[], &qdrant).is_empty());
    }

    #[tokio::test]
    async fn candidate_batch_only_returns_multi_branch_files() {
        let pool = test_pool().await;
        // A single-branch file (branches=["main"]) — NOT a candidate.
        seed_file(&pool, 1, true, true).await;
        // A file the authority marks on two branches — the candidate.
        let multi = seed_file(&pool, 2, true, true).await;
        sqlx::query("UPDATE tracked_files SET branches = ?1 WHERE file_id = ?2")
            .bind(r#"["main","fix/x"]"#)
            .bind(multi)
            .execute(&pool)
            .await
            .unwrap();

        let batch = fetch_candidate_batch(&pool, 0, 100).await.unwrap();
        let ids: Vec<i64> = batch.iter().map(|r| r.0).collect();
        assert_eq!(ids, vec![multi], "only the 2+-branch file is a candidate");
        // And the returned branches JSON is the multi-branch one.
        assert_eq!(batch[0].2, r#"["main","fix/x"]"#);
    }
}
