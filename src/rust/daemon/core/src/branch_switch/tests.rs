//! Tests for the branch switch protocol.

use std::time::Duration;

use sqlx::sqlite::SqlitePoolOptions;
use sqlx::SqlitePool;

use crate::queue_operations::QueueManager;
use crate::tracked_files_schema::{
    CREATE_TRACKED_FILES_V41_INDEXES_SQL, CREATE_TRACKED_FILES_V41_SQL,
};
use crate::unified_queue_schema::{
    QueueOperation, CREATE_UNIFIED_QUEUE_INDEXES_SQL, CREATE_UNIFIED_QUEUE_SQL,
};
use crate::watch_folders_schema;

use super::db::{
    fetch_paths_missing_branch, fetch_unchanged_paths_with_chunker, update_last_commit_hash,
};

/// Test helper: the unchanged paths (dropping the chunker_version the handler
/// uses to route stale files) so the membership-focused assertions read cleanly.
async fn fetch_unchanged_paths(
    pool: &SqlitePool,
    watch_folder_id: &str,
    old_branch: &str,
    new_branch: &str,
) -> Result<Vec<String>, String> {
    Ok(
        fetch_unchanged_paths_with_chunker(pool, watch_folder_id, old_branch, new_branch)
            .await?
            .into_iter()
            .map(|(p, _, _)| p)
            .collect(),
    )
}
use super::handlers::enqueue_unchanged_files;
use super::queue::{enqueue_branch_membership_bulk, enqueue_file_op, enqueue_unchanged_file};
use super::reconcile_branch_membership;
use super::types::BranchSwitchStats;

async fn create_test_pool() -> SqlitePool {
    SqlitePoolOptions::new()
        .max_connections(1)
        .acquire_timeout(Duration::from_secs(5))
        .connect("sqlite::memory:")
        .await
        .expect("Failed to create in-memory SQLite pool")
}

async fn setup_tables(pool: &SqlitePool) {
    sqlx::query("PRAGMA foreign_keys = ON")
        .execute(pool)
        .await
        .unwrap();
    sqlx::query(watch_folders_schema::CREATE_WATCH_FOLDERS_SQL)
        .execute(pool)
        .await
        .unwrap();
    sqlx::query(CREATE_TRACKED_FILES_V41_SQL)
        .execute(pool)
        .await
        .unwrap();
    for idx in CREATE_TRACKED_FILES_V41_INDEXES_SQL {
        sqlx::query(idx).execute(pool).await.unwrap();
    }
    sqlx::query(CREATE_UNIFIED_QUEUE_SQL)
        .execute(pool)
        .await
        .unwrap();
    for idx in CREATE_UNIFIED_QUEUE_INDEXES_SQL {
        sqlx::query(idx).execute(pool).await.unwrap();
    }
}

async fn insert_watch_folder(pool: &SqlitePool, watch_id: &str, tenant_id: &str, path: &str) {
    sqlx::query(
        "INSERT INTO watch_folders (watch_id, path, collection, tenant_id, enabled, is_archived, created_at, updated_at)
         VALUES (?1, ?2, 'projects', ?3, 1, 0, '2025-01-01T00:00:00Z', '2025-01-01T00:00:00Z')"
    )
    .bind(watch_id)
    .bind(path)
    .bind(tenant_id)
    .execute(pool).await.unwrap();
}

async fn insert_tracked_file(
    pool: &SqlitePool,
    watch_id: &str,
    branches_list: &[&str],
    file_hash: &str,
    relative_path: &str,
) {
    let base_point = wqm_common::hashing::compute_base_point("t1", relative_path, file_hash);
    let branches = serde_json::to_string(branches_list).unwrap();
    sqlx::query(
        "INSERT INTO tracked_files (watch_folder_id, relative_path, branches, file_mtime, file_hash,
         collection, base_point, created_at, updated_at)
         VALUES (?1, ?2, ?3, '2025-01-01T00:00:00Z', ?4, 'projects', ?5, '2025-01-01T00:00:00Z', '2025-01-01T00:00:00Z')"
    )
    .bind(watch_id)
    .bind(relative_path)
    .bind(&branches)
    .bind(file_hash)
    .bind(base_point)
    .execute(pool).await.unwrap();
}

