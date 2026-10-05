//! Branch-membership candidates and worktree discovery: which paths a branch
//! still owes, and when a linked worktree gets reconciled.

use std::path::Path;

use crate::allowed_extensions::AllowedExtensions;
use crate::queue_operations::QueueManager;

use super::db::fetch_paths_missing_branch;
use super::tests::{create_test_pool, insert_tracked_file, insert_watch_folder, setup_tables};
use super::WorktreeDiscovery;

/// fetch_paths_missing_branch selects files tracked under any branch but NOT the
/// target branch, regardless of WHICH other branch tags them (event-independent).
#[tokio::test]
async fn test_fetch_paths_missing_branch_selects_untagged() {
    let pool = create_test_pool().await;
    setup_tables(&pool).await;
    insert_watch_folder(&pool, "w1", "t1", "/tmp/project").await;
    insert_tracked_file(&pool, "w1", &["main"], "h_a", "src/a.rs").await; // missing feat
    insert_tracked_file(&pool, "w1", &["main", "feat"], "h_b", "src/b.rs").await; // tagged
    insert_tracked_file(&pool, "w1", &["dev-clean"], "h_c", "src/c.rs").await; // missing feat

    let mut paths = fetch_paths_missing_branch(&pool, "w1", "feat")
        .await
        .unwrap();
    paths.sort();
    assert_eq!(paths, vec!["src/a.rs".to_string(), "src/c.rs".to_string()]);
}

/// A path has one row per content generation and a branch holds one of them.
/// Live 2026-10-05: asking per ROW reported ~900 paths per branch as missing on
/// every scan — each also had another branch's version — and every one was
/// re-enqueued and skipped as unchanged, forever.
#[tokio::test]
async fn test_fetch_paths_missing_branch_is_judged_per_path() {
    let pool = create_test_pool().await;
    setup_tables(&pool).await;
    insert_watch_folder(&pool, "w1", "t1", "/tmp/project").await;
    // Two versions of a.rs: main holds one, feat the other.
    insert_tracked_file(&pool, "w1", &["main"], "h_a1", "src/a.rs").await;
    insert_tracked_file(&pool, "w1", &["feat"], "h_a2", "src/a.rs").await;
    // b.rs exists only on main.
    insert_tracked_file(&pool, "w1", &["main"], "h_b", "src/b.rs").await;

    let feat = fetch_paths_missing_branch(&pool, "w1", "feat")
        .await
        .unwrap();
    assert_eq!(
        feat,
        vec!["src/b.rs".to_string()],
        "feat holds a version of a.rs"
    );
    let main = fetch_paths_missing_branch(&pool, "w1", "main")
        .await
        .unwrap();
    assert!(
        main.is_empty(),
        "main holds a version of every path: {main:?}"
    );
    let mut fresh = fetch_paths_missing_branch(&pool, "w1", "fresh")
        .await
        .unwrap();
    fresh.sort();
    assert_eq!(fresh, vec!["src/a.rs".to_string(), "src/b.rs".to_string()]);
}

/// Register a linked worktree of `main_repo` on `branch`, holding `files`, the
/// way `git worktree add` lays it out (admin dir + gitlink file).
fn add_worktree(main_repo: &Path, name: &str, branch: &str, files: &[&str]) {
    let wt = main_repo.join(".claude").join("worktrees").join(name);
    for f in files {
        let p = wt.join(f);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(&p, b"fn a() {}").unwrap();
    }
    let admin = main_repo.join(".git").join("worktrees").join(name);
    std::fs::create_dir_all(&admin).unwrap();
    std::fs::write(admin.join("gitdir"), format!("{}/.git\n", wt.display())).unwrap();
    std::fs::write(admin.join("HEAD"), format!("ref: refs/heads/{branch}\n")).unwrap();
    std::fs::write(wt.join(".git"), format!("gitdir: {}\n", admin.display())).unwrap();
}

async fn queued_branches(pool: &sqlx::SqlitePool) -> Vec<String> {
    sqlx::query_scalar("SELECT branch FROM unified_queue WHERE tenant_id = 't1' ORDER BY branch")
        .fetch_all(pool)
        .await
        .unwrap()
}

/// Live 2026-10-04/05: worktrees created between two tenant scans waited for the
/// next one (a daemon restart, 15 h later). Discovery reconciles a worktree that
/// appears after start-up once it is stable, and only once.
#[tokio::test]
async fn test_discovery_reconciles_a_worktree_created_after_start() {
    let tmp = tempfile::tempdir().unwrap();
    let main_repo = tmp.path().join("main");
    std::fs::create_dir_all(main_repo.join(".git")).unwrap();
    let pool = create_test_pool().await;
    setup_tables(&pool).await;
    let main_str = main_repo.to_string_lossy().to_string();
    insert_watch_folder(&pool, "w1", "t1", &main_str).await;
    insert_tracked_file(&pool, "w1", &["main"], "h_a", "src/a.rs").await;
    // Present at start: the start-up tenant scan reconciles this one.
    add_worktree(&main_repo, "wt-old", "old", &["src/a.rs"]);

    let qm = QueueManager::new(pool.clone());
    let ext = AllowedExtensions::default();
    let mut discovery = WorktreeDiscovery::default();
    assert_eq!(
        discovery.tick(&pool, &qm, &ext).await,
        0,
        "first sight: seed only"
    );

    add_worktree(&main_repo, "wt-new", "feat", &["src/a.rs"]);
    assert_eq!(
        discovery.tick(&pool, &qm, &ext).await,
        0,
        "just appeared: its checkout may still be running"
    );
    assert_eq!(
        discovery.tick(&pool, &qm, &ext).await,
        1,
        "stable: reconciled"
    );
    assert_eq!(queued_branches(&pool).await, vec!["feat".to_string()]);

    assert_eq!(discovery.tick(&pool, &qm, &ext).await, 0, "only once");
    assert_eq!(queued_branches(&pool).await, vec!["feat".to_string()]);
}
