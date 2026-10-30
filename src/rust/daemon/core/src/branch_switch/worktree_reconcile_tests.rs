//! Linked-worktree reconcile: the shared baseline, new-on-branch and divergent
//! sets `reconcile_worktree_branches` enqueues for a worktree branch.

use crate::allowed_extensions::AllowedExtensions;
use crate::queue_operations::QueueManager;

use super::reconcile_worktree_branches;
use super::tests::{create_test_pool, insert_tracked_file, insert_watch_folder, setup_tables};

/// B1: a linked worktree's branch tags the shared baseline via the MAIN
/// watch_folder. Exercises the full path: enumerate `.git/worktrees/*` → fold
/// the recorded path → enqueue a `File/Add` on the worktree branch flagged
/// `worktree_membership` (so the process-time restamp keeps the worktree branch
/// instead of rewriting it to the main HEAD). A detached-HEAD worktree is
/// skipped; a file present in the worktree tree but missing the branch is the
/// only candidate enqueued.
#[tokio::test]
async fn test_reconcile_worktree_branches_tags_baseline() {
    let tmp = tempfile::tempdir().unwrap();

    // Main repo with a real `.git` directory.
    let main_repo = tmp.path().join("main");
    let main_git = main_repo.join(".git");
    std::fs::create_dir_all(&main_git).unwrap();
    // The shared baseline is read from the MAIN tree, so the file must be there.
    std::fs::create_dir_all(main_repo.join("src")).unwrap();
    std::fs::write(main_repo.join("src/a.rs"), b"fn a() {}").unwrap();

    // A linked worktree on branch "feat", UNDER the main root (the `/batch`
    // layout `.claude/worktrees/<name>`), whose tree holds the tracked file.
    let wt = main_repo.join(".claude").join("worktrees").join("wt-feat");
    std::fs::create_dir_all(wt.join("src")).unwrap();
    std::fs::write(wt.join("src/a.rs"), b"fn a() {}").unwrap();
    let admin = main_git.join("worktrees").join("wt-feat");
    std::fs::create_dir_all(&admin).unwrap();
    std::fs::write(admin.join("gitdir"), format!("{}/.git\n", wt.display())).unwrap();
    std::fs::write(admin.join("HEAD"), "ref: refs/heads/feat\n").unwrap();
    // The gitlink that makes this a LEAF worktree root. `is_leaf_worktree_root`
    // requires `<wt>/.git` to be a FILE — that is what tells a real worktree
    // apart from the `.claude/worktrees` CONTAINER directory, whose walk was the
    // phantom-storm bug. Without it the reconcile correctly skips this fixture.
    std::fs::write(wt.join(".git"), format!("gitdir: {}\n", admin.display())).unwrap();

    // A second linked worktree in detached HEAD — must be skipped entirely.
    let wt_det = main_repo
        .join(".claude")
        .join("worktrees")
        .join("wt-detached");
    std::fs::create_dir_all(wt_det.join("src")).unwrap();
    std::fs::write(wt_det.join("src/a.rs"), b"fn a() {}").unwrap();
    let admin_det = main_git.join("worktrees").join("wt-detached");
    std::fs::create_dir_all(&admin_det).unwrap();
    std::fs::write(
        admin_det.join("gitdir"),
        format!("{}/.git\n", wt_det.display()),
    )
    .unwrap();
    std::fs::write(
        admin_det.join("HEAD"),
        "0123456789abcdef0123456789abcdef01234567\n",
    )
    .unwrap();
    // Deliberately NO gitlink here. `reconcile_worktree_branches` bails on
    // `let Some(branch) = wt.branch else { continue }` BEFORE it ever reaches
    // `is_leaf_worktree_root`, so a detached worktree cannot exercise the leaf
    // check — writing one would be dead fixture code implying an ordering the
    // loop does not have.

    let pool = create_test_pool().await;
    setup_tables(&pool).await;
    let main_str = main_repo.to_string_lossy().to_string();
    insert_watch_folder(&pool, "w1", "t1", &main_str).await;
    // Baseline file tracked under the MAIN folder, tagged only `main`.
    insert_tracked_file(&pool, "w1", &["main"], "h_a", "src/a.rs").await;

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
    assert_eq!(
        n, 1,
        "only the branch worktree tags the baseline (detached skipped)"
    );

    let rows: Vec<(String, String, String, Option<String>)> = sqlx::query_as(
        "SELECT op, branch, payload_json, metadata FROM unified_queue WHERE tenant_id = 't1'",
    )
    .fetch_all(&pool)
    .await
    .unwrap();
    assert_eq!(
        rows.len(),
        1,
        "exactly one enqueue, from the 'feat' worktree"
    );
    assert_eq!(rows[0].0, "add"); // Add op → cross-branch dedup fast-path, no re-embed
    assert_eq!(rows[0].1, "feat"); // tagged under the WORKTREE's branch
    assert!(rows[0].2.contains("src/a.rs"));
    // The `worktree_membership` flag tells the processor to keep 'feat' (skip the
    // #224 restamp that would otherwise rewrite it to the main HEAD).
    let meta = rows[0].3.as_deref().unwrap_or("");
    assert!(
        meta.contains("worktree_membership"),
        "metadata flags worktree_membership: {meta}"
    );
    assert!(
        !meta.contains("read_root"),
        "the baseline reads from the main tree (no read_root): {meta}"
    );
}

