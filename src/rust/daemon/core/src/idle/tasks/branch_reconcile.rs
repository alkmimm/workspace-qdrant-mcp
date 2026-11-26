//! Branch-tag reconcile — bring the shared Qdrant `branch` payload AND the
//! search.db `file_metadata.branches` set UP TO the `tracked_files` authority
//! for each base_point (issue #224, inverse sub-case).
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
//! The SAME drift lives in the third store: search.db `file_metadata.branches`
//! (the branch filter behind grep / exact FTS reads). It drifts BOTH ways:
//! * NARROWER than authority — the CURRENT generation missing a tag the
//!   authority holds (measured 2026-07-15: a file carried every branch EXCEPT
//!   `main`, so a main-scoped grep missed it and the auto-widen surfaced STALE
//!   generations, "167 matches for ~2 real lines").
//! * WIDER than authority — a STALE generation still carrying a tag the
//!   authority moved to a newer generation (measured 2026-08-05: 109 rows,
//!   incl. `main` on 3 generations of one file). A branch-scoped grep then
//!   returns those dead generations as duplicate hits.
//!
//! Because `file_metadata.file_id` is 1:1 with `tracked_files.file_id`, a file's
//! mirror MUST EQUAL its authority — so this task keeps `file_metadata.branches`
//! an EXACT MIRROR of the authority (add missing AND remove stale), via the
//! shared [`crate::search_db::branch_mirror::target_branches`]. The one-time
//! backlog sweep lives in `search_db::branch_mirror`; this is the ongoing net.
//!
//! `reconcile_branch_membership` (branch-switch) only closes the ADD direction.
//! This task is the missing exact-sync pass for `file_metadata`.
//!
//! ## What it does
//!
//! For EVERY tracked file with a usable authority it syncs search.db
//! `file_metadata.branches` to exactly the authority set (one cheap indexed
//! SELECT each — including single-branch files, which is what heals a
//! corrupt/`[]` set). An EMPTY authority is a guard-skip, never a wipe.
//!
//! The Qdrant side is DIFFERENT and stays ADDITIVE ONLY: a base_point is SHARED
//! across generations/clones, so removing a tag there is the dangerous
//! forward-drift #224 direction. For base_points the authority marks with 2+
//! branches — the only shape where Qdrant-side inverse drift can exist, since a
//! single-branch file was ingested on that branch and already carries the tag —
//! it reads the live Qdrant `branch` set by tracked point ID, in one batch per
//! collection on each page. Candidates without a verified point use a filtered
//! Scroll. If the authority holds branches
//! Qdrant lacks, merges them in via the daemon-owned
//! `merge_branches_into_base_point`. Idempotent (a synced base_point is a
//! no-op). A base_point with zero live points is skipped: that is a
//! missing-points / empty-`[]` case for re-ingest, not a tag reconcile.
//!
//! Runs only in `FullIdle` (needs both Qdrant and SQLite), batched with a keyset
//! cursor on `file_id`, resumable and cancellable — same shape as
//! [`super::orphan_cleanup`].

use std::collections::{BTreeMap, HashMap};
use std::time::Duration;

use async_trait::async_trait;
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};
use uuid::Uuid;

use crate::idle::task::{MaintenanceContext, MaintenanceResult, MaintenanceTask};
use crate::idle::IdleState;
use crate::queue_operations::has_ready_work;
use crate::search_db::branch_mirror::target_branches;

// The queue loop awaits maintenance. Bound the new batched read so a slow
// Qdrant response cannot keep the next dequeue waiting on its normal retries.
const GET_TIMEOUT: Duration = Duration::from_secs(1);

#[derive(sqlx::FromRow)]
struct CandidateRow {
    file_id: i64,
    base_point: String,
    branches: String,
    collection: String,
    point_id: Option<String>,
}

impl CandidateRow {
    fn tracked_uuid(&self) -> Option<Uuid> {
        Uuid::parse_str(self.point_id.as_deref()?).ok()
    }
}

#[derive(Default)]
struct ReconcileReads {
    get_batches: u64,
    get_errors: u64,
    fallback_scrolls: u64,
    repair_scrolls: u64,
}

/// Syncs the shared Qdrant `branch` array up to the `tracked_files` authority.
pub struct BranchReconcileTask {
    batch_size: i64,
    /// Keyset cursor: largest `file_id` already examined this cycle.
    cursor: i64,
    total_checked: u64,
    base_points_repaired: u64,
    tags_added: u64,
    /// search.db `file_metadata` rows whose `branches` set was synced to the
    /// authority this cycle (missing tags added AND stale tags removed).
    file_metadata_repaired: u64,
    reads: ReconcileReads,
}