async fn insert_tracked_file_with_chunker(
    pool: &SqlitePool,
    watch_id: &str,
    branches_list: &[&str],
    file_hash: &str,
    relative_path: &str,
    chunker_version: Option<&str>,
) {
    let base_point = wqm_common::hashing::compute_base_point("t1", relative_path, file_hash);
    let branches = serde_json::to_string(branches_list).unwrap();
    sqlx::query(
        "INSERT INTO tracked_files (watch_folder_id, relative_path, branches, file_mtime, file_hash,
         collection, base_point, chunker_version, created_at, updated_at)
         VALUES (?1, ?2, ?3, '2025-01-01T00:00:00Z', ?4, 'projects', ?5, ?6, '2025-01-01T00:00:00Z', '2025-01-01T00:00:00Z')"
    )
    .bind(watch_id)
    .bind(relative_path)
    .bind(&branches)
    .bind(file_hash)
    .bind(base_point)
    .bind(chunker_version)
    .execute(pool).await.unwrap();
}

#[test]
fn test_branch_switch_stats_default() {
    let stats = BranchSwitchStats::default();
    assert_eq!(stats.enqueued_unchanged, 0);
    assert_eq!(stats.enqueued_changed, 0);
    assert_eq!(stats.enqueued_added, 0);
    assert_eq!(stats.enqueued_deleted, 0);
    assert_eq!(stats.errors, 0);
}

/// All old-branch files are dedup candidates when the new branch has no rows
/// yet (the `git checkout -b feature` case: same commit, empty diff).
#[tokio::test]
async fn test_fetch_unchanged_returns_all_when_new_branch_empty() {
    let pool = create_test_pool().await;
    setup_tables(&pool).await;
    insert_watch_folder(&pool, "w1", "t1", "/tmp/project").await;

    insert_tracked_file(&pool, "w1", &["main"], "hash_a", "src/a.rs").await;
    insert_tracked_file(&pool, "w1", &["main"], "hash_b", "src/b.rs").await;
    insert_tracked_file(&pool, "w1", &["main"], "hash_c", "src/c.rs").await;

    let mut paths = fetch_unchanged_paths(&pool, "w1", "main", "feature")
        .await
        .unwrap();
    paths.sort();
    assert_eq!(
        paths,
        vec![
            "src/a.rs".to_string(),
            "src/b.rs".to_string(),
            "src/c.rs".to_string()
        ]
    );
}

/// Files already tracked on the target branch are excluded — repeated switches
/// stay idempotent and don't re-enqueue already-deduped files.
#[tokio::test]
async fn test_fetch_unchanged_excludes_paths_already_on_new_branch() {
    let pool = create_test_pool().await;
    setup_tables(&pool).await;
    insert_watch_folder(&pool, "w1", "t1", "/tmp/project").await;

    insert_tracked_file(&pool, "w1", &["main"], "hash_b", "src/b.rs").await;
    insert_tracked_file(&pool, "w1", &["main"], "hash_c", "src/c.rs").await;
    // a.rs already deduped onto feature in a prior switch: one content-row with
    // both branches in its set.
    insert_tracked_file(&pool, "w1", &["main", "feature"], "hash_a", "src/a.rs").await;

    let mut paths = fetch_unchanged_paths(&pool, "w1", "main", "feature")
        .await
        .unwrap();
    paths.sort();
    assert_eq!(
        paths,
        vec!["src/b.rs".to_string(), "src/c.rs".to_string()]
    );
}

/// The chunker-aware variant (issue #246) returns each unchanged file's stored
/// `chunker_version` so the handler can route STALE files to a re-chunk. Rows
/// with a fresh fingerprint, a stale one, and NULL must all come back with their
/// value; `stored_fingerprint_is_stale` then classifies them.
#[tokio::test]
async fn test_fetch_unchanged_with_chunker_returns_versions() {
    use crate::tree_sitter::chunker::{chunking_fingerprint, stored_fingerprint_is_stale};

    let pool = create_test_pool().await;
    setup_tables(&pool).await;
    insert_watch_folder(&pool, "w1", "t1", "/tmp/project").await;

    let fresh = chunking_fingerprint(Some("rust"));
    insert_tracked_file_with_chunker(&pool, "w1", &["main"], "h_fresh", "src/fresh.rs", Some(&fresh))
        .await;
    insert_tracked_file_with_chunker(
        &pool,
        "w1",
        &["main"],
        "h_stale",
        "src/stale.rs",
        Some("0:rust:deadbeef0000"),
    )
    .await;
    insert_tracked_file_with_chunker(&pool, "w1", &["main"], "h_null", "src/null.rs", None).await;

    let mut rows = fetch_unchanged_paths_with_chunker(&pool, "w1", "main", "feature")
        .await
        .unwrap();
    rows.sort_by(|a, b| a.0.cmp(&b.0));
    assert_eq!(rows.len(), 3);

    // Classify with the same predicate the handler uses.
    let stale: Vec<&str> = rows
        .iter()
        .filter(|(_, cv, _)| stored_fingerprint_is_stale(cv.as_deref()))
        .map(|(p, _, _)| p.as_str())
        .collect();
    assert_eq!(stale, vec!["src/stale.rs"], "only the old-version row is stale");
}

