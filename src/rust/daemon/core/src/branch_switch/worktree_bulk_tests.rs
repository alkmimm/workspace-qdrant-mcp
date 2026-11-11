//! Worktree reconcile: content the index already holds is tagged in bulk by
//! hash; only content no branch has indexed goes per file.

use std::path::Path;

use wqm_common::hashing::compute_bytes_hash;

use crate::allowed_extensions::AllowedExtensions;
use crate::queue_operations::QueueManager;

use super::reconcile_worktree_branches;
use super::tests::{create_test_pool, insert_tracked_file, insert_watch_folder, setup_tables};

/// Register a leaf linked worktree on `branch` under `<main>/.claude/worktrees/<name>`.
fn add_linked_worktree(main_repo: &Path, name: &str, branch: &str) -> std::path::PathBuf {
    let wt = main_repo.join(".claude").join("worktrees").join(name);
    std::fs::create_dir_all(wt.join("src")).unwrap();
    let admin = main_repo.join(".git").join("worktrees").join(name);
    std::fs::create_dir_all(&admin).unwrap();
    std::fs::write(admin.join("gitdir"), format!("{}/.git\n", wt.display())).unwrap();
    std::fs::write(admin.join("HEAD"), format!("ref: refs/heads/{branch}\n")).unwrap();
    std::fs::write(wt.join(".git"), format!("gitdir: {}\n", admin.display())).unwrap();
    wt
}

/// A new agent worktree whose files are (a) identical to the trunk, (b)
/// content only a sibling branch indexed (not in the main tree — the branch
/// was created from another base), (c) edited, (d) brand new. (a) and (b) are
/// tagged by ONE bulk op certified by their content hash; only (c) and (d)
/// cost a per-file item.
#[tokio::test]
async fn a_new_worktree_tags_already_indexed_content_in_bulk() {
    let tmp = tempfile::tempdir().unwrap();
    let main_repo = tmp.path().join("main");
    std::fs::create_dir_all(main_repo.join(".git")).unwrap();
    std::fs::write(main_repo.join(".git/HEAD"), "ref: refs/heads/main\n").unwrap();
    std::fs::create_dir_all(main_repo.join("src")).unwrap();
    std::fs::write(main_repo.join("src/same.rs"), b"fn same() {}").unwrap();
    std::fs::write(main_repo.join("src/edited.rs"), b"fn v1() {}").unwrap();

    let wt = add_linked_worktree(&main_repo, "wt-x", "agent/x");
    std::fs::write(wt.join("src/same.rs"), b"fn same() {}").unwrap();
    std::fs::write(wt.join("src/sibling.rs"), b"fn sibling() {}").unwrap();
    std::fs::write(wt.join("src/edited.rs"), b"fn v2() {}").unwrap();
    std::fs::write(wt.join("src/new.rs"), b"fn brand_new() {}").unwrap();

    let pool = create_test_pool().await;
    setup_tables(&pool).await;
    let main_str = main_repo.to_string_lossy().to_string();
    insert_watch_folder(&pool, "w1", "t1", &main_str).await;
    let h = |b: &[u8]| compute_bytes_hash(b);
    insert_tracked_file(&pool, "w1", &["main"], &h(b"fn same() {}"), "src/same.rs").await;
    insert_tracked_file(&pool, "w1", &["main"], &h(b"fn v1() {}"), "src/edited.rs").await;
    // Indexed only through a sibling branch; the main tree has no copy.
    insert_tracked_file(
        &pool,
        "w1",
        &["release/platform"],
        &h(b"fn sibling() {}"),
        "src/sibling.rs",
    )
    .await;

    let qm = QueueManager::new(pool.clone());
    let n = reconcile_worktree_branches(
        &pool,
        &qm,
        "w1",
        "t1",
        "projects",
        &main_str,
        &AllowedExtensions::default(),
    )
    .await;
    assert_eq!(n, 4, "2 in bulk + 2 per file");

    let rows: Vec<(String, String, String)> = sqlx::query_as(
        "SELECT item_type, op, payload_json FROM unified_queue WHERE tenant_id = 't1' \
         ORDER BY item_type, payload_json",
    )
    .fetch_all(&pool)
    .await
    .unwrap();
    let files: Vec<&str> = rows
        .iter()
        .filter(|r| r.0 == "file")
        .map(|r| r.2.as_str())
        .collect();
    assert_eq!(files.len(), 2, "{rows:?}");
    assert!(
        files.iter().any(|p| p.contains("src/edited.rs")),
        "{files:?}"
    );
    assert!(files.iter().any(|p| p.contains("src/new.rs")), "{files:?}");

    let bulk: Vec<serde_json::Value> = rows
        .iter()
        .filter(|r| r.0 == "tenant")
        .map(|r| serde_json::from_str(&r.2).unwrap())
        .collect();
    assert_eq!(bulk.len(), 1, "one bulk op: {rows:?}");
    let spec = &bulk[0]["branch_membership"];
    assert_eq!(spec["branch"], "agent/x");
    let mut tagged: Vec<(String, String)> = spec["paths"]
        .as_array()
        .unwrap()
        .iter()
        .zip(spec["hashes"].as_array().unwrap())
        .map(|(p, h)| (p.as_str().unwrap().into(), h.as_str().unwrap().into()))
        .collect();
    tagged.sort();
    assert_eq!(
        tagged,
        vec![
            ("src/same.rs".to_string(), h(b"fn same() {}")),
            ("src/sibling.rs".to_string(), h(b"fn sibling() {}")),
        ]
    );
    assert!(spec.get("from_branch").is_none(), "hash mode: {spec}");
}

