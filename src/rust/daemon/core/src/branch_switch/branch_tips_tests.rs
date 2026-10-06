//! A branch nobody has checked out, following its tip.

use std::collections::HashMap;
use std::path::Path;

use crate::allowed_extensions::AllowedExtensions;
use crate::queue_operations::QueueManager;

use super::branch_tips::{plan_branch, TipPlan};
use super::tests::{create_test_pool, insert_tracked_file, insert_watch_folder, setup_tables};
use super::BranchTipFollower;

pub(super) fn commit(
    repo: &git2::Repository,
    files: &[(&str, &str)],
    parent: Option<git2::Oid>,
) -> git2::Oid {
    let mut builder = repo.treebuilder(None).unwrap();
    for (path, content) in files {
        let blob = repo.blob(content.as_bytes()).unwrap();
        builder.insert(path, blob, 0o100644).unwrap();
    }
    let tree = repo.find_tree(builder.write().unwrap()).unwrap();
    let sig = git2::Signature::now("t", "t@t").unwrap();
    let parents: Vec<git2::Commit> = parent
        .map(|p| repo.find_commit(p).unwrap())
        .into_iter()
        .collect();
    let parent_refs: Vec<&git2::Commit> = parents.iter().collect();
    repo.commit(None, &sig, &sig, "c", &tree, &parent_refs)
        .unwrap()
}

/// The content identity the index stores for these one-line test files.
pub(super) fn h(content: &str) -> String {
    wqm_common::hashing::compute_content_hash(content)
}

fn tagged(pairs: &[(&str, &[&str])]) -> HashMap<String, Vec<String>> {
    pairs
        .iter()
        .map(|(rel, contents)| (rel.to_string(), contents.iter().map(|c| h(c)).collect()))
        .collect()
}

#[test]
fn a_tip_plan_reingests_what_moved_and_untags_what_left() {
    let dir = tempfile::tempdir().unwrap();
    let repo = git2::Repository::init(dir.path()).unwrap();
    let c1 = commit(
        &repo,
        &[("a.rs", "1"), ("b.rs", "1"), ("gone.rs", "1")],
        None,
    );
    let c2 = commit(
        &repo,
        &[("a.rs", "2"), ("b.rs", "1"), ("new.rs", "1")],
        Some(c1),
    );
    let held = tagged(&[("a.rs", &["1"]), ("b.rs", &["1"]), ("gone.rs", &["1"])]);

    // First sight: only what the branch is tagged on can be judged.
    assert_eq!(
        plan_branch(dir.path(), c2, None, &held),
        Some(TipPlan {
            reingest: vec![("a.rs".to_string(), b"2".to_vec())],
            untag: vec!["gone.rs".to_string()],
        })
    );
    // A known move also brings in what it added.
    let moved = plan_branch(dir.path(), c2, Some(c1), &held).unwrap();
    assert_eq!(
        moved.reingest,
        vec![
            ("a.rs".to_string(), b"2".to_vec()),
            ("new.rs".to_string(), b"1".to_vec())
        ]
    );
    assert_eq!(moved.untag, vec!["gone.rs".to_string()]);
}

#[test]
fn a_path_holding_the_tips_version_is_current_whatever_else_is_tagged() {
    let dir = tempfile::tempdir().unwrap();
    let repo = git2::Repository::init(dir.path()).unwrap();
    let tip = commit(&repo, &[("a.rs", "2")], None);
    // Shadowed debris (the old version still tagged too) is the overlap
    // strip's business: re-ingesting would never converge.
    let held = tagged(&[("a.rs", &["1", "2"])]);
    assert_eq!(
        plan_branch(dir.path(), tip, None, &held),
        Some(TipPlan::default())
    );
    // A tip git cannot read is no answer, not an empty one.
    let unknown = git2::Oid::from_str("0123456789abcdef0123456789abcdef01234567").unwrap();
    assert_eq!(plan_branch(dir.path(), unknown, None, &held), None);
}