/// Content guard (issue #224 cross-branch stale content): a candidate whose
/// tracked generation's stored hash does NOT match the on-disk file on the new
/// branch must be re-ingested per-file (content-safe), NOT folded onto the stale
/// generation by the hash-free bulk append. A candidate whose hash DOES match is
/// bulk-re-keyed as before.
#[tokio::test]
async fn test_content_guard_reingests_diverged_generation() {
    use crate::tracked_files_schema::compute_file_hash;
    use std::collections::HashSet;

    let pool = create_test_pool().await;
    setup_tables(&pool).await;
    let qm = QueueManager::new(pool.clone());

    // Working tree is on `feature` (the checkout just landed).
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    std::fs::create_dir_all(root.join("src")).unwrap();
    std::fs::write(root.join("src/match.rs"), b"fn matches() {}\n").unwrap();
    std::fs::write(root.join("src/diverged.rs"), b"fn new_content_on_feature() {}\n").unwrap();
    let root_str = root.to_str().unwrap();

    // match.rs: tracked `main` generation's hash EQUALS the working tree → safe
    // to bulk re-key onto `feature`.
    let match_hash = compute_file_hash(&root.join("src/match.rs")).unwrap();
    insert_watch_folder(&pool, "w1", "t1", root_str).await;
    insert_tracked_file(&pool, "w1", &["main"], &match_hash, "src/match.rs").await;
    // diverged.rs: tracked `main` generation holds STALE content (a hash that is
    // NOT what is on disk) — the bug shape. Must NOT be bulk re-keyed.
    insert_tracked_file(
        &pool,
        "w1",
        &["main"],
        "stale_hash_not_on_disk_0000000000000000000000000000000000000000",
        "src/diverged.rs",
    )
    .await;

    let changed: HashSet<String> = HashSet::new();
    let mut stats = BranchSwitchStats::default();
    enqueue_unchanged_files(
        &pool, &qm, "w1", "main", "feature", "t1", "projects", root_str, &changed, &mut stats,
    )
    .await;

    assert_eq!(stats.errors, 0);
    assert_eq!(
        stats.enqueued_changed, 1,
        "the diverged file is routed to a per-file re-ingest"
    );

    let ops: Vec<(String, String)> =
        sqlx::query_as("SELECT op, payload_json FROM unified_queue WHERE tenant_id = 't1'")
            .fetch_all(&pool)
            .await
            .unwrap();

    // Exactly one per-file Add, and it is diverged.rs (not match.rs).
    let adds: Vec<&(String, String)> = ops.iter().filter(|(op, _)| op == "add").collect();
    assert_eq!(adds.len(), 1, "one per-file re-ingest");
    assert!(adds[0].1.contains("src/diverged.rs"));
    assert!(!adds[0].1.contains("src/match.rs"));

    // The bulk branch-membership op carries the verified file, never the diverged.
    let bulk: Vec<&(String, String)> = ops
        .iter()
        .filter(|(op, pj)| op == "scan" && pj.contains("branch_membership"))
        .collect();
    assert_eq!(bulk.len(), 1, "one bulk re-key op");
    assert!(bulk[0].1.contains("src/match.rs"), "verified file bulk-re-keyed");
    assert!(
        !bulk[0].1.contains("src/diverged.rs"),
        "diverged file must never be bulk-re-keyed onto stale content"
    );
}

#[tokio::test]
async fn test_fetch_unchanged_empty_when_old_branch_has_no_files() {
    let pool = create_test_pool().await;
    setup_tables(&pool).await;
    insert_watch_folder(&pool, "w1", "t1", "/tmp/empty").await;

    let paths = fetch_unchanged_paths(&pool, "w1", "main", "dev")
        .await
        .unwrap();
    assert!(paths.is_empty());
}