/// B1.1: a file that exists ONLY on the worktree branch (no `tracked_files`
/// row under the main folder) is discovered by walking the worktree tree and
/// enqueued with a `read_root` so the processor reads its bytes from the
/// worktree. The shared baseline (a path already tracked under main) is enqueued
/// too, but WITHOUT a `read_root` — it reads from the main tree. The two sets
/// stay disjoint: the baseline takes only paths the main tree holds.
#[tokio::test]
async fn test_reconcile_worktree_branches_tags_new_on_branch() {
    let tmp = tempfile::tempdir().unwrap();

    let main_repo = tmp.path().join("main");
    let main_git = main_repo.join(".git");
    std::fs::create_dir_all(&main_git).unwrap();

    // A project .wqmignore at the MAIN root excludes `generated/`. This is what
    // the main-anchored eligibility gate consults — a worktree file under
    // generated/ must be dropped even though the worktree tree has no ignore
    // file of its own.
    std::fs::write(main_repo.join(".wqmignore"), "generated/\n").unwrap();
    std::fs::create_dir_all(main_repo.join("src")).unwrap();
    std::fs::write(main_repo.join("src/a.rs"), b"fn a() {}").unwrap();

    // Worktree on branch "feat": one shared file (tracked under main and in its tree), one
    // file that lives ONLY on this branch (no tracked_files row anywhere), and
    // one branch-only file under generated/ that the main gate must exclude.
    let wt = main_repo.join(".claude").join("worktrees").join("wt-feat");
    std::fs::create_dir_all(wt.join("src")).unwrap();
    std::fs::create_dir_all(wt.join("generated")).unwrap();
    std::fs::write(wt.join("src/a.rs"), b"fn a() {}").unwrap();
    std::fs::write(wt.join("src/only_feat.rs"), b"fn only() {}").unwrap();
    std::fs::write(wt.join("generated/gen.rs"), b"// generated").unwrap();
    // A branch-only cert the extension allowlist rejects (mirrors the main
    // scan): passes the ignore gate but must not be enqueued or churned.
    std::fs::write(wt.join("deploy.pem"), b"-----BEGIN-----").unwrap();
    let admin = main_git.join("worktrees").join("wt-feat");
    std::fs::create_dir_all(&admin).unwrap();
    std::fs::write(admin.join("gitdir"), format!("{}/.git\n", wt.display())).unwrap();
    std::fs::write(admin.join("HEAD"), "ref: refs/heads/feat\n").unwrap();
    // Leaf-worktree gitlink — see `is_leaf_worktree_root`.
    std::fs::write(wt.join(".git"), format!("gitdir: {}\n", admin.display())).unwrap();

    let pool = create_test_pool().await;
    setup_tables(&pool).await;
    let main_str = main_repo.to_string_lossy().to_string();
    insert_watch_folder(&pool, "w1", "t1", &main_str).await;
    // Only src/a.rs is tracked under main — src/only_feat.rs is branch-only.
    insert_tracked_file(&pool, "w1", &["main"], "h_a", "src/a.rs").await;

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
    assert_eq!(
        n, 2,
        "shared baseline (src/a.rs) + new-on-branch (src/only_feat.rs); generated/gen.rs excluded"
    );

    let rows: Vec<(String, String, String, Option<String>)> = sqlx::query_as(
        "SELECT op, branch, payload_json, metadata FROM unified_queue WHERE tenant_id = 't1'",
    )
    .fetch_all(&pool)
    .await
    .unwrap();
    assert_eq!(rows.len(), 2, "two enqueues from the 'feat' worktree");
    for r in &rows {
        assert_eq!(
            r.0, "add",
            "Add op → dedup fast-path / novel-content ingest"
        );
        assert_eq!(r.1, "feat", "tagged under the WORKTREE branch");
        assert!(
            r.3.as_deref().unwrap_or("").contains("worktree_membership"),
            "both items flag worktree_membership: {:?}",
            r.3
        );
    }

    // The shared baseline reads from the MAIN tree → NO read_root.
    let baseline = rows
        .iter()
        .find(|r| r.2.contains("src/a.rs"))
        .expect("baseline enqueue present");
    assert!(
        !baseline.3.as_deref().unwrap_or("").contains("read_root"),
        "shared baseline must not carry read_root: {:?}",
        baseline.3
    );

    // The branch-only file reads from the WORKTREE tree → carries read_root.
    let new_on_branch = rows
        .iter()
        .find(|r| r.2.contains("only_feat.rs"))
        .expect("new-on-branch enqueue present");
    let meta = new_on_branch.3.as_deref().unwrap_or("");
    assert!(
        meta.contains("read_root"),
        "new-on-branch carries read_root: {meta}"
    );
    assert!(
        meta.contains("wt-feat"),
        "read_root points at the worktree root: {meta}"
    );

    // The main-anchored eligibility gate must have dropped generated/gen.rs
    // (excluded by the main .wqmignore) — a worktree branch never indexes files
    // the main scan omits.
    assert!(
        !rows.iter().any(|r| r.2.contains("generated/gen.rs")),
        "generated/gen.rs must be excluded by the main-anchored gate"
    );
    // The extension allowlist must have dropped deploy.pem (would otherwise be
    // enqueued only to skip at the process guard, re-churning every scan).
    assert!(
        !rows.iter().any(|r| r.2.contains("deploy.pem")),
        "deploy.pem must be excluded by the extension allowlist"
    );
}