impl BranchReconcileTask {
    pub fn new() -> Self {
        Self {
            batch_size: 100,
            cursor: 0,
            total_checked: 0,
            base_points_repaired: 0,
            tags_added: 0,
            file_metadata_repaired: 0,
            reads: ReconcileReads::default(),
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
        self.file_metadata_repaired = 0;
        self.reads = ReconcileReads::default();
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

        if rows.is_empty() {
            self.log_completion();
            return MaintenanceResult::Done;
        }

        let mut ids_by_collection = BTreeMap::<&str, Vec<Uuid>>::new();
        for row in &rows {
            if let Some(id) = row.tracked_uuid() {
                ids_by_collection
                    .entry(&row.collection)
                    .or_default()
                    .push(id);
            }
        }
        let mut branch_sets = HashMap::new();
        for (collection, mut ids) in ids_by_collection {
            if should_yield(ctx, cancel).await {
                return MaintenanceResult::Yielded;
            }
            ids.sort_unstable();
            ids.dedup();
            self.reads.get_batches += 1;
            let read = ctx
                .storage_client
                .read_tracked_branch_sets(collection, &ids, None);
            match tokio::time::timeout(GET_TIMEOUT, read).await {
                Ok(Ok(sets)) => {
                    branch_sets.insert(collection, sets);
                }
                Ok(Err(error)) => {
                    self.reads.get_errors += 1;
                    warn!(collection, %error, "branch reconcile: batch Get failed, using Scroll");
                }
                Err(_) => {
                    self.reads.get_errors += 1;
                    warn!(
                        collection,
                        "branch reconcile: batch Get timed out, using Scroll"
                    );
                }
            }
        }

        for row in &rows {
            if cancel.is_cancelled() {
                return MaintenanceResult::Yielded;
            }
            let authority: Vec<String> = match serde_json::from_str(&row.branches) {
                Ok(v) => v,
                Err(error) => {
                    debug!(file_id = row.file_id, %error, "branch reconcile: invalid branch authority");
                    self.cursor = row.file_id;
                    self.total_checked += 1;
                    continue;
                }
            };

            // FTS branch membership is independently keyed by file_id and covers
            // single-branch files and rows without matching Qdrant points.
            self.reconcile_file_metadata(ctx, row.file_id, &authority)
                .await;
            if authority.len() >= 2 {
                let returned = row
                    .tracked_uuid()
                    .and_then(|id| branch_sets.get(row.collection.as_str())?.get(&id));
                let verified = verified_branch_set(returned, &row.base_point);
                if !self
                    .reconcile_base_point(ctx, cancel, row, &authority, verified)
                    .await
                {
                    return MaintenanceResult::Yielded;
                }
            }
            self.cursor = row.file_id;
            self.total_checked += 1;
        }

        MaintenanceResult::Continue
    }
}

impl BranchReconcileTask {
    /// Sync the search.db `file_metadata.branches` set for this file to EXACTLY
    /// the authority (add missing AND remove stale) — the mirror is 1:1 with the
    /// authority by `file_id`. Skipped when the context carries no search.db
    /// handle (e.g. tests).
    async fn reconcile_file_metadata(
        &mut self,
        ctx: &MaintenanceContext<'_>,
        file_id: i64,
        authority: &[String],
    ) {
        let Some(search_db) = ctx.search_db else {
            return;
        };
        match sync_authority_into_file_metadata(search_db.pool(), file_id, authority).await {
            Ok(0) => {}
            Ok(changed) => {
                self.file_metadata_repaired += 1;
                debug!(
                    "branch reconcile: synced file_metadata for file_id {} to authority ({} tag(s) changed)",
                    file_id, changed
                );
            }
            Err(e) => warn!(
                "branch reconcile: file_metadata sync failed for file_id {}: {}",
                file_id, e
            ),
        }
    }

