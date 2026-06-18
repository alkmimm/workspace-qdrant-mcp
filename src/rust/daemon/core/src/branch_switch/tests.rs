//! Tests for the branch switch protocol.

use std::time::Duration;

use sqlx::sqlite::SqlitePoolOptions;
use sqlx::SqlitePool;

use crate::queue_operations::QueueManager;
use crate::tracked_files_schema::{
    CREATE_TRACKED_FILES_V41_INDEXES_SQL, CREATE_TRACKED_FILES_V41_SQL,
};
use crate::unified_queue_schema::{
    QueueOperation, CREATE_UNIFIED_QUEUE_INDEXES_SQL, CREATE_UNIFIED_QUEUE_SQL,
};
use crate::watch_folders_schema;

use super::db::{fetch_unchanged_relative_paths, update_last_commit_hash};
use super::queue::{enqueue_file_op, enqueue_unchanged_file};
use super::types::BranchSwitchStats;

async fn create_test_pool() -> SqlitePool {
    SqlitePoolOptions::new()
        .max_connections(1)
        .acquire_timeout(Duration::from_secs(5))
        .connect("sqlite::memory:")
        .await
        .expect("Failed to create in-memory SQLite pool")
}

async fn setup_tables(pool: &SqlitePool) {
    sqlx::query("PRAGMA foreign_keys = ON")
        .execute(pool)
        .await
        .unwrap();
    sqlx::query(watch_folders_schema::CREATE_WATCH_FOLDERS_SQL)
        .execute(pool)
        .await
        .unwrap();
    sqlx::query(CREATE_TRACKED_FILES_V41_SQL)
        .execute(pool)
        .await
        .unwrap();
    for idx in CREATE_TRACKED_FILES_V41_INDEXES_SQL {
        sqlx::query(idx).execute(pool).await.unwrap();
    }
    sqlx::query(CREATE_UNIFIED_QUEUE_SQL)
        .execute(pool)
        .await
        .unwrap();
    for idx in CREATE_UNIFIED_QUEUE_INDEXES_SQL {
        sqlx::query(idx).execute(pool).await.unwrap();
    }
}

async fn insert_watch_folder(pool: &SqlitePool, watch_id: &str, tenant_id: &str, path: &str) {
    sqlx::query(
        "INSERT INTO watch_folders (watch_id, path, collection, tenant_id, enabled, is_archived, created_at, updated_at)
         VALUES (?1, ?2, 'projects', ?3, 1, 0, '2025-01-01T00:00:00Z', '2025-01-01T00:00:00Z')"
    )
    .bind(watch_id)
    .bind(path)
    .bind(tenant_id)
    .execute(pool).await.unwrap();
}

async fn insert_tracked_file(
    pool: &SqlitePool,
    watch_id: &str,
    branches_list: &[&str],
    file_hash: &str,
    relative_path: &str,
) {
    let base_point = wqm_common::hashing::compute_base_point("t1", relative_path, file_hash);
    let branches = serde_json::to_string(branches_list).unwrap();
    sqlx::query(
        "INSERT INTO tracked_files (watch_folder_id, relative_path, branches, file_mtime, file_hash,
         collection, base_point, created_at, updated_at)
         VALUES (?1, ?2, ?3, '2025-01-01T00:00:00Z', ?4, 'projects', ?5, '2025-01-01T00:00:00Z', '2025-01-01T00:00:00Z')"
    )
    .bind(watch_id)
    .bind(relative_path)
    .bind(&branches)
    .bind(file_hash)
    .bind(base_point)
    .execute(pool).await.unwrap();
}

#[test]
fn test_branch_switch_stats_default() {
    let stats = BranchSwitchStats::default();
    assert_eq!(stats.enqueued_unchanged, 0);
    assert_eq!(stats.enqueued_changed, 0);
    assert_eq!(stats.enqueued_added, 0);
    assert_eq!(stats.enqueued_deleted, 0);
    assert_eq!(stats.errors, 0);
}

