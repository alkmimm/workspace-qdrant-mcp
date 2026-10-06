//! Library-format documents (`.pdf`, `.docx`, …) inside a project folder are
//! stored in the project's `-refs` library. Live 2026-10-05: the one
//! bws-engineer `master` row the tip follower left stale was a `.docx` — every
//! branch_switch path judged eligibility with the project allowlist and
//! enqueued under `projects`, so such a document never followed a branch.

use std::collections::HashSet;

use wqm_common::constants::COLLECTION_PROJECTS;

use crate::allowed_extensions::AllowedExtensions;
use crate::queue_operations::QueueManager;

use super::branch_tips_tests::{commit, h};
use super::handlers::enqueue_unchanged_files;
use super::tests::{create_test_pool, insert_tracked_file, insert_watch_folder, setup_tables};
use super::types::BranchSwitchStats;
use super::worktree_membership::{main_eligibility_gate, worktree_path_eligible};
use super::BranchTipFollower;

#[test]
fn worktree_eligibility_admits_documents_and_refuses_private_keys() {
    let main = tempfile::tempdir().unwrap();
    let root = main.path().to_string_lossy().to_string();
    let gate = main_eligibility_gate(&root);
    let ext = AllowedExtensions::default();
    let eligible = |rel: &str| worktree_path_eligible(&root, &gate, &ext, COLLECTION_PROJECTS, rel);
    assert!(eligible("integrator-api/docs/wiki-cliente/guia.docx"));
    assert!(eligible("docs/manual.pdf"));
    assert!(eligible("src/main.rs"));
    assert!(!eligible("api-service/src/main/resources/ca/server.key"));
}

/// The bulk re-key tags points in the op's collection (`projects`); a
/// document's points are in the library, so it goes file by file and the queue
/// routes it there.
#[tokio::test]
async fn a_branch_switch_rekeys_a_document_file_by_file_into_its_library() {
    use crate::tracked_files_schema::compute_file_hash;

    let pool = create_test_pool().await;
    setup_tables(&pool).await;
    let qm = QueueManager::new(pool.clone());
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    std::fs::write(root.join("guide.docx"), b"PK\x03\x04 doc").unwrap();
    std::fs::write(root.join("lib.rs"), b"fn f() {}\n").unwrap();
    let root_str = root.to_str().unwrap();
    insert_watch_folder(&pool, "w1", "t1", root_str).await;
    for rel in ["guide.docx", "lib.rs"] {
        let hash = compute_file_hash(&root.join(rel)).unwrap();
        insert_tracked_file(&pool, "w1", &["main"], &hash, rel).await;
    }

    let mut stats = BranchSwitchStats::default();
    enqueue_unchanged_files(
        &pool,
        &qm,
        "w1",
        "main",
        "feature",
        "t1",
        COLLECTION_PROJECTS,
        root_str,
        &HashSet::new(),
        &mut stats,
    )
    .await;
    assert_eq!(stats.errors, 0);

    let rows: Vec<(String, String, String, String)> = sqlx::query_as(
        "SELECT op, tenant_id, collection, payload_json FROM unified_queue ORDER BY op",
    )
    .fetch_all(&pool)
    .await
    .unwrap();
    let add = rows.iter().find(|r| r.0 == "add").expect("per-file add");
    assert_eq!((add.1.as_str(), add.2.as_str()), ("t1-refs", "libraries"));
    assert!(add.3.contains("guide.docx"));
    let bulk = rows.iter().find(|r| r.0 == "scan").expect("bulk re-key");
    assert!(bulk.3.contains("lib.rs"));
    assert!(!bulk.3.contains("guide.docx"), "{bulk:?}");
    assert_eq!(rows.len(), 2, "{rows:?}");
}

#[tokio::test]
async fn the_follower_moves_a_document_into_the_projects_library() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("repo");
    std::fs::create_dir_all(&root).unwrap();
    let repo = git2::Repository::init(&root).unwrap();
    let c1 = commit(&repo, &[("guide.docx", "1"), ("server.key", "1")], None);
    let c2 = commit(&repo, &[("guide.docx", "2"), ("server.key", "2")], Some(c1));
    repo.reference("refs/heads/main", c1, true, "").unwrap();
    repo.reference("refs/heads/feat", c2, true, "").unwrap();
    repo.set_head("refs/heads/main").unwrap();

    let pool = create_test_pool().await;
    setup_tables(&pool).await;
    insert_watch_folder(&pool, "w1", "t1", &root.to_string_lossy()).await;
    insert_tracked_file(&pool, "w1", &["feat"], &h("1"), "guide.docx").await;
    // Never indexable; tagged here only to prove the follower refuses it.
    insert_tracked_file(&pool, "w1", &["feat"], &h("1"), "server.key").await;

    let stage = tempfile::tempdir().unwrap();
    let qm = QueueManager::new(pool.clone());
    let ext = AllowedExtensions::default();
    let mut follower = BranchTipFollower::new(stage.path().to_path_buf());
    assert_eq!(
        follower.tick(&pool, &qm, &ext).await,
        1,
        "the document only"
    );

    let rows: Vec<(String, String, String, String, Option<String>)> = sqlx::query_as(
        "SELECT tenant_id, collection, branch, payload_json, metadata FROM unified_queue",
    )
    .fetch_all(&pool)
    .await
    .unwrap();
    assert_eq!(rows.len(), 1, "{rows:?}");
    let (tenant, collection, branch, payload, metadata) = &rows[0];
    assert_eq!(
        (tenant.as_str(), collection.as_str(), branch.as_str()),
        ("t1-refs", "libraries", "feat")
    );
    assert!(payload.contains("guide.docx"));
    let meta: serde_json::Value = serde_json::from_str(metadata.as_deref().unwrap()).unwrap();
    assert_eq!(meta["source_project_id"], "t1");
    assert_eq!(meta["git_stage"], true);
    assert_eq!(meta["worktree_membership"], true);
    assert!(meta["read_root"].as_str().is_some());
}