    /// Returns false when foreground work requires resuming this row later.
    async fn reconcile_base_point(
        &mut self,
        ctx: &MaintenanceContext<'_>,
        cancel: &CancellationToken,
        row: &CandidateRow,
        authority: &[String],
        verified: Option<&[String]>,
    ) -> bool {
        let fallback;
        let current = match verified {
            Some(set) => set,
            None => {
                if should_yield(ctx, cancel).await {
                    return false;
                }
                self.reads.fallback_scrolls += 1;
                fallback = match ctx
                    .storage_client
                    .read_branch_set(&row.collection, &row.base_point)
                    .await
                {
                    Ok(set) => set,
                    Err(error) => {
                        warn!(file_id = row.file_id, %error, "branch reconcile: fallback Scroll failed");
                        return true;
                    }
                };
                &fallback
            }
        };
        if current.is_empty() {
            // Missing points or an empty branch array are for re-ingestion.
            return true;
        }
        let missing = missing_branches(authority, current);
        if missing.is_empty() {
            return true;
        }
        if should_yield(ctx, cancel).await {
            return false;
        }

        // Keep the existing additive repair and its fresh read before writing.
        self.reads.repair_scrolls += 1;
        match ctx
            .storage_client
            .merge_branches_into_base_point(&row.collection, &row.base_point, &missing)
            .await
        {
            Ok(()) => {
                self.base_points_repaired += 1;
                self.tags_added += missing.len() as u64;
                debug!(
                    file_id = row.file_id,
                    tags_added = missing.len(),
                    "branch reconcile: added missing tags"
                );
            }
            Err(error) => warn!(file_id = row.file_id, %error, "branch reconcile: merge failed"),
        }
        true
    }

    fn log_completion(&self) {
        info!(
            checked = self.total_checked,
            base_points_repaired = self.base_points_repaired,
            tags_added = self.tags_added,
            file_metadata_repaired = self.file_metadata_repaired,
            get_batches = self.reads.get_batches,
            get_errors = self.reads.get_errors,
            fallback_scrolls = self.reads.fallback_scrolls,
            repair_scrolls = self.reads.repair_scrolls,
            "Branch reconcile complete"
        );
    }
}

/// A point whose payload belongs to another generation cannot authorize a
/// repair. Preserve the Scroll path for missing or unusable metadata.
fn verified_branch_set<'a>(
    entry: Option<&'a (String, Vec<String>)>,
    base_point: &str,
) -> Option<&'a [String]> {
    entry
        .filter(|(returned, branches)| returned == base_point && !branches.is_empty())
        .map(|(_, branches)| branches.as_slice())
}

async fn should_yield(ctx: &MaintenanceContext<'_>, cancel: &CancellationToken) -> bool {
    if cancel.is_cancelled() {
        return true;
    }
    match has_ready_work(ctx.pool).await {
        Ok(ready) => ready,
        Err(error) => {
            debug!(%error, "branch reconcile: queue probe failed");
            true
        }
    }
}

