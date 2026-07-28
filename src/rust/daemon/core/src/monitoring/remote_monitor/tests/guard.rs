//! Tests for the issue #299 cascade-rename safety guard.
//!
//! The guard sits in front of every tenant cascade rename the remote/git-state
//! monitors would enqueue. It prevents the silent-zero-search failure where a
//! rename keyed on a SHARED canonical tenant (a worktree sharing the main repo's
//! tenant, whose gitlink made a background check recompute a `local_*` fallback)
//! dragged the main repo's Qdrant points onto a dead worktree's fallback tenant.

use super::*;
use tempfile::TempDir;

const CANONICAL: &str = "367157a01d98";

/// Insert a minimal `watch_folders` row (active, non-archived) for guard tests.
async fn insert_watch(
    pool: &SqlitePool,
    watch_id: &str,
    path: &str,
    tenant_id: &str,
    is_worktree: i64,
) {
    sqlx::query(
        r#"
        INSERT INTO watch_folders (watch_id, path, collection, tenant_id, is_active,
            is_archived, is_git_tracked, is_worktree)
        VALUES (?1, ?2, 'projects', ?3, 1, 0, 1, ?4)
        "#,
    )
    .bind(watch_id)
    .bind(path)
    .bind(tenant_id)
    .bind(is_worktree)
    .execute(pool)
    .await
    .unwrap();
}

/// Read `(is_active, is_archived)` for a row.
async fn row_flags(pool: &SqlitePool, watch_id: &str) -> (i64, i64) {
    sqlx::query_as::<_, (i64, i64)>(
        "SELECT is_active, is_archived FROM watch_folders WHERE watch_id = ?1",
    )
    .bind(watch_id)
    .fetch_one(pool)
    .await
    .unwrap()
}

/// A worktree whose backing path vanished is the exact #299 orphan: prune it
/// (archive) instead of letting it rename the shared canonical tenant.
#[tokio::test]
async fn test_guard_prunes_orphan_worktree() {
    let pool = create_test_database().await;
    let gone = "/definitely/not/a/real/worktree/path-299";
    insert_watch(&pool, "wt", gone, CANONICAL, 1).await;

    let outcome =
        guard_cascade_rename(&pool, "wt", gone, true, CANONICAL, "local_deadbeef00").await;

    assert_eq!(outcome, RenameGuardOutcome::Pruned);
    assert_eq!(
        row_flags(&pool, "wt").await,
        (0, 1),
        "orphan worktree should be archived (is_active=0, is_archived=1)"
    );
}

/// A lingering `local_*` fallback registration whose path is gone is also an
/// orphan and should be pruned.
#[tokio::test]
async fn test_guard_prunes_orphan_local_fallback() {
    let pool = create_test_database().await;
    let gone = "/definitely/not/a/real/path-299b";
    insert_watch(&pool, "orphan", gone, "local_dacc2738defc", 0).await;

    let outcome =
        guard_cascade_rename(&pool, "orphan", gone, false, "local_dacc2738defc", "local_x").await;

    assert_eq!(outcome, RenameGuardOutcome::Pruned);
    assert_eq!(row_flags(&pool, "orphan").await, (0, 1));
}

/// A canonical, non-worktree folder whose path is (transiently) missing must NOT
/// be pruned — a bind-mount hiccup would make every project look gone. Suppress
/// the rename this cycle, but leave the row active.
#[tokio::test]
async fn test_guard_blocks_missing_canonical_without_pruning() {
    let pool = create_test_database().await;
    let gone = "/definitely/not/a/real/path-299c";
    insert_watch(&pool, "main", gone, CANONICAL, 0).await;

    let outcome = guard_cascade_rename(&pool, "main", gone, false, CANONICAL, "local_x").await;

    assert_eq!(outcome, RenameGuardOutcome::Block);
    assert_eq!(
        row_flags(&pool, "main").await,
        (1, 0),
        "a transiently-missing canonical folder must stay active (not pruned)"
    );
}

/// A worktree that still exists on disk must never rename its (shared) tenant —
/// its content is indexed via the main folder — but it is not an orphan, so it
/// is blocked, not pruned.
#[tokio::test]
async fn test_guard_blocks_live_worktree() {
    let pool = create_test_database().await;
    let temp = TempDir::new().unwrap();
    let path = temp.path().to_str().unwrap();
    insert_watch(&pool, "wt", path, CANONICAL, 1).await;

    let outcome = guard_cascade_rename(&pool, "wt", path, true, CANONICAL, "local_x").await;

    assert_eq!(outcome, RenameGuardOutcome::Block);
    assert_eq!(row_flags(&pool, "wt").await, (1, 0));
}

/// The core guard: a canonical tenant SHARED by another watch folder must never
/// be demoted onto a `local_*` fallback (that cascade would drag the co-tenant's
/// points). Blocked, not pruned.
#[tokio::test]
async fn test_guard_blocks_shared_canonical_demotion() {
    let pool = create_test_database().await;
    let main = TempDir::new().unwrap();
    let other = TempDir::new().unwrap();
    // Two live folders sharing the SAME canonical tenant.
    insert_watch(&pool, "main", main.path().to_str().unwrap(), CANONICAL, 0).await;
    insert_watch(&pool, "other", other.path().to_str().unwrap(), CANONICAL, 0).await;

    let outcome = guard_cascade_rename(
        &pool,
        "other",
        other.path().to_str().unwrap(),
        false,
        CANONICAL,
        "local_x",
    )
    .await;

    assert_eq!(outcome, RenameGuardOutcome::Block);
}

/// A standalone canonical repo (tenant NOT shared) that legitimately dropped its
/// git remote SHOULD still be allowed to move to its `local_*` id — no co-tenant
/// to endanger.
#[tokio::test]
async fn test_guard_allows_standalone_canonical_to_local() {
    let pool = create_test_database().await;
    let temp = TempDir::new().unwrap();
    let path = temp.path().to_str().unwrap();
    insert_watch(&pool, "solo", path, CANONICAL, 0).await;

    let outcome = guard_cascade_rename(&pool, "solo", path, false, CANONICAL, "local_x").await;

    assert_eq!(outcome, RenameGuardOutcome::Proceed);
    assert_eq!(row_flags(&pool, "solo").await, (1, 0));
}

/// A normal remote-URL change (repo transferred to a new org) stays under a
/// git-derived tenant on both sides — never touches the `local_*` guard — and
/// must proceed.
#[tokio::test]
async fn test_guard_allows_normal_remote_change() {
    let pool = create_test_database().await;
    let temp = TempDir::new().unwrap();
    let path = temp.path().to_str().unwrap();
    insert_watch(&pool, "repo", path, "old_canonical_id", 0).await;

    let outcome =
        guard_cascade_rename(&pool, "repo", path, false, "old_canonical_id", "new_canonical_id")
            .await;

    assert_eq!(outcome, RenameGuardOutcome::Proceed);
}
