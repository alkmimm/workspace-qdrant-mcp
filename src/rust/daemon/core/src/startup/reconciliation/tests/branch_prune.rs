//! Tests for `branch_prune::prune_orphaned_branches`.
//!
//! Verifies that documents for branches no longer present in git are enqueued
//! for deletion, while documents for live branches are left untouched — and
//! that an unreadable/non-git path is skipped rather than wiped.

use std::path::Path;
use std::sync::Arc;

use git2::{Repository, Signature};
use tempfile::TempDir;

use crate::queue_operations::QueueManager;

use super::super::branch_prune::{covered_by_other_live_generation, prune_orphaned_branches};
use super::{create_test_pool, setup_schema};

/// Initialise a git repo with one commit, then create the named local branches.
fn init_repo(dir: &Path, extra_branches: &[&str]) {
    let repo = Repository::init(dir).expect("git init");
    let sig = Signature::now("Test", "test@example.com").unwrap();
    let tree_id = {
        let mut index = repo.index().unwrap();
        index.write_tree().unwrap()
    };
    let tree = repo.find_tree(tree_id).unwrap();
    let commit_id = repo
        .commit(Some("HEAD"), &sig, &sig, "init", &tree, &[])
        .unwrap();
    let commit = repo.find_commit(commit_id).unwrap();
    for b in extra_branches {
        repo.branch(b, &commit, true).unwrap();
    }
}

async fn insert_watch_folder(pool: &sqlx::SqlitePool, watch_id: &str, tenant: &str, path: &str) {
    sqlx::query(
        "INSERT INTO watch_folders (watch_id, path, collection, tenant_id, created_at, updated_at) \
         VALUES (?1, ?2, 'projects', ?3, '2025-01-01T00:00:00Z', '2025-01-01T00:00:00Z')",
    )
    .bind(watch_id)
    .bind(path)
    .bind(tenant)
    .execute(pool)
    .await
    .unwrap();
}

async fn insert_tracked_file(
    pool: &sqlx::SqlitePool,
    watch_id: &str,
    branch: &str,
    relative_path: &str,
) {
    // Post-v41 the branch lives in a `branches` JSON array. These tests model
    // each (branch, relative_path) as a distinct content-row, so give every row
    // a unique `file_hash` — otherwise the UNIQUE(watch, relative_path, file_hash)
    // constraint would collapse the same path on two branches into one row.
    let branches = serde_json::to_string(&[branch]).unwrap();
    let file_hash = format!("hash-{branch}-{relative_path}");
    sqlx::query(
        "INSERT INTO tracked_files \
         (watch_folder_id, branches, file_mtime, file_hash, relative_path, collection, created_at, updated_at) \
         VALUES (?1, ?2, '2025-01-01T00:00:00Z', ?4, ?3, 'projects', \
         '2025-01-01T00:00:00Z', '2025-01-01T00:00:00Z')",
    )
    .bind(watch_id)
    .bind(&branches)
    .bind(relative_path)
    .bind(&file_hash)
    .execute(pool)
    .await
    .unwrap();
}

async fn file_delete_count(pool: &sqlx::SqlitePool) -> i64 {
    sqlx::query_scalar(
        "SELECT COUNT(*) FROM unified_queue WHERE item_type = 'file' AND op = 'delete'",
    )
    .fetch_one(pool)
    .await
    .unwrap()
}

/// `metadata` of the enqueued `file|delete` for a given path (stage 3 stamping).
async fn delete_metadata_for(pool: &sqlx::SqlitePool, relative_path: &str) -> Option<String> {
    sqlx::query_scalar(
        "SELECT metadata FROM unified_queue \
         WHERE item_type = 'file' AND op = 'delete' AND file_path = ?1",
    )
    .bind(relative_path)
    .fetch_optional(pool)
    .await
    .unwrap()
    .flatten()
}

