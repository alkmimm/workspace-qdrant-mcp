//! `file_metadata.branches` → `tracked_files` authority reconcile.
//!
//! `file_metadata.file_id` is 1:1 with `tracked_files.file_id` (a shared,
//! `AUTOINCREMENT`, never-reused id space), so a file's search.db branch set
//! MUST equal its `tracked_files` branch set — the authority. The FTS branch
//! filter behind `grep`/exact search is `EXISTS(json_each(fm.branches) WHERE
//! value = ?)`, so any branch the mirror carries that the authority dropped is a
//! false positive: a branch-scoped query returns that stale content generation
//! as a duplicate hit.
//!
//! ## The drift this heals
//!
//! Two directions have leaked the mirror OUT of sync with the authority:
//!
//! * **Wider than authority** (the one this heals): a `base_point`
//!   (`SHA256(tenant|relative_path|file_hash)`) is shared by every clone/worktree
//!   of a project holding that content, but each keeps its OWN `file_id`. A bulk
//!   branch re-key that tagged `file_metadata` by `base_point` (fixed forward in
//!   [`super::SearchDbManager::add_branch_to_file_metadata_by_file_ids`]) stamped
//!   the branch onto sibling worktrees' generations whose `tracked_files` never
//!   carried it. Measured live (this repo, 2026-08-05): 109 rows drifted wider,
//!   32 paths carried `main` on 2+ generations at once — including the reported
//!   `profile_detail_page.dart` (`main` on 3 generations).
//! * **Narrower than authority**: the current generation missing a tag the
//!   authority holds — healed additively by the idle
//!   [`crate::idle::tasks::BranchReconcileTask`].
//!
//! This is the one-time backlog sweep for the WIDER direction: the additive idle
//! task can never remove a stale tag, and the per-file write paths only touch the
//! generation being written, so accumulated debris on OTHER generations lingers
//! forever. Run at startup BEFORE the queue processor starts — the same
//! pre-`start` window as [`super::orphan_gc`], so no FTS5 write can race the
//! read-then-write.
//!
//! ## Guard
//!
//! An EMPTY (or unparsable) authority is treated as "unknown", never as "remove
//! every branch": rewriting the mirror to `[]` would hide the file from every
//! branch-scoped `grep`. This mirrors the #224 invariant that an empty authority
//! read is a failure, not truth. Rows whose `file_id` is absent from
//! `tracked_files` are left to [`super::orphan_gc`] (which deletes them
//! wholesale); this pass only re-keys rows the authority still knows.
//!
//! ## Why REMOVING a tag here is safe (not just adding)
//!
//! The mirror can never legitimately be WIDER than the authority: `file_id` is
//! the `AUTOINCREMENT` PK allocated BY the `tracked_files` INSERT
//! (`store_track::upsert_and_track` returns it), so a `file_metadata` row cannot
//! exist before its authority row,
//! and every branch-ADD writes `tracked_files.branches` BEFORE the search.db
//! mirror (dedup insert → FTS enqueue; the bulk re-key updates state.db then
//! search.db; ingest sets the tracked row before `full_rewrite`). A crash
//! therefore leaves authority ⊇ mirror, never the reverse. The delete path in
//! turn drops the `tracked_files` tag FIRST, so a mirror still carrying a tag
//! the authority dropped is either drift (the bug this heals) or a branch-move
//! whose search.db removal has not landed yet — in BOTH cases removing the
//! surplus tag is the correct outcome. Hence exact-mirror (add missing AND
//! remove stale) is safe given only the empty-authority guard above. If a future
//! change ever wrote `file_metadata.branches` before `tracked_files.branches`,
//! that invariant — and this removal — would break.

use std::collections::HashMap;

use sqlx::SqlitePool;
use tracing::{info, warn};

use super::types::SearchDbResult;
use super::SearchDbManager;