/// B2: a shared file whose content DIVERGES on the worktree branch (git-Modified
/// between the main HEAD and the branch tip) is enqueued with a `read_root` (read
/// from the worktree, indexing the branch's OWN bytes), and the baseline SKIPS it
/// (no read-from-main). Needs a real git repo so the commit-to-commit diff runs.
#[tokio::test]
async fn test_reconcile_worktree_branches_tags_divergent() {
    let tmp = tempfile::tempdir().unwrap();
    let main_repo = tmp.path().join("main");
    std::fs::create_dir_all(&main_repo).unwrap();

    // Real git repo: shared.rs committed on main, then diverges on branch "feat".
    let repo = git2::Repository::init(&main_repo).unwrap();
    let mut cfg = repo.config().unwrap();
    cfg.set_str("user.name", "T").unwrap();
    cfg.set_str("user.email", "t@e").unwrap();
    let sig = repo.signature().unwrap();

    std::fs::write(main_repo.join("shared.rs"), b"fn v1() {}").unwrap();
    let mut idx = repo.index().unwrap();
    idx.add_path(std::path::Path::new("shared.rs")).unwrap();
    idx.write().unwrap();
    let tree1 = repo.find_tree(idx.write_tree().unwrap()).unwrap();
    let c1 = repo
        .commit(Some("HEAD"), &sig, &sig, "c1", &tree1, &[])
        .unwrap();
    let commit1 = repo.find_commit(c1).unwrap();

    // feat: shared.rs diverges (HEAD stays on main → head-vs-feat = {shared.rs}).
    std::fs::write(main_repo.join("shared.rs"), b"fn v2() {}").unwrap();
    let mut idx = repo.index().unwrap();
    idx.add_path(std::path::Path::new("shared.rs")).unwrap();
    idx.write().unwrap();
    let tree2 = repo.find_tree(idx.write_tree().unwrap()).unwrap();
    repo.commit(
        Some("refs/heads/feat"),
        &sig,
        &sig,
        "c2",
        &tree2,
        &[&commit1],
    )
    .unwrap();

    // Linked worktree on branch feat, holding feat's (divergent) content.
    let wt = main_repo.join(".claude").join("worktrees").join("wt-feat");
    std::fs::create_dir_all(&wt).unwrap();
    std::fs::write(wt.join("shared.rs"), b"fn v2() {}").unwrap();
    let admin = main_repo.join(".git").join("worktrees").join("wt-feat");
    std::fs::create_dir_all(&admin).unwrap();
    std::fs::write(admin.join("gitdir"), format!("{}/.git\n", wt.display())).unwrap();
    std::fs::write(admin.join("HEAD"), "ref: refs/heads/feat\n").unwrap();
    // Leaf-worktree gitlink — see `is_leaf_worktree_root`.
    std::fs::write(wt.join(".git"), format!("gitdir: {}\n", admin.display())).unwrap();

    let pool = create_test_pool().await;
    setup_tables(&pool).await;
    let main_str = main_repo.to_string_lossy().to_string();
    insert_watch_folder(&pool, "w1", "t1", &main_str).await;
    // shared.rs tracked under main only — the divergence candidate.
    insert_tracked_file(&pool, "w1", &["main"], "h_shared_main", "shared.rs").await;

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
    assert_eq!(n, 1, "only the divergent shared.rs is enqueued (once)");

    let rows: Vec<(String, String, String, Option<String>)> = sqlx::query_as(
        "SELECT op, branch, payload_json, metadata FROM unified_queue WHERE tenant_id = 't1'",
    )
    .fetch_all(&pool)
    .await
    .unwrap();
    assert_eq!(
        rows.iter().filter(|r| r.2.contains("shared.rs")).count(),
        1,
        "baseline skipped the divergent file — exactly one enqueue"
    );
    let shared = rows
        .iter()
        .find(|r| r.2.contains("shared.rs"))
        .expect("shared.rs enqueued");
    assert_eq!(shared.0, "add");
    assert_eq!(shared.1, "feat", "tagged under the worktree branch");
    let meta = shared.3.as_deref().unwrap_or("");
    assert!(
        meta.contains("read_root"),
        "divergent file reads from the worktree (read_root): {meta}"
    );
    assert!(
        meta.contains("wt-feat"),
        "read_root points at the worktree root: {meta}"
    );
}

