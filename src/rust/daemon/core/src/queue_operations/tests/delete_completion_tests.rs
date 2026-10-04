//! F-036 post-completion cleanup is scoped to the completed delete's branch.

use super::*;
use crate::tracked_files_schema::CREATE_TRACKED_FILES_V41_SQL;

async fn setup(db_name: &str, watch_root: &str) -> (tempfile::TempDir, SqlitePool, QueueManager) {
    let temp_dir = tempdir().unwrap();
    let db_path = temp_dir.path().join(db_name);
    let config = QueueConnectionConfig::with_database_path(&db_path);
    let pool = config.create_pool().await.unwrap();
    apply_sql_script(&pool, include_str!("../../schema/watch_folders_schema.sql"))
        .await
        .unwrap();
    sqlx::query(CREATE_TRACKED_FILES_V41_SQL)
        .execute(&pool)
        .await
        .unwrap();
    let manager = QueueManager::new(pool.clone());
    manager.init_unified_queue().await.unwrap();
    sqlx::query(
        "INSERT INTO watch_folders (watch_id, path, collection, tenant_id, created_at, updated_at) \
         VALUES ('wf-1', ?1, 'projects', 'tenant-a', '2025-01-01T00:00:00Z', '2025-01-01T00:00:00Z')",
    )
    .bind(watch_root)
    .execute(&pool)
    .await
    .unwrap();
    (temp_dir, pool, manager)
}

async fn track(pool: &SqlitePool, rel: &str, branches: &str, hash: &str) {
    sqlx::query(
        "INSERT INTO tracked_files \
         (watch_folder_id, relative_path, branches, needs_reconcile, file_mtime, file_hash, \
          collection, created_at, updated_at) \
         VALUES ('wf-1', ?1, ?2, 0, '2025-01-01T00:00:00Z', ?3, \
                 'projects', '2025-01-01T00:00:00Z', '2025-01-01T00:00:00Z')",
    )
    .bind(rel)
    .bind(branches)
    .bind(hash)
    .execute(pool)
    .await
    .unwrap();
}

async fn complete_delete(
    pool: &SqlitePool,
    manager: &QueueManager,
    qid: &str,
    rel: &str,
    branch: &str,
) {
    sqlx::query(
        "INSERT INTO unified_queue \
         (queue_id, idempotency_key, item_type, op, tenant_id, collection, \
          status, branch, payload_json, metadata, file_path, created_at, updated_at) \
         VALUES (?1, ?1, 'file', 'delete', 'tenant-a', 'projects', \
                 'in_progress', ?2, '{}', '{}', ?3, \
                 '2025-01-01T00:00:00Z', '2025-01-01T00:00:00Z')",
    )
    .bind(qid)
    .bind(branch)
    .bind(rel)
    .execute(pool)
    .await
    .unwrap();
    assert!(manager.delete_unified_item(qid).await.unwrap());
}

async fn branch_sets(pool: &SqlitePool, rel: &str) -> Vec<String> {
    let mut v: Vec<String> =
        sqlx::query_scalar("SELECT branches FROM tracked_files WHERE relative_path = ?1")
            .bind(rel)
            .fetch_all(pool)
            .await
            .unwrap();
    v.sort();
    v
}

/// Regression (2026-10-03): a completed delete on `develop` used to remove
/// EVERY row of the path because the file was missing from the main folder,
/// erasing the generation another branch still held. Only the delete's own
/// branch's leftover goes now.
#[tokio::test]
async fn completed_delete_removes_only_its_own_branch_leftover() {
    let root = tempdir().unwrap();
    let root_str = root.path().to_string_lossy().to_string();
    let (_db, pool, manager) = setup("scoped.db", &root_str).await;

    track(
        &pool,
        "app/lib/actions/endpoint.ts",
        "[\"develop\"]",
        "h-develop",
    )
    .await;
    track(
        &pool,
        "app/lib/actions/endpoint.ts",
        "[\"feat/x\"]",
        "h-feat",
    )
    .await;

    complete_delete(
        &pool,
        &manager,
        "qid-dev",
        "app/lib/actions/endpoint.ts",
        "develop",
    )
    .await;

    assert_eq!(
        branch_sets(&pool, "app/lib/actions/endpoint.ts").await,
        vec!["[\"feat/x\"]".to_string()],
        "the feat/x generation must survive a develop delete"
    );
}

/// A row shared with another branch is never removed by the cleanup — the
/// handler strips the tag; the cleanup only sweeps single-branch leftovers.
#[tokio::test]
async fn completed_delete_keeps_rows_other_branches_still_hold() {
    let root = tempdir().unwrap();
    let root_str = root.path().to_string_lossy().to_string();
    let (_db, pool, manager) = setup("shared.db", &root_str).await;

    track(&pool, "src/shared.ts", "[\"develop\",\"feat/x\"]", "h1").await;
    complete_delete(&pool, &manager, "qid-shared", "src/shared.ts", "develop").await;

    assert_eq!(branch_sets(&pool, "src/shared.ts").await.len(), 1);
}

/// The on-disk orphan guard looks in the delete branch's own checkout: a
/// worktree-only file is still "on disk" for the worktree's branch, so its
/// row is preserved even though the main folder never had it.
#[tokio::test]
async fn completed_delete_preserves_worktree_file_present_in_its_checkout() {
    let root = tempdir().unwrap();
    let main = root.path().join("repo");
    let git = main.join(".git");
    std::fs::create_dir_all(&git).unwrap();
    std::fs::write(git.join("HEAD"), "ref: refs/heads/develop\n").unwrap();
    let wt = main.join(".claude/worktrees/wt-x");
    std::fs::create_dir_all(wt.join("app/lib/actions")).unwrap();
    std::fs::write(wt.join(".git"), "gitdir: x\n").unwrap();
    std::fs::write(wt.join("app/lib/actions/sync.ts"), "export {}\n").unwrap();
    let admin = git.join("worktrees/wt-x");
    std::fs::create_dir_all(&admin).unwrap();
    std::fs::write(admin.join("gitdir"), format!("{}/.git\n", wt.display())).unwrap();
    std::fs::write(admin.join("HEAD"), "ref: refs/heads/feat/x\n").unwrap();

    let main_str = main.to_string_lossy().to_string();
    let (_db, pool, manager) = setup("worktree.db", &main_str).await;
    track(&pool, "app/lib/actions/sync.ts", "[\"feat/x\"]", "h-sync").await;

    complete_delete(
        &pool,
        &manager,
        "qid-wt",
        "app/lib/actions/sync.ts",
        "feat/x",
    )
    .await;

    assert_eq!(
        branch_sets(&pool, "app/lib/actions/sync.ts").await.len(),
        1,
        "the worktree file is on disk in feat/x's checkout; its row stays"
    );
}