/// Insert a pending `File/Add` queued by an earlier reconcile pass.
async fn insert_old_worktree_add(pool: &sqlx::SqlitePool, id: &str, path: &str, meta: &str) {
    sqlx::query(
        "INSERT INTO unified_queue (queue_id, item_type, op, tenant_id, collection, status, \
         created_at, branch, file_path, idempotency_key, metadata) \
         VALUES (?1, 'file', 'add', 't1', 'projects', 'pending', '2020-01-01T00:00:00.000Z', \
                 'agent/x', ?2, ?1, ?3)",
    )
    .bind(id)
    .bind(path)
    .bind(meta)
    .execute(pool)
    .await
    .unwrap();
}

/// A reconcile replaces the per-file adds an earlier pass queued for the
/// branch (the backlog planned before the hash bulk existed) with a fresh
/// plan: the already-indexed file now goes in bulk. The tip follower's staged
/// item is not the reconcile's and stays.
#[tokio::test]
async fn a_reconcile_replans_the_branch_backlog_in_bulk() {
    let tmp = tempfile::tempdir().unwrap();
    let main_repo = tmp.path().join("main");
    std::fs::create_dir_all(main_repo.join(".git")).unwrap();
    std::fs::write(main_repo.join(".git/HEAD"), "ref: refs/heads/main\n").unwrap();
    std::fs::create_dir_all(main_repo.join("src")).unwrap();
    std::fs::write(main_repo.join("src/same.rs"), b"fn same() {}").unwrap();
    let wt = add_linked_worktree(&main_repo, "wt-x", "agent/x");
    std::fs::write(wt.join("src/same.rs"), b"fn same() {}").unwrap();

    let pool = create_test_pool().await;
    setup_tables(&pool).await;
    let main_str = main_repo.to_string_lossy().to_string();
    insert_watch_folder(&pool, "w1", "t1", &main_str).await;
    let hash = compute_bytes_hash(b"fn same() {}");
    insert_tracked_file(&pool, "w1", &["main"], &hash, "src/same.rs").await;
    insert_old_worktree_add(
        &pool,
        "old",
        "src/same.rs",
        r#"{"worktree_membership":true}"#,
    )
    .await;
    insert_old_worktree_add(
        &pool,
        "staged",
        "src/tip.rs",
        r#"{"worktree_membership":true,"read_root":"/stage","git_stage":true}"#,
    )
    .await;

    let qm = QueueManager::new(pool.clone());
    reconcile_worktree_branches(
        &pool,
        &qm,
        "w1",
        "t1",
        "projects",
        &main_str,
        &AllowedExtensions::default(),
    )
    .await;

    let rows: Vec<(String, String)> = sqlx::query_as(
        "SELECT queue_id, item_type FROM unified_queue WHERE tenant_id = 't1' ORDER BY item_type",
    )
    .fetch_all(&pool)
    .await
    .unwrap();
    let ids: Vec<&str> = rows.iter().map(|r| r.0.as_str()).collect();
    assert!(
        !ids.contains(&"old"),
        "the old per-file add is replaced: {rows:?}"
    );
    assert!(
        ids.contains(&"staged"),
        "the follower's item stays: {rows:?}"
    );
    assert_eq!(
        rows.iter().filter(|r| r.1 == "tenant").count(),
        1,
        "the file now goes in one bulk op: {rows:?}"
    );
    assert_eq!(rows.len(), 2, "{rows:?}");
}