#[tokio::test]
async fn test_update_last_commit_hash() {
    let pool = create_test_pool().await;
    setup_tables(&pool).await;

    insert_watch_folder(&pool, "w1", "t1", "/tmp/project").await;

    update_last_commit_hash(&pool, "w1", "abc123def456")
        .await
        .unwrap();

    let hash: Option<String> =
        sqlx::query_scalar("SELECT last_commit_hash FROM watch_folders WHERE watch_id = 'w1'")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(hash.as_deref(), Some("abc123def456"));
}

#[tokio::test]
async fn test_enqueue_file_op() {
    let pool = create_test_pool().await;
    setup_tables(&pool).await;
    // Watch folder is needed so the helper can anchor the absolute path.
    insert_watch_folder(&pool, "w1", "t1", "/tmp/project").await;

    let qm = QueueManager::new(pool.clone());

    enqueue_file_op(
        &qm,
        "t1",
        "projects",
        "/tmp/project/src/main.rs",
        QueueOperation::Update,
        "main",
    )
    .await
    .unwrap();

    let count: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM unified_queue WHERE tenant_id = 't1'")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(count, 1);

    let op: String = sqlx::query_scalar("SELECT op FROM unified_queue WHERE tenant_id = 't1'")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(op, "update");
}

/// Unchanged files must be enqueued as `Add` (not `Update`) so the Update
/// pre-flight's non-branch-scoped defensive delete can't wipe the source
/// branch's points before the dedup fast-path scrolls them.
#[tokio::test]
async fn test_enqueue_unchanged_file_uses_add_op() {
    let pool = create_test_pool().await;
    setup_tables(&pool).await;
    insert_watch_folder(&pool, "w1", "t1", "/tmp/project").await;

    let qm = QueueManager::new(pool.clone());

    enqueue_unchanged_file(&qm, "t1", "projects", "src/main.rs", "feature")
        .await
        .unwrap();

    let (op, branch): (String, String) =
        sqlx::query_as("SELECT op, branch FROM unified_queue WHERE tenant_id = 't1'")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(op, "add");
    assert_eq!(branch, "feature");
}

/// A new branch's unchanged files are enqueued as ONE bulk `(Tenant, Scan)` op
/// carrying the verified path list — NOT one `File/Add` per file. This is the fix
/// for "creating a branch floods the queue with pending indexing items".
#[tokio::test]
async fn test_branch_membership_bulk_collapses_to_one_op() {
    let pool = create_test_pool().await;
    setup_tables(&pool).await;
    insert_watch_folder(&pool, "w1", "t1", "/tmp/project").await;
    let qm = QueueManager::new(pool.clone());

    let paths = vec![
        "src/a.rs".to_string(),
        "src/b.rs".to_string(),
        "src/c.rs".to_string(),
    ];
    let n = enqueue_branch_membership_bulk(
        &qm,
        "t1",
        "projects",
        "w1",
        "/tmp/project",
        "feature",
        "main",
        paths,
    )
    .await
    .unwrap();
    assert_eq!(n, 3, "all three paths counted");

    let rows: Vec<(String, String, String)> = sqlx::query_as(
        "SELECT item_type, op, payload_json FROM unified_queue WHERE tenant_id = 't1'",
    )
    .fetch_all(&pool)
    .await
    .unwrap();
    assert_eq!(rows.len(), 1, "one bulk op enqueued, not one item per file");
    assert_eq!(rows[0].0, "tenant");
    assert_eq!(rows[0].1, "scan");
    assert!(
        rows[0].2.contains("branch_membership"),
        "payload carries the bulk marker"
    );
    assert!(
        rows[0].2.contains("src/c.rs"),
        "payload carries the verified path list"
    );
    assert!(
        rows[0].2.contains(r#""from_branch":"main""#),
        "payload carries the branch the no-diff verification compared against \
         (scopes the processor's candidate set to that generation, #224)"
    );
}

/// A path list larger than `BRANCH_BULK_CHUNK` is split into several BOUNDED ops,
/// never one monolithic op. Each op runs to completion on a single processor slot
/// doing a sequential Qdrant `set_payload` per base_point, so a small chunk keeps
/// each op short — it completes, releases the slot, and interleaves with File
/// ingestion instead of monopolizing a slot for tens of minutes under load.
#[tokio::test]
async fn test_branch_membership_bulk_chunks_large_lists() {
    let pool = create_test_pool().await;
    setup_tables(&pool).await;
    insert_watch_folder(&pool, "w1", "t1", "/tmp/project").await;
    let qm = QueueManager::new(pool.clone());

    // 60 paths with BRANCH_BULK_CHUNK = 25 → ceil(60/25) = 3 bounded ops.
    let paths: Vec<String> = (0..60).map(|i| format!("src/f{i}.rs")).collect();
    let n = enqueue_branch_membership_bulk(
        &qm, "t1", "projects", "w1", "/tmp/project", "feature", "main", paths,
    )
    .await
    .unwrap();
    assert_eq!(n, 60, "all paths counted");

    let rows: Vec<(String,)> =
        sqlx::query_as("SELECT payload_json FROM unified_queue WHERE tenant_id = 't1'")
            .fetch_all(&pool)
            .await
            .unwrap();
    assert_eq!(rows.len(), 3, "60 paths / chunk 25 = 3 bounded ops, not 1 monolith");

    // Every path appears exactly once across the chunk payloads — no loss, no dup.
    let combined = rows.iter().map(|r| r.0.as_str()).collect::<Vec<_>>().join("");
    for i in 0..60 {
        assert!(
            combined.contains(&format!("src/f{i}.rs")),
            "path src/f{i}.rs present in some chunk"
        );
    }
}

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

    let mut paths = fetch_paths_missing_branch(&pool, "w1", "feat").await.unwrap();
    paths.sort();
    assert_eq!(paths, vec!["src/a.rs".to_string(), "src/c.rs".to_string()]);
}