/// All old-branch files are dedup candidates when the new branch has no rows
/// yet (the `git checkout -b feature` case: same commit, empty diff).
#[tokio::test]
async fn test_fetch_unchanged_returns_all_when_new_branch_empty() {
    let pool = create_test_pool().await;
    setup_tables(&pool).await;
    insert_watch_folder(&pool, "w1", "t1", "/tmp/project").await;

    insert_tracked_file(&pool, "w1", &["main"], "hash_a", "src/a.rs").await;
    insert_tracked_file(&pool, "w1", &["main"], "hash_b", "src/b.rs").await;
    insert_tracked_file(&pool, "w1", &["main"], "hash_c", "src/c.rs").await;

    let mut paths = fetch_unchanged_relative_paths(&pool, "w1", "main", "feature")
        .await
        .unwrap();
    paths.sort();
    assert_eq!(
        paths,
        vec![
            "src/a.rs".to_string(),
            "src/b.rs".to_string(),
            "src/c.rs".to_string()
        ]
    );
}

/// Files already tracked on the target branch are excluded — repeated switches
/// stay idempotent and don't re-enqueue already-deduped files.
#[tokio::test]
async fn test_fetch_unchanged_excludes_paths_already_on_new_branch() {
    let pool = create_test_pool().await;
    setup_tables(&pool).await;
    insert_watch_folder(&pool, "w1", "t1", "/tmp/project").await;

    insert_tracked_file(&pool, "w1", &["main"], "hash_b", "src/b.rs").await;
    insert_tracked_file(&pool, "w1", &["main"], "hash_c", "src/c.rs").await;
    // a.rs already deduped onto feature in a prior switch: one content-row with
    // both branches in its set.
    insert_tracked_file(&pool, "w1", &["main", "feature"], "hash_a", "src/a.rs").await;

    let mut paths = fetch_unchanged_relative_paths(&pool, "w1", "main", "feature")
        .await
        .unwrap();
    paths.sort();
    assert_eq!(
        paths,
        vec!["src/b.rs".to_string(), "src/c.rs".to_string()]
    );
}

#[tokio::test]
async fn test_fetch_unchanged_empty_when_old_branch_has_no_files() {
    let pool = create_test_pool().await;
    setup_tables(&pool).await;
    insert_watch_folder(&pool, "w1", "t1", "/tmp/empty").await;

    let paths = fetch_unchanged_relative_paths(&pool, "w1", "main", "dev")
        .await
        .unwrap();
    assert!(paths.is_empty());
}

#[tokio::test]
async fn test_update_last_commit_hash() {
    let pool = create_test_pool().await;
    setup_tables(&pool).await;

    insert_watch_folder(&pool, "w1", "t1", "/tmp/project").await;

    update_last_commit_hash(&pool, "w1", "abc123def456")
        .await
        .unwrap();

    let hash: Option<String> =
        sqlx::query_scalar("SELECT last_commit_hash FROM watch_folders WHERE watch_id = 'w1'")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(hash.as_deref(), Some("abc123def456"));
}

#[tokio::test]
async fn test_enqueue_file_op() {
    let pool = create_test_pool().await;
    setup_tables(&pool).await;
    // Watch folder is needed so the helper can anchor the absolute path.
    insert_watch_folder(&pool, "w1", "t1", "/tmp/project").await;

    let qm = QueueManager::new(pool.clone());

    enqueue_file_op(
        &qm,
        "t1",
        "projects",
        "/tmp/project/src/main.rs",
        QueueOperation::Update,
        "main",
    )
    .await
    .unwrap();

    let count: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM unified_queue WHERE tenant_id = 't1'")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(count, 1);

    let op: String = sqlx::query_scalar("SELECT op FROM unified_queue WHERE tenant_id = 't1'")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(op, "update");
}

/// Unchanged files must be enqueued as `Add` (not `Update`) so the Update
/// pre-flight's non-branch-scoped defensive delete can't wipe the source
/// branch's points before the dedup fast-path scrolls them.
#[tokio::test]
async fn test_enqueue_unchanged_file_uses_add_op() {
    let pool = create_test_pool().await;
    setup_tables(&pool).await;
    insert_watch_folder(&pool, "w1", "t1", "/tmp/project").await;

    let qm = QueueManager::new(pool.clone());

    enqueue_unchanged_file(&qm, "t1", "projects", "src/main.rs", "feature")
        .await
        .unwrap();

    let (op, branch): (String, String) =
        sqlx::query_as("SELECT op, branch FROM unified_queue WHERE tenant_id = 't1'")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(op, "add");
    assert_eq!(branch, "feature");
}