/// Live 2026-10-05: bws-engineer `master` moved inside a worktree since removed,
/// and 508 of its tagged rows kept versions its tip no longer had — nothing
/// followed a branch nobody had checked out.
#[tokio::test]
async fn the_follower_moves_a_branch_nobody_has_checked_out_once_per_tip() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("repo");
    std::fs::create_dir_all(&root).unwrap();
    let repo = git2::Repository::init(&root).unwrap();
    let c1 = commit(
        &repo,
        &[("a.rs", "1"), ("b.rs", "1"), ("gone.rs", "1")],
        None,
    );
    let c2 = commit(&repo, &[("a.rs", "2"), ("b.rs", "1")], Some(c1));
    repo.reference("refs/heads/main", c1, true, "").unwrap();
    repo.reference("refs/heads/feat", c2, true, "").unwrap();
    repo.set_head("refs/heads/main").unwrap(); // the main folder is on main

    let pool = create_test_pool().await;
    setup_tables(&pool).await;
    let root_str = root.to_string_lossy().to_string();
    insert_watch_folder(&pool, "w1", "t1", &root_str).await;
    // main is checked out: even a stale-looking tag is its checkout's business.
    insert_tracked_file(&pool, "w1", &["main"], "not-the-tip", "a.rs").await;
    // feat has no checkout and still holds c1's versions of a.rs and gone.rs.
    insert_tracked_file(&pool, "w1", &["feat"], &h("1"), "a.rs").await;
    insert_tracked_file(&pool, "w1", &["feat"], &h("1"), "gone.rs").await;
    insert_tracked_file(&pool, "w1", &["feat", "main"], &h("1"), "b.rs").await;
    // A branch git no longer has is the branch prune's, not this follower's.
    insert_tracked_file(&pool, "w1", &["deleted"], &h("x"), "c.rs").await;

    let stage = tempfile::tempdir().unwrap();
    let qm = QueueManager::new(pool.clone());
    let ext = AllowedExtensions::default();
    let mut follower = BranchTipFollower::new(stage.path().to_path_buf());
    assert_eq!(
        follower.tick(&pool, &qm, &ext).await,
        2,
        "a.rs re-ingested, gone.rs untagged"
    );

    let rows: Vec<(String, String, String, Option<String>)> =
        sqlx::query_as("SELECT op, branch, payload_json, metadata FROM unified_queue")
            .fetch_all(&pool)
            .await
            .unwrap();
    assert!(
        rows.iter().all(|r| r.1 == "feat"),
        "only feat moves: {rows:?}"
    );
    let add = rows.iter().find(|r| r.0 == "add").expect("re-ingest");
    assert!(add.2.contains("a.rs"));
    let meta: serde_json::Value = serde_json::from_str(add.3.as_deref().unwrap()).unwrap();
    assert_eq!(meta["git_stage"], true, "no language server for git bytes");
    assert_eq!(
        meta["worktree_membership"], true,
        "keeps feat, not main's HEAD"
    );
    let read_root = meta["read_root"].as_str().unwrap();
    assert!(read_root.starts_with(&*stage.path().to_string_lossy()));
    assert_eq!(
        std::fs::read(Path::new(read_root).join("a.rs")).unwrap(),
        b"2",
        "the tip's bytes, staged for the queue"
    );
    let delete = rows.iter().find(|r| r.0 == "delete").expect("untag");
    assert!(delete.2.contains("gone.rs"));
    assert!(delete.3.as_deref().unwrap().contains("branch_prune"));

    // The same tip again: nothing.
    assert_eq!(follower.tick(&pool, &qm, &ext).await, 0);

    // feat moves on before the queue has run. new.rs comes with the move; a.rs
    // at c3 cannot enter while c2's item for the same (branch, path) is pending
    // (the per-file unique index), so this tip is not settled.
    let c3 = commit(
        &repo,
        &[("a.rs", "3"), ("b.rs", "1"), ("new.rs", "1")],
        Some(c2),
    );
    repo.reference("refs/heads/feat", c3, true, "").unwrap();
    assert_eq!(follower.tick(&pool, &qm, &ext).await, 1, "new.rs only");

    // The queue runs: feat's a.rs now holds c2's version, new.rs is tagged.
    sqlx::query("DELETE FROM unified_queue WHERE op = 'add'")
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("UPDATE tracked_files SET file_hash = ?1 WHERE relative_path = 'a.rs' AND branches = '[\"feat\"]'")
        .bind(h("2"))
        .execute(&pool)
        .await
        .unwrap();
    insert_tracked_file(&pool, "w1", &["feat"], &h("1"), "new.rs").await;

    // The unsettled tip is planned again: an intermediate version is never the
    // resting state.
    assert_eq!(follower.tick(&pool, &qm, &ext).await, 1, "a.rs at c3");
    assert_eq!(follower.tick(&pool, &qm, &ext).await, 0, "settled");
}