/// A non-`projects` collection is never worktree-reconciled (branch tags only
/// exist for projects), and a repo with no linked worktrees is a clean no-op.
#[tokio::test]
async fn test_reconcile_worktree_branches_gated_and_noop() {
    let tmp = tempfile::tempdir().unwrap();
    let main_repo = tmp.path().join("main");
    std::fs::create_dir_all(main_repo.join(".git")).unwrap();

    let pool = create_test_pool().await;
    setup_tables(&pool).await;
    let main_str = main_repo.to_string_lossy().to_string();
    insert_watch_folder(&pool, "w1", "t1", &main_str).await;
    insert_tracked_file(&pool, "w1", &["main"], "h_a", "src/a.rs").await;

    let qm = QueueManager::new(pool.clone());

    // Non-projects collection → gated out.
    let n_lib = reconcile_worktree_branches(
        &pool,
        &qm,
        "w1",
        "t1",
        "libraries",
        &main_str,
        &AllowedExtensions::default(),
    )
    .await;
    assert_eq!(n_lib, 0);

    // Projects but no `.git/worktrees` → nothing to do.
    let n_none = reconcile_worktree_branches(
        &pool,
        &qm,
        "w1",
        "t1",
        "projects",
        &main_str,
        &AllowedExtensions::default(),
    )
    .await;
    assert_eq!(n_none, 0);

    let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM unified_queue")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(count, 0, "no enqueues in either gated/no-op case");
}
