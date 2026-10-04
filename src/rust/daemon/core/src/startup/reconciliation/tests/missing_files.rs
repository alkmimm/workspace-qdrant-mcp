//! Startup steps 4b/5 judge each branch in its own checkout.

use crate::queue_operations::QueueManager;

use super::super::clean_stale_state;
use super::{create_test_pool, setup_schema};

/// A main repo on `develop` with a linked worktree on `feat/x`; returns
/// (tempdir guard, main root, worktree root).
fn repo_with_worktree() -> (tempfile::TempDir, std::path::PathBuf, std::path::PathBuf) {
    let temp = tempfile::tempdir().unwrap();
    let main = temp.path().join("repo");
    let git = main.join(".git");
    std::fs::create_dir_all(&git).unwrap();
    std::fs::write(git.join("HEAD"), "ref: refs/heads/develop\n").unwrap();
    let wt = main.join(".claude/worktrees/wt-x");
    std::fs::create_dir_all(&wt).unwrap();
    std::fs::write(wt.join(".git"), "gitdir: x\n").unwrap();
    let admin = git.join("worktrees/wt-x");
    std::fs::create_dir_all(&admin).unwrap();
    std::fs::write(admin.join("gitdir"), format!("{}/.git\n", wt.display())).unwrap();
    std::fs::write(admin.join("HEAD"), "ref: refs/heads/feat/x\n").unwrap();
    (temp, main, wt)
}

async fn track(pool: &sqlx::SqlitePool, rel: &str, branches: &str, hash: &str) {
    sqlx::query(
        "INSERT INTO tracked_files \
         (watch_folder_id, relative_path, branches, file_mtime, file_hash, created_at, updated_at) \
         VALUES ('wf1', ?1, ?2, '2025-01-01T00:00:00Z', ?3, \
         '2025-01-01T00:00:00Z', '2025-01-01T00:00:00Z')",
    )
    .bind(rel)
    .bind(branches)
    .bind(hash)
    .execute(pool)
    .await
    .unwrap();
}

/// Regression (emnify, 2026-10-03): a worktree-only file is neither deleted
/// (step 4b) nor dropped (step 5) because the main folder lacks it; a file
/// the main HEAD really lost still is; a branch no checkout has is left to
/// branch pruning.
#[tokio::test]
async fn startup_judges_each_branch_in_its_own_checkout() {
    let pool = create_test_pool().await;
    setup_schema(&pool).await;
    let qm = QueueManager::new(pool.clone());
    let (_guard, main, wt) = repo_with_worktree();

    sqlx::query(
        "INSERT INTO watch_folders (watch_id, path, collection, tenant_id, \
         created_at, updated_at) \
         VALUES ('wf1', ?1, 'projects', 'tenant1', \
         '2025-01-01T00:00:00Z', '2025-01-01T00:00:00Z')",
    )
    .bind(main.to_string_lossy().to_string())
    .execute(&pool)
    .await
    .unwrap();

    std::fs::create_dir_all(wt.join("app/lib/actions")).unwrap();
    std::fs::write(wt.join("app/lib/actions/sync.ts"), "export {}\n").unwrap();
    track(&pool, "app/lib/actions/sync.ts", "[\"feat/x\"]", "h-sync").await;
    track(&pool, "gone-on-develop.ts", "[\"develop\"]", "h-gone").await;
    track(&pool, "old-branch-only.ts", "[\"old/merged\"]", "h-old").await;

    let stats = clean_stale_state(&pool, &qm)
        .await
        .expect("clean_stale_state");

    assert_eq!(
        stats.deletes_enqueued, 1,
        "only the file the main HEAD's checkout lost gets a Delete"
    );
    let branches: Vec<String> = sqlx::query_scalar(
        "SELECT branch FROM unified_queue WHERE op = 'delete' AND item_type = 'file'",
    )
    .fetch_all(&pool)
    .await
    .unwrap();
    assert_eq!(branches, vec!["develop".to_string()]);

    let mut kept: Vec<String> =
        sqlx::query_scalar("SELECT relative_path FROM tracked_files ORDER BY relative_path")
            .fetch_all(&pool)
            .await
            .unwrap();
    kept.sort();
    assert_eq!(
        kept,
        vec![
            "app/lib/actions/sync.ts".to_string(),
            "gone-on-develop.ts".to_string(),
            "old-branch-only.ts".to_string(),
        ],
        "no row is dropped: sync.ts is on disk in its checkout, gone-on-develop.ts \
         has its Delete in flight, old-branch-only.ts has no checkout to judge it"
    );
    assert_eq!(stats.tracked_files_removed, 0);
}