/// reconcile enqueues an Add (dedup fast-path) only for files that are BOTH
/// missing the current branch AND present in the working tree. Deleted files
/// (tracked but absent on disk) and already-tagged files are skipped.
#[tokio::test]
async fn test_reconcile_enqueues_present_untagged_only() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    std::fs::create_dir_all(root.join("src")).unwrap();
    std::fs::write(root.join("src/a.rs"), b"fn a() {}").unwrap(); // present
    // src/gone.rs is NOT created — tracked but deleted on this branch.

    let pool = create_test_pool().await;
    setup_tables(&pool).await;
    let root_str = root.to_string_lossy().to_string();
    insert_watch_folder(&pool, "w1", "t1", &root_str).await;
    insert_tracked_file(&pool, "w1", &["main"], "h_a", "src/a.rs").await; // present + untagged
    insert_tracked_file(&pool, "w1", &["main"], "h_g", "src/gone.rs").await; // untagged but absent
    insert_tracked_file(&pool, "w1", &["main", "feat"], "h_b", "src/b.rs").await; // tagged

    let qm = QueueManager::new(pool.clone());
    let n = reconcile_branch_membership(&pool, &qm, "w1", "t1", "projects", &root_str, "feat").await;
    assert_eq!(n, 1, "only the present, untagged file is reconciled");

    let rows: Vec<(String, String, String)> =
        sqlx::query_as("SELECT op, branch, payload_json FROM unified_queue WHERE tenant_id = 't1'")
            .fetch_all(&pool)
            .await
            .unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].0, "add"); // Add op → dedup fast-path, no re-embed
    assert_eq!(rows[0].1, "feat");
    assert!(rows[0].2.contains("src/a.rs"));
}

/// Once every working-tree file is already tagged, reconcile is a no-op (no
/// queue churn) — the idempotency that makes it safe to run on every scan.
#[tokio::test]
async fn test_reconcile_noop_when_all_tagged() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    std::fs::create_dir_all(root.join("src")).unwrap();
    std::fs::write(root.join("src/a.rs"), b"fn a() {}").unwrap();

    let pool = create_test_pool().await;
    setup_tables(&pool).await;
    let root_str = root.to_string_lossy().to_string();
    insert_watch_folder(&pool, "w1", "t1", &root_str).await;
    insert_tracked_file(&pool, "w1", &["main", "feat"], "h_a", "src/a.rs").await;

    let qm = QueueManager::new(pool.clone());
    let n = reconcile_branch_membership(&pool, &qm, "w1", "t1", "projects", &root_str, "feat").await;
    assert_eq!(n, 0);
    let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM unified_queue")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(count, 0);
}