/// Sync the search.db `file_metadata.branches` set for `file_id` to EXACTLY the
/// `authority` set — adds missing tags AND removes stale ones, so the mirror
/// (1:1 with the authority by `file_id`) can never be wider or narrower. Returns
/// the number of tags changed (0 when the row is absent, already in sync, or the
/// authority is empty). An empty authority is a guard-skip, never a wipe (see
/// [`target_branches`]); an unparsable stored array is treated as empty, so the
/// repair also heals a corrupt/`[]` set toward a non-empty authority.
///
/// Read-then-write without a transaction is safe here: the task only runs in
/// `FullIdle`, so no ingest upsert can interleave with the sync.
pub(crate) async fn sync_authority_into_file_metadata(
    pool: &sqlx::SqlitePool,
    file_id: i64,
    authority: &[String],
) -> Result<usize, sqlx::Error> {
    let stored: Option<String> =
        sqlx::query_scalar("SELECT branches FROM file_metadata WHERE file_id = ?1")
            .bind(file_id)
            .fetch_optional(pool)
            .await?;
    let Some(stored_json) = stored else {
        return Ok(0);
    };
    let current: Vec<String> = serde_json::from_str(&stored_json).unwrap_or_default();
    let Some(target) = target_branches(authority, &current) else {
        return Ok(0);
    };
    let changed = current.iter().filter(|b| !target.contains(b)).count()
        + target.iter().filter(|b| !current.contains(b)).count();
    sqlx::query("UPDATE file_metadata SET branches = ?1 WHERE file_id = ?2")
        .bind(serde_json::json!(target).to_string())
        .bind(file_id)
        .execute(pool)
        .await?;
    Ok(changed)
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

/// Fetch the next batch of tracked files with a usable authority, strictly
/// after `after_file_id` (keyset pagination — same rationale as
/// [`super::orphan_cleanup::fetch_cleanup_batch`]).
///
/// Includes single-branch files: the search.db `file_metadata` repair covers
/// them too (one cheap indexed SELECT each — this is what heals a corrupt/`[]`
/// branches set on a file that only ever lived on one branch). The EXPENSIVE
/// Qdrant lookup stays gated to 2+-branch authorities in
/// `run_batch` — a single-branch file was ingested on that branch and its
/// Qdrant payload already carries it, so the read would be redundant.
async fn fetch_candidate_batch(
    pool: &sqlx::SqlitePool,
    after_file_id: i64,
    limit: i64,
) -> Result<Vec<CandidateRow>, sqlx::Error> {
    // Scroll orders UUIDs by ID. Pick the same representative among the
    // tracked chunks, regardless of chunk index or stored UUID spelling.
    sqlx::query_as(
        "SELECT tf.file_id, tf.base_point, tf.branches, tf.collection,
                CASE WHEN json_array_length(
                    CASE WHEN json_valid(tf.branches) THEN tf.branches ELSE '[]' END
                ) >= 2 THEN (
                    SELECT point_id FROM qdrant_chunks
                    WHERE file_id = tf.file_id
                    ORDER BY lower(replace(point_id, '-', '')) LIMIT 1
                ) END AS point_id
         FROM tracked_files tf
         WHERE tf.file_id > ?1
           AND tf.base_point IS NOT NULL
           AND tf.branches IS NOT NULL
           AND tf.collection IS NOT NULL
         ORDER BY tf.file_id
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
    use super::{
        fetch_candidate_batch, missing_branches, sync_authority_into_file_metadata,
        verified_branch_set,
    };

    /// Temp search.db with one seeded `file_metadata` row (branches as given).
    async fn search_db_with_row(
        file_id: i64,
        branch: &str,
    ) -> (tempfile::TempDir, crate::search_db::SearchDbManager) {
        let dir = tempfile::tempdir().unwrap();
        let db = crate::search_db::SearchDbManager::new(dir.path().join("search.db"))
            .await
            .unwrap();
        sqlx::query(crate::code_lines_schema::UPSERT_FILE_METADATA_SQL)
            .bind(file_id)
            .bind("tenant-a")
            .bind(branch)
            .bind("/repo/src/lib.rs")
            .bind("bp1")
            .bind("src/lib.rs")
            .bind("hash")
            .bind(64_i64)
            .bind(0_i64)
            .execute(db.pool())
            .await
            .unwrap();
        (dir, db)
    }

    async fn stored_branches(db: &crate::search_db::SearchDbManager, file_id: i64) -> Vec<String> {
        let json: String =
            sqlx::query_scalar("SELECT branches FROM file_metadata WHERE file_id = ?1")
                .bind(file_id)
                .fetch_one(db.pool())
                .await
                .unwrap();
        serde_json::from_str(&json).unwrap()
    }

    #[tokio::test]
    async fn file_metadata_sync_adds_missing_authority_branches() {
        // The narrower-drift shape: the current generation's file_metadata
        // carries the feature branches but NOT `main`, while the authority does —
        // a main-scoped grep then misses the CURRENT content and the auto-widen
        // surfaces stale generations instead.
        let (_d, db) = search_db_with_row(7, "feat/x").await;
        let authority = vec!["feat/x".to_string(), "main".to_string()];

        let changed = sync_authority_into_file_metadata(db.pool(), 7, &authority)
            .await
            .unwrap();

        assert_eq!(changed, 1);
        assert_eq!(stored_branches(&db, 7).await, vec!["feat/x", "main"]);

        // Idempotent: a synced row is a no-op.
        let again = sync_authority_into_file_metadata(db.pool(), 7, &authority)
            .await
            .unwrap();
        assert_eq!(again, 0);
        assert_eq!(stored_branches(&db, 7).await, vec!["feat/x", "main"]);
    }

    #[tokio::test]
    async fn file_metadata_sync_removes_stale_wider_tag() {
        // A stored branch the authority DROPPED must be removed — this is the
        // duplicate-grep-hit direction (the mirror is 1:1 with the authority by
        // file_id, so it may not be wider). The target is exactly the authority.
        let (_d, db) = search_db_with_row(7, "feat/extra").await;
        // Seed the drifted state: fm carries a stale `main` the authority lacks.
        sqlx::query(
            r#"UPDATE file_metadata SET branches = '["feat/extra","main"]' WHERE file_id = 7"#,
        )
        .execute(db.pool())
        .await
        .unwrap();
        let authority = vec!["feat/extra".to_string()];

        let changed = sync_authority_into_file_metadata(db.pool(), 7, &authority)
            .await
            .unwrap();

        assert_eq!(changed, 1, "the stale `main` tag is removed");
        assert_eq!(stored_branches(&db, 7).await, vec!["feat/extra"]);
    }

    #[tokio::test]
    async fn file_metadata_sync_empty_authority_never_wipes() {
        // Guard: an empty authority must NOT blank the mirror (that would hide
        // the file from every branch-scoped grep).
        let (_d, db) = search_db_with_row(7, "main").await;
        let changed = sync_authority_into_file_metadata(db.pool(), 7, &[])
            .await
            .unwrap();
        assert_eq!(changed, 0);
        assert_eq!(stored_branches(&db, 7).await, vec!["main"]);
    }

    #[tokio::test]
    async fn file_metadata_sync_noop_without_row() {
        let (_d, db) = search_db_with_row(7, "main").await;
        let changed = sync_authority_into_file_metadata(db.pool(), 999, &["main".to_string()])
            .await
            .unwrap();
        assert_eq!(changed, 0);
    }

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
    async fn candidate_batch_includes_single_branch_files() {
        let pool = test_pool().await;
        // Single-branch file: a candidate for the search.db file_metadata
        // repair (the Qdrant read is gated separately in run_batch on 2+).
        let single = seed_file(&pool, 1, true, true).await;
        // Multi-branch file: candidate for BOTH repairs.
        let multi = seed_file(&pool, 2, true, true).await;
        sqlx::query("UPDATE tracked_files SET branches = ?1 WHERE file_id = ?2")
            .bind(r#"["main","fix/x"]"#)
            .bind(multi)
            .execute(&pool)
            .await
            .unwrap();

        let batch = fetch_candidate_batch(&pool, 0, 100).await.unwrap();
        let ids: Vec<i64> = batch.iter().map(|r| r.file_id).collect();
        assert_eq!(
            ids,
            vec![single, multi],
            "single-branch files are candidates too (fm-side repair)"
        );
        let multi_row = batch.iter().find(|r| r.file_id == multi).unwrap();
        assert_eq!(multi_row.branches, r#"["main","fix/x"]"#);
    }
    #[test]
    fn batch_get_missing_or_unrelated_branch_set_requires_scroll() {
        let good = ("base-a".to_string(), vec!["main".to_string()]);
        assert_eq!(
            verified_branch_set(Some(&good), "base-a"),
            Some(good.1.as_slice())
        );
        assert!(verified_branch_set(Some(&good), "base-b").is_none());
        assert!(verified_branch_set(Some(&("base-a".into(), Vec::new())), "base-a").is_none());
        assert!(verified_branch_set(None, "base-a").is_none());
    }

    #[tokio::test]
    async fn candidate_batch_selects_lowest_tracked_uuid_only_for_multibranch_rows() {
        let pool = test_pool().await;
        let single = seed_file(&pool, 1, true, true).await;
        let multi = seed_file(&pool, 2, true, true).await;
        let legacy = seed_file(&pool, 3, true, true).await;
        let smallest = "00000000-0000-0000-0000-000000000001";
        sqlx::query(
            "UPDATE tracked_files SET branches = '[\"main\",\"fix/x\"]' WHERE file_id IN (?1, ?2)",
        )
        .bind(multi)
        .bind(legacy)
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query("DELETE FROM qdrant_chunks")
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query("INSERT INTO qdrant_chunks (file_id, point_id, chunk_index, content_hash, created_at)
                     VALUES (?1, ?3, 0, 'hash', ''), (?2, ?4, 0, 'hash', ''), (?2, ?3, 1, 'hash', '')")
            .bind(single)
            .bind(multi)
            .bind(smallest)
            .bind("00000000000000000000000000000002")
            .execute(&pool)
            .await
            .unwrap();

        let rows = fetch_candidate_batch(&pool, 0, 3).await.unwrap();
        assert!(rows[0].tracked_uuid().is_none());
        assert_eq!(rows[1].tracked_uuid().unwrap().to_string(), smallest);
        assert!(rows[2].tracked_uuid().is_none());
    }
}