/// Outcome of one reconcile pass.
#[derive(Debug, Default, Clone, Copy)]
pub struct BranchMirrorStats {
    /// `file_metadata` rows whose `branches` set was rewritten to the authority.
    pub rows_resynced: u64,
    /// Stale branch tags removed across those rows (mirror-wider direction).
    pub tags_removed: u64,
    /// Missing branch tags added across those rows (mirror-narrower direction).
    pub tags_added: u64,
}

/// The branch set `file_metadata.branches` should become to mirror the
/// `tracked_files` authority for the SAME `file_id`, or `None` when no write is
/// needed.
///
/// Returns `None` when the row is already in sync (set equality, order- and
/// duplicate-insensitive) OR when the authority is empty — an empty authority
/// is "unknown, do not wipe", never "remove every branch" (see the module
/// guard). Otherwise returns the canonical target: the authority set,
/// deduplicated, in authority order.
pub(crate) fn target_branches(authority: &[String], current: &[String]) -> Option<Vec<String>> {
    // Canonical target: authority, de-duplicated, order-preserving.
    let mut canon: Vec<String> = Vec::with_capacity(authority.len());
    for b in authority {
        if !canon.contains(b) {
            canon.push(b.clone());
        }
    }
    // Guard: never rewrite the mirror to empty on an empty/unknown authority.
    if canon.is_empty() {
        return None;
    }
    // Already in sync? Compare as sets (json_each ignores order/duplicates).
    let in_sync = canon.iter().all(|b| current.contains(b))
        && current.iter().all(|b| canon.contains(b));
    if in_sync {
        return None;
    }
    Some(canon)
}

/// Parse a `branches` JSON-array column into a Vec, tolerating NULL/empty/
/// malformed JSON as an empty set (a corrupt row degrades to "no branches").
fn parse_branches(json: &str) -> Vec<String> {
    serde_json::from_str(json).unwrap_or_default()
}