#[tokio::test]
async fn prunes_genuine_orphan_feature_branch() {
    let pool = create_test_pool().await;
    setup_schema(&pool).await;
    let qm = Arc::new(QueueManager::new(pool.clone()));

    let repo_dir = TempDir::new().unwrap();
    init_repo(repo_dir.path(), &["keep-a", "keep-b"]);
    let repo_path = repo_dir.path().to_str().unwrap();

    insert_watch_folder(&pool, "w1", "t1", repo_path).await;
    // Live branches (present in git) — must be preserved. keep-a is the largest
    // tracked branch, so it is the corpus (never pruned regardless).
    insert_tracked_file(&pool, "w1", "keep-a", "src/a.rs").await;
    insert_tracked_file(&pool, "w1", "keep-a", "src/b.rs").await;
    insert_tracked_file(&pool, "w1", "keep-a", "src/c.rs").await;
    insert_tracked_file(&pool, "w1", "keep-b", "src/a.rs").await;
    // A genuine deleted feature branch: absent from git, a minor offshoot (fewer
    // files than the corpus), not a default name → pruned.
    insert_tracked_file(&pool, "w1", "feature/gone", "src/a.rs").await;
    insert_tracked_file(&pool, "w1", "feature/gone", "src/d.rs").await;

    let stats = prune_orphaned_branches(&pool, &qm).await.expect("prune");

    assert_eq!(stats.branches_pruned, 1, "only 'feature/gone' is orphaned");
    assert_eq!(stats.files_enqueued, 2, "2 files on 'feature/gone'");
    assert_eq!(
        file_delete_count(&pool).await,
        2,
        "exactly the 2 'feature/gone' files enqueued; live branches untouched"
    );
    // Stage 3 (#224): of those two, only `src/a.rs` is also held by the live
    // keep-a/keep-b generations — it is covered, so its stale generation can be
    // removed. `src/d.rs` exists ONLY on the dead branch: deleting it would drop
    // the file's last index entry, so it stays uncovered (preserved).
    assert_eq!(stats.files_covered, 1, "only src/a.rs is covered");
    assert!(
        delete_metadata_for(&pool, "src/a.rs")
            .await
            .expect("a.rs delete enqueued")
            .contains(r#""covered_by_live_generation":true"#),
        "covered generation must carry the stage-3 flag"
    );
    let d_meta = delete_metadata_for(&pool, "src/d.rs")
        .await
        .expect("d.rs delete enqueued");
    assert!(
        !d_meta.contains("covered_by_live_generation"),
        "uncovered generation must keep the plain marker (preserve guard intact), got {d_meta}"
    );
}

/// The pure stage-3 predicate: a generation is covered only when a DIFFERENT
/// generation of the same path carries a live branch.
#[test]
fn covered_by_other_live_generation_ignores_the_row_itself() {
    // Another generation (7) is live → this stale one (42) is covered.
    assert!(covered_by_other_live_generation(Some(&vec![7]), 42));
    assert!(covered_by_other_live_generation(Some(&vec![7, 42]), 42));
    // The ONLY live generation is this row itself → not covered by another.
    // (Cannot happen for a single-dead-tag row, but the predicate must not
    // count a row as its own cover.)
    assert!(!covered_by_other_live_generation(Some(&vec![42]), 42));
    // No live generation at all for the path → the mislabeled-corpus case.
    assert!(!covered_by_other_live_generation(Some(&vec![]), 42));
    assert!(!covered_by_other_live_generation(None, 42));
}

/// A path whose ONLY generations sit on dead branches must never be reported as
/// covered — the corpus-wipe protection the preserve guard exists for.
#[tokio::test]
async fn dead_only_path_is_never_covered() {
    let pool = create_test_pool().await;
    setup_schema(&pool).await;
    let qm = Arc::new(QueueManager::new(pool.clone()));

    let repo_dir = TempDir::new().unwrap();
    init_repo(repo_dir.path(), &["live-main"]);
    let repo_path = repo_dir.path().to_str().unwrap();

    insert_watch_folder(&pool, "w1", "t1", repo_path).await;
    // Corpus on a live branch (also the largest → the primary guard protects it).
    for f in ["a", "b", "c"] {
        insert_tracked_file(&pool, "w1", "live-main", &format!("src/{f}.rs")).await;
    }
    // Two dead-branch generations of a path that NO live generation holds.
    insert_tracked_file(&pool, "w1", "dead-1", "src/only-here.rs").await;
    insert_tracked_file(&pool, "w1", "dead-2", "src/only-here.rs").await;

    let stats = prune_orphaned_branches(&pool, &qm).await.expect("prune");

    assert_eq!(stats.branches_pruned, 2, "both dead branches pruned");
    assert_eq!(stats.files_enqueued, 2);
    assert_eq!(
        stats.files_covered, 0,
        "no live generation serves src/only-here.rs — must not be marked covered"
    );
    assert!(
        !delete_metadata_for(&pool, "src/only-here.rs")
            .await
            .expect("delete enqueued")
            .contains("covered_by_live_generation"),
        "a dead-only path must keep the preserve guard"
    );
}

/// Regression for the bws-engineer / compress-mcp incident: a project's corpus
/// mislabeled under a non-existent branch must NEVER be deleted. The largest
/// tracked branch is protected even when absent from git.
#[tokio::test]
async fn never_prunes_largest_branch_even_if_absent_from_git() {
    let pool = create_test_pool().await;
    setup_schema(&pool).await;
    let qm = Arc::new(QueueManager::new(pool.clone()));

    let repo_dir = TempDir::new().unwrap();
    init_repo(repo_dir.path(), &["dev-clean"]);
    let repo_path = repo_dir.path().to_str().unwrap();

    insert_watch_folder(&pool, "w1", "t1", repo_path).await;
    // Corpus mislabeled under "ghost-default" (NOT a git branch), the largest set.
    for f in ["a", "b", "c", "d", "e"] {
        insert_tracked_file(&pool, "w1", "ghost-default", &format!("src/{f}.rs")).await;
    }
    // A small live branch present in git.
    insert_tracked_file(&pool, "w1", "dev-clean", "src/a.rs").await;

    let stats = prune_orphaned_branches(&pool, &qm).await.expect("prune");

    assert_eq!(stats.branches_pruned, 0, "largest branch is never pruned");
    assert_eq!(file_delete_count(&pool).await, 0, "no deletes enqueued");
}

/// A branch literally named `main`/`master` is never pruned even when git has no
/// such branch — the exact shape of the incident (content under fallback "main").
#[tokio::test]
async fn never_prunes_main_or_master_even_if_absent() {
    let pool = create_test_pool().await;
    setup_schema(&pool).await;
    let qm = Arc::new(QueueManager::new(pool.clone()));

    let repo_dir = TempDir::new().unwrap();
    init_repo(repo_dir.path(), &["dev-clean"]); // git default + dev-clean; no "main"
    let repo_path = repo_dir.path().to_str().unwrap();

    insert_watch_folder(&pool, "w1", "t1", repo_path).await;
    // "main" is absent from git but is a default name AND would be the corpus —
    // doubly protected. Add a larger live branch so it is NOT the largest, to
    // prove the name guard alone protects it.
    insert_tracked_file(&pool, "w1", "main", "src/a.rs").await;
    insert_tracked_file(&pool, "w1", "dev-clean", "src/a.rs").await;
    insert_tracked_file(&pool, "w1", "dev-clean", "src/b.rs").await;
    insert_tracked_file(&pool, "w1", "dev-clean", "src/c.rs").await;

    let stats = prune_orphaned_branches(&pool, &qm).await.expect("prune");

    assert_eq!(stats.branches_pruned, 0, "'main' is name-protected");
    assert_eq!(file_delete_count(&pool).await, 0, "no deletes enqueued");
}

#[tokio::test]
async fn skips_non_git_path_without_pruning() {
    let pool = create_test_pool().await;
    setup_schema(&pool).await;
    let qm = Arc::new(QueueManager::new(pool.clone()));

    // A real directory that is NOT a git repo — list_all_branches errors, so the
    // project must be skipped entirely (never prune on uncertain ground truth).
    let plain_dir = TempDir::new().unwrap();
    insert_watch_folder(&pool, "w1", "t1", plain_dir.path().to_str().unwrap()).await;
    insert_tracked_file(&pool, "w1", "some-branch", "src/a.rs").await;

    let stats = prune_orphaned_branches(&pool, &qm).await.expect("prune");

    assert_eq!(stats.branches_pruned, 0, "non-git path must be skipped");
    assert_eq!(file_delete_count(&pool).await, 0, "nothing enqueued");
}
