//! A worktree branch tagged with the trunk's bytes of a file is re-read once
//! the branch edits that file, even when the main folder's HEAD lacks it.
//!
//! Live 2026-11-24 (tecsul): the main folder sat on a personal branch with no
//! `plataforma/` tree; an agent branch got `ObjectApi.java` tagged with the
//! release's bytes when its worktree was created, then committed an edit. The
//! main-HEAD diff called the file `Added`, never `Modified`; the bulk and the
//! baseline skip paths the branch already holds; so nothing ever re-read it and
//! searches on the branch answered from the release's version, nothing pending.

use git2::{Commit, Oid, Repository, Signature};
use wqm_common::hashing::compute_bytes_hash;

use crate::allowed_extensions::AllowedExtensions;
use crate::queue_operations::QueueManager;

use super::reconcile_worktree_branches;
use super::tests::{create_test_pool, insert_tracked_file, insert_watch_folder, setup_tables};

fn commit(repo: &Repository, refname: &str, files: &[(&str, &str)], parents: &[&Commit]) -> Oid {
    let mut src = repo.treebuilder(None).unwrap();
    for (name, body) in files {
        src.insert(name, repo.blob(body.as_bytes()).unwrap(), 0o100644)
            .unwrap();
    }
    let src = src.write().unwrap();
    let mut root = repo.treebuilder(None).unwrap();
    root.insert("src", src, 0o040000).unwrap();
    let tree = repo.find_tree(root.write().unwrap()).unwrap();
    let sig = Signature::now("t", "t@t").unwrap();
    repo.commit(Some(refname), &sig, &sig, refname, &tree, parents)
        .unwrap()
}

#[tokio::test]
async fn an_edited_file_the_branch_holds_with_the_trunks_bytes_is_reread() {
    let tmp = tempfile::tempdir().unwrap();
    let main_repo = tmp.path().join("main");
    let repo = Repository::init(&main_repo).unwrap();
    repo.config()
        .unwrap()
        .set_str("wqm.trunk", "release")
        .unwrap();
    let c0 = commit(
        &repo,
        "refs/heads/release",
        &[("obj.rs", "fn v1() {}"), ("same.rs", "fn s() {}")],
        &[],
    );
    let c0 = repo.find_commit(c0).unwrap();
    commit(
        &repo,
        "refs/heads/agent/x",
        &[("obj.rs", "fn v2() {}"), ("same.rs", "fn s() {}")],
        &[&c0],
    );
    // The main folder: a personal branch without the trunk's src/ tree.
    let mut legacy = repo.treebuilder(None).unwrap();
    legacy
        .insert("legacy.txt", repo.blob(b"old").unwrap(), 0o100644)
        .unwrap();
    let legacy = repo.find_tree(legacy.write().unwrap()).unwrap();
    let sig = Signature::now("t", "t@t").unwrap();
    repo.commit(Some("refs/heads/mine"), &sig, &sig, "mine", &legacy, &[])
        .unwrap();
    repo.set_head("refs/heads/mine").unwrap();

    // The agent's worktree (registered the way git does), its disk at the tip.
    let wt = main_repo.join(".claude").join("worktrees").join("wt-x");
    std::fs::create_dir_all(wt.join("src")).unwrap();
    let admin = main_repo.join(".git").join("worktrees").join("wt-x");
    std::fs::create_dir_all(&admin).unwrap();
    std::fs::write(admin.join("gitdir"), format!("{}/.git\n", wt.display())).unwrap();
    std::fs::write(admin.join("HEAD"), "ref: refs/heads/agent/x\n").unwrap();
    std::fs::write(wt.join(".git"), format!("gitdir: {}\n", admin.display())).unwrap();
    std::fs::write(wt.join("src/obj.rs"), b"fn v2() {}").unwrap();
    std::fs::write(wt.join("src/same.rs"), b"fn s() {}").unwrap();

    // The index as found live: the branch holds the TRUNK's bytes of obj.rs.
    let pool = create_test_pool().await;
    setup_tables(&pool).await;
    let main_str = main_repo.to_string_lossy().to_string();
    insert_watch_folder(&pool, "w1", "t1", &main_str).await;
    let h = |b: &[u8]| compute_bytes_hash(b);
    let both = ["release", "agent/x"];
    insert_tracked_file(&pool, "w1", &both, &h(b"fn v1() {}"), "src/obj.rs").await;
    insert_tracked_file(&pool, "w1", &both, &h(b"fn s() {}"), "src/same.rs").await;

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

    let rows: Vec<(String, String, Option<String>)> = sqlx::query_as(
        "SELECT payload_json, branch, metadata FROM unified_queue \
         WHERE tenant_id = 't1' AND item_type = 'file'",
    )
    .fetch_all(&pool)
    .await
    .unwrap();
    assert_eq!(n, 1, "only the edited file is owed: {rows:?}");
    assert_eq!(rows.len(), 1, "{rows:?}");
    let (payload, branch, metadata) = &rows[0];
    assert!(payload.contains("src/obj.rs"), "{payload}");
    assert_eq!(branch, "agent/x");
    let meta: serde_json::Value = serde_json::from_str(metadata.as_deref().unwrap()).unwrap();
    assert_eq!(
        meta["read_root"].as_str(),
        Some(wt.to_string_lossy().as_ref()),
        "read from the worktree, so the branch gets its OWN bytes: {meta}"
    );
}
