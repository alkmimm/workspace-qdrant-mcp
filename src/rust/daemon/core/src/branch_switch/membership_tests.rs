//! Branch-membership candidates and worktree discovery: which paths a branch
//! still owes, and when a linked worktree gets reconciled.

use std::path::Path;

use wqm_common::constants::COLLECTION_PROJECTS;

use crate::allowed_extensions::AllowedExtensions;
use crate::queue_operations::QueueManager;

use super::db::fetch_paths_missing_branch;
use super::tests::{create_test_pool, insert_tracked_file, insert_watch_folder, setup_tables};
use super::worktree_discovery::reconcile_linked_worktree;
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
        write_file(&wt, f);
    }
    let admin = main_repo.join(".git").join("worktrees").join(name);
    std::fs::create_dir_all(&admin).unwrap();
    std::fs::write(admin.join("gitdir"), format!("{}/.git\n", wt.display())).unwrap();
    std::fs::write(admin.join("HEAD"), format!("ref: refs/heads/{branch}\n")).unwrap();
    std::fs::write(wt.join(".git"), format!("gitdir: {}\n", admin.display())).unwrap();
}

fn write_file(root: &Path, rel: &str) {
    let p = root.join(rel);
    std::fs::create_dir_all(p.parent().unwrap()).unwrap();
    std::fs::write(&p, b"fn a() {}").unwrap();
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
    write_file(&main_repo, "src/a.rs");
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

/// Live 2026-10-30 (tecsul): 1,012 files that only other worktree branches had
/// indexed were enqueued again on every commit of one worktree. The baseline
/// read them from the main tree, which has no copy, so the tag never landed and
/// the set never settled. They are read from the worktree now; the baseline
/// keeps exactly what the main tree can supply.
#[tokio::test]
async fn test_worktree_only_files_are_read_from_the_worktree_not_the_main_tree() {
    let tmp = tempfile::tempdir().unwrap();
    let main_repo = tmp.path().join("main");
    std::fs::create_dir_all(main_repo.join(".git")).unwrap();
    write_file(&main_repo, "src/a.rs");
    let pool = create_test_pool().await;
    setup_tables(&pool).await;
    let main_str = main_repo.to_string_lossy().to_string();
    insert_watch_folder(&pool, "w1", "t1", &main_str).await;
    insert_tracked_file(&pool, "w1", &["main"], "h_a", "src/a.rs").await;
    // Indexed through another worktree branch; the main tree has no copy.
    insert_tracked_file(&pool, "w1", &["feat-a"], "h_p", "plataforma/p.rs").await;
    // Already tagged on feat-b: nothing owed.
    insert_tracked_file(&pool, "w1", &["feat-b"], "h_q", "plataforma/q.rs").await;
    let files = ["src/a.rs", "plataforma/p.rs", "plataforma/q.rs"];
    add_worktree(&main_repo, "wt-b", "feat-b", &files);
    let wt = crate::git::list_linked_worktrees(&main_repo)
        .into_iter()
        .find(|w| w.branch.as_deref() == Some("feat-b"))
        .unwrap();

    let qm = QueueManager::new(pool.clone());
    let ext = AllowedExtensions::default();
    let enqueued = reconcile_linked_worktree(
        &pool,
        &qm,
        "w1",
        "t1",
        COLLECTION_PROJECTS,
        &main_str,
        &wt,
        &ext,
    )
    .await;

    let rows: Vec<(String, Option<String>)> = sqlx::query_as(
        "SELECT json_extract(payload_json, '$.file_path'), json_extract(metadata, '$.read_root')
         FROM unified_queue WHERE branch = 'feat-b' ORDER BY 1",
    )
    .fetch_all(&pool)
    .await
    .unwrap();
    let wt_root = main_repo
        .join(".claude/worktrees/wt-b")
        .to_string_lossy()
        .to_string();
    assert_eq!(
        rows,
        vec![
            ("plataforma/p.rs".to_string(), Some(wt_root)),
            ("src/a.rs".to_string(), None),
        ],
        "p.rs from the worktree, a.rs from main, q.rs already held"
    );
    assert_eq!(enqueued, 2);
}