/// Re-key every `file_metadata` row whose `branches` set drifted from its
/// `tracked_files` authority back to the authority.
///
/// `state_pool` is the state.db pool (`tracked_files` lives there). Reads both
/// tables in full, diffs in memory, and writes only the drifted rows — the same
/// cross-database diff shape as [`super::orphan_gc::gc_orphaned_files`], and safe
/// for the same reason (run pre-`start`, no FTS5 write in flight). `branches` is
/// NOT part of the FTS5 content index (the filter joins `file_metadata` at query
/// time), so no rebuild is needed.
pub async fn reconcile_file_metadata_branches(
    search_db: &SearchDbManager,
    state_pool: &SqlitePool,
) -> SearchDbResult<BranchMirrorStats> {
    let search_pool = search_db.pool();

    // Authority: file_id -> branches, from state.db.
    let authority_rows: Vec<(i64, String)> = sqlx::query_as::<_, (i64, String)>(
        "SELECT file_id, branches FROM tracked_files WHERE branches IS NOT NULL",
    )
    .fetch_all(state_pool)
    .await?;
    if authority_rows.is_empty() {
        // No authority to mirror against — treat as unknown, touch nothing
        // (orphan_gc owns the "authority is truly empty" wipe decision).
        return Ok(BranchMirrorStats::default());
    }
    let authority: HashMap<i64, Vec<String>> = authority_rows
        .into_iter()
        .map(|(id, j)| (id, parse_branches(&j)))
        .collect();

    // Current mirror: file_id -> branches, from search.db.
    let current_rows: Vec<(i64, String)> =
        sqlx::query_as::<_, (i64, String)>("SELECT file_id, branches FROM file_metadata")
            .fetch_all(search_pool)
            .await?;

    // Diff: collect (file_id, new_json, removed, added) for drifted rows.
    struct Update {
        file_id: i64,
        new_json: String,
        removed: u64,
        added: u64,
    }
    let mut updates: Vec<Update> = Vec::new();
    for (file_id, cur_json) in &current_rows {
        // file_id absent from tracked_files → orphan; orphan_gc handles it.
        let Some(auth) = authority.get(file_id) else {
            continue;
        };
        let current = parse_branches(cur_json);
        if let Some(target) = target_branches(auth, &current) {
            let removed = current.iter().filter(|b| !target.contains(b)).count() as u64;
            let added = target.iter().filter(|b| !current.contains(b)).count() as u64;
            updates.push(Update {
                file_id: *file_id,
                new_json: serde_json::json!(target).to_string(),
                removed,
                added,
            });
        }
    }

    if updates.is_empty() {
        return Ok(BranchMirrorStats::default());
    }

    let mut stats = BranchMirrorStats::default();
    let mut tx = crate::db_retry::begin_immediate(search_pool).await?;
    for u in &updates {
        sqlx::query("UPDATE file_metadata SET branches = ?1 WHERE file_id = ?2")
            .bind(&u.new_json)
            .bind(u.file_id)
            .execute(&mut *tx)
            .await?;
        stats.rows_resynced += 1;
        stats.tags_removed += u.removed;
        stats.tags_added += u.added;
    }
    tx.commit().await?;

    if stats.tags_removed > 0 || stats.tags_added > 0 {
        info!(
            "[branch_mirror] resynced {} file_metadata row(s) to authority: {} stale tag(s) removed, {} missing tag(s) added",
            stats.rows_resynced, stats.tags_removed, stats.tags_added
        );
    } else {
        warn!(
            "[branch_mirror] {} row(s) differed but no tags changed — unexpected",
            stats.rows_resynced
        );
    }
    Ok(stats)
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---- target_branches (pure) -------------------------------------------

    #[test]
    fn target_removes_stale_tag_when_mirror_wider() {
        // Authority dropped `main`; mirror still carries it → remove it.
        let authority = vec!["feat/x".to_string()];
        let current = vec!["feat/x".to_string(), "main".to_string()];
        assert_eq!(
            target_branches(&authority, &current),
            Some(vec!["feat/x".to_string()])
        );
    }

    #[test]
    fn target_adds_missing_tag_when_mirror_narrower() {
        let authority = vec!["feat/x".to_string(), "main".to_string()];
        let current = vec!["feat/x".to_string()];
        assert_eq!(
            target_branches(&authority, &current),
            Some(vec!["feat/x".to_string(), "main".to_string()])
        );
    }

    #[test]
    fn target_none_when_in_sync_regardless_of_order() {
        let authority = vec!["main".to_string(), "feat/x".to_string()];
        let current = vec!["feat/x".to_string(), "main".to_string()];
        assert_eq!(target_branches(&authority, &current), None);
    }

    #[test]
    fn target_none_on_empty_authority_never_wipes() {
        // Guard: an empty authority must NOT rewrite the mirror to [].
        let current = vec!["main".to_string()];
        assert_eq!(target_branches(&[], &current), None);
    }

    // ---- reconcile_file_metadata_branches (integration) -------------------

    async fn search_db_in_temp() -> (tempfile::TempDir, SearchDbManager) {
        let dir = tempfile::tempdir().unwrap();
        let db = SearchDbManager::new(dir.path().join("search.db"))
            .await
            .unwrap();
        (dir, db)
    }

    /// Minimal state.db stand-in: only `tracked_files.(file_id, branches)` is
    /// read by the reconcile.
    async fn state_pool_with(rows: &[(i64, &str)]) -> SqlitePool {
        let pool = SqlitePool::connect("sqlite::memory:").await.unwrap();
        sqlx::query("CREATE TABLE tracked_files (file_id INTEGER PRIMARY KEY, branches TEXT)")
            .execute(&pool)
            .await
            .unwrap();
        for (id, branches) in rows {
            sqlx::query("INSERT INTO tracked_files (file_id, branches) VALUES (?1, ?2)")
                .bind(id)
                .bind(branches)
                .execute(&pool)
                .await
                .unwrap();
        }
        pool
    }

    async fn seed_fm(db: &SearchDbManager, file_id: i64, branches_json: &str) {
        // Seed a file_metadata row directly (no code_lines / FTS5 trigram — this
        // reconcile only touches the `branches` column), then set the branches
        // array to the drifted state under test.
        sqlx::query(crate::code_lines_schema::UPSERT_FILE_METADATA_SQL)
            .bind(file_id)
            .bind("tenant-a")
            .bind("main")
            .bind(format!("/repo/file{file_id}.rs"))
            .bind(None::<&str>)
            .bind(None::<&str>)
            .bind(None::<&str>)
            .bind(None::<i64>)
            .bind(0_i64)
            .execute(db.pool())
            .await
            .unwrap();
        sqlx::query("UPDATE file_metadata SET branches = ?1 WHERE file_id = ?2")
            .bind(branches_json)
            .bind(file_id)
            .execute(db.pool())
            .await
            .unwrap();
    }

    async fn fm_branches(db: &SearchDbManager, file_id: i64) -> Vec<String> {
        let raw: String =
            sqlx::query_scalar("SELECT branches FROM file_metadata WHERE file_id = ?1")
                .bind(file_id)
                .fetch_one(db.pool())
                .await
                .unwrap();
        serde_json::from_str(&raw).unwrap()
    }

    #[tokio::test]
    async fn reconcile_removes_stale_wider_and_adds_missing_narrower() {
        let (_d, db) = search_db_in_temp().await;
        // file 1: mirror WIDER (stale `main` the authority dropped).
        seed_fm(&db, 1, r#"["feat/x","main"]"#).await;
        // file 2: mirror NARROWER (missing `main` the authority holds).
        seed_fm(&db, 2, r#"["feat/x"]"#).await;
        // file 3: already in sync (order differs) → untouched.
        seed_fm(&db, 3, r#"["main","feat/x"]"#).await;
        let state = state_pool_with(&[
            (1, r#"["feat/x"]"#),
            (2, r#"["feat/x","main"]"#),
            (3, r#"["feat/x","main"]"#),
        ])
        .await;

        let stats = reconcile_file_metadata_branches(&db, &state).await.unwrap();

        assert_eq!(stats.rows_resynced, 2);
        assert_eq!(stats.tags_removed, 1);
        assert_eq!(stats.tags_added, 1);
        assert_eq!(fm_branches(&db, 1).await, vec!["feat/x".to_string()]);
        assert_eq!(
            fm_branches(&db, 2).await,
            vec!["feat/x".to_string(), "main".to_string()]
        );
        // Idempotent: a second pass is a no-op.
        let again = reconcile_file_metadata_branches(&db, &state).await.unwrap();
        assert_eq!(again.rows_resynced, 0);
    }

    #[tokio::test]
    async fn reconcile_empty_authority_never_wipes_mirror() {
        let (_d, db) = search_db_in_temp().await;
        seed_fm(&db, 1, r#"["main"]"#).await;
        // Authority row exists but its branch set is empty ([]): must be treated
        // as unknown, leaving the mirror intact rather than blanking it.
        let state = state_pool_with(&[(1, "[]")]).await;

        let stats = reconcile_file_metadata_branches(&db, &state).await.unwrap();

        assert_eq!(stats.rows_resynced, 0);
        assert_eq!(fm_branches(&db, 1).await, vec!["main".to_string()]);
    }

    #[tokio::test]
    async fn reconcile_skips_file_ids_absent_from_authority() {
        // A file_metadata row with no tracked_files row is an orphan owned by
        // orphan_gc — this pass must leave it alone.
        let (_d, db) = search_db_in_temp().await;
        seed_fm(&db, 1, r#"["main","stale"]"#).await;
        let state = state_pool_with(&[(2, r#"["main"]"#)]).await; // no row for file 1

        let stats = reconcile_file_metadata_branches(&db, &state).await.unwrap();

        assert_eq!(stats.rows_resynced, 0);
        assert_eq!(
            fm_branches(&db, 1).await,
            vec!["main".to_string(), "stale".to_string()]
        );
    }
}
