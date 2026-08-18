//! Tests for the tenant processing strategy.

use std::path::Path;
use std::sync::Arc;

use async_trait::async_trait;
use sqlx::SqlitePool;
use tokio::sync::Semaphore;

use crate::allowed_extensions::AllowedExtensions;
use crate::context::ProcessingContext;
use crate::document_processor::DocumentProcessor;
use crate::embedding::{
    DenseEmbedding, DenseProvider, EmbeddingConfig, EmbeddingError, EmbeddingGenerator,
};
use crate::git::GitStatus;
use crate::lexicon::LexiconManager;
use crate::patterns::exclusion::should_exclude_file;
use crate::queue_operations::QueueManager;
use crate::storage::StorageClient;
use crate::strategies::ProcessingStrategy;
use crate::unified_queue_schema::{
    ItemType, ProjectPayload, QueueOperation, QueueStatus, UnifiedQueueItem,
    CREATE_UNIFIED_QUEUE_INDEXES_SQL, CREATE_UNIFIED_QUEUE_SQL,
};
use crate::watch_folders_schema::CREATE_WATCH_FOLDERS_SQL;
use wqm_common::constants::COLLECTION_PROJECTS;

use super::project::{enqueue_project_scan, insert_watch_folder, WatchFolderInsertStatus};
use super::TenantStrategy;

#[test]
fn test_tenant_strategy_handles_tenant_items() {
    let strategy = TenantStrategy;
    assert!(strategy.handles(&ItemType::Tenant, &QueueOperation::Add));
    assert!(strategy.handles(&ItemType::Tenant, &QueueOperation::Scan));
    assert!(strategy.handles(&ItemType::Tenant, &QueueOperation::Delete));
    assert!(strategy.handles(&ItemType::Tenant, &QueueOperation::Rename));
}

#[test]
fn test_tenant_strategy_handles_doc_items() {
    let strategy = TenantStrategy;
    assert!(strategy.handles(&ItemType::Doc, &QueueOperation::Delete));
    assert!(strategy.handles(&ItemType::Doc, &QueueOperation::Uplift));
}

#[test]
fn test_tenant_strategy_rejects_non_tenant_items() {
    let strategy = TenantStrategy;
    assert!(!strategy.handles(&ItemType::File, &QueueOperation::Add));
    assert!(!strategy.handles(&ItemType::Text, &QueueOperation::Add));
    assert!(!strategy.handles(&ItemType::Folder, &QueueOperation::Scan));
}

#[test]
fn test_tenant_strategy_name() {
    let strategy = TenantStrategy;
    assert_eq!(strategy.name(), "tenant");
}

/// Test that the exclusion check logic correctly identifies files that should be cleaned up.
/// This tests the core decision logic used by cleanup_excluded_files without needing
/// Qdrant or SQLite connections.
#[test]
fn test_cleanup_exclusion_logic_identifies_hidden_files() {
    let project_root = Path::new("/home/user/project");

    // Simulate file paths as they would be stored in Qdrant (absolute paths)
    let qdrant_paths = vec![
        "/home/user/project/src/main.rs",
        "/home/user/project/.hidden_file",
        "/home/user/project/src/.secret",
        "/home/user/project/.git/config",
        "/home/user/project/src/lib.rs",
        "/home/user/project/node_modules/package/index.js",
        "/home/user/project/.env",
        "/home/user/project/README.md",
        "/home/user/project/src/.cache/data",
        "/home/user/project/.github/workflows/ci.yml",
    ];

    let mut should_delete = Vec::new();
    let mut should_keep = Vec::new();

    for qdrant_file in &qdrant_paths {
        let rel_path = match Path::new(qdrant_file).strip_prefix(project_root) {
            Ok(stripped) => stripped.to_string_lossy().to_string(),
            Err(_) => qdrant_file.to_string(),
        };

        if should_exclude_file(&rel_path) {
            should_delete.push(qdrant_file.to_string());
        } else {
            should_keep.push(qdrant_file.to_string());
        }
    }

    // Hidden files should be marked for deletion
    assert!(
        should_delete.contains(&"/home/user/project/.hidden_file".to_string()),
        "Expected .hidden_file to be excluded"
    );
    assert!(
        should_delete.contains(&"/home/user/project/src/.secret".to_string()),
        "Expected src/.secret to be excluded"
    );
    assert!(
        should_delete.contains(&"/home/user/project/.git/config".to_string()),
        "Expected .git/config to be excluded"
    );
    assert!(
        should_delete.contains(&"/home/user/project/.env".to_string()),
        "Expected .env to be excluded"
    );
    assert!(
        should_delete.contains(&"/home/user/project/src/.cache/data".to_string()),
        "Expected src/.cache/data to be excluded"
    );
    assert!(
        should_delete.contains(&"/home/user/project/node_modules/package/index.js".to_string()),
        "Expected node_modules content to be excluded"
    );

    // Normal files should NOT be deleted
    assert!(
        should_keep.contains(&"/home/user/project/src/main.rs".to_string()),
        "Expected src/main.rs to be kept"
    );
    assert!(
        should_keep.contains(&"/home/user/project/src/lib.rs".to_string()),
        "Expected src/lib.rs to be kept"
    );
    assert!(
        should_keep.contains(&"/home/user/project/README.md".to_string()),
        "Expected README.md to be kept"
    );

    // .github/ should be whitelisted (not excluded)
    assert!(
        should_keep.contains(&"/home/user/project/.github/workflows/ci.yml".to_string()),
        "Expected .github/workflows/ci.yml to be kept (whitelisted)"
    );
}

#[test]
fn test_cleanup_exclusion_logic_with_non_strippable_paths() {
    // Test when Qdrant paths don't share the project root prefix
    let project_root = Path::new("/home/user/project");
    let qdrant_file = "/different/root/src/.hidden";

    let rel_path = match Path::new(qdrant_file).strip_prefix(project_root) {
        Ok(stripped) => stripped.to_string_lossy().to_string(),
        Err(_) => qdrant_file.to_string(),
    };

    // Should still detect hidden component even with full path fallback
    assert!(
        should_exclude_file(&rel_path),
        "Expected .hidden to be excluded even when path can't be stripped"
    );
}

#[test]
fn test_cleanup_exclusion_logic_empty_paths() {
    // Verify no panic with edge cases
    let project_root = Path::new("/home/user/project");
    let qdrant_paths: Vec<String> = vec![];

    let mut count = 0u64;
    for qdrant_file in &qdrant_paths {
        let rel_path = match Path::new(qdrant_file).strip_prefix(project_root) {
            Ok(stripped) => stripped.to_string_lossy().to_string(),
            Err(_) => qdrant_file.clone(),
        };

        if should_exclude_file(&rel_path) {
            count += 1;
        }
    }

    assert_eq!(count, 0, "Empty path list should produce zero deletions");
}

// =============================================================================
// Tests for Unit 2 (audit issue #8): gate scan enqueue on watch_folder insert
// =============================================================================
//
// Background: `insert_watch_folder` silently returns Ok(()) when the
// project_root is a subdirectory of an already-registered project. Before
// this fix, `handle_project_add` then called `enqueue_project_scan`
// unconditionally, generating File/Add items with an orphan tenant_id (no
// watch_folder row exists). These tests verify the new gating contract.

/// No-op dense provider for tests that don't need actual embeddings.
///
/// `insert_watch_folder` never calls into the embedding pipeline, but the
/// `ProcessingContext::new` constructor requires a non-`None`
/// `EmbeddingGenerator`. This stub satisfies the signature without
/// downloading any models or touching ONNX Runtime.
#[derive(Debug)]
struct NoopDenseProvider;

#[async_trait]
impl DenseProvider for NoopDenseProvider {
    async fn embed(&self, texts: &[&str]) -> Result<Vec<DenseEmbedding>, EmbeddingError> {
        Ok(texts
            .iter()
            .map(|t| DenseEmbedding {
                vector: vec![0.0; 1],
                model_name: "noop".to_string(),
                sequence_length: t.len(),
            })
            .collect())
    }

    fn output_dim(&self) -> usize {
        1
    }

    fn provider_label(&self) -> &str {
        "noop"
    }

    fn metrics_label(&self) -> &'static str {
        "fastembed"
    }

    async fn probe(&self) -> Result<(), EmbeddingError> {
        Ok(())
    }
}

/// Build an in-memory SQLite pool with the schemas `insert_watch_folder`
/// and `enqueue_project_scan` actually touch.
async fn setup_test_pool() -> SqlitePool {
    let pool = SqlitePool::connect("sqlite::memory:")
        .await
        .expect("Failed to create in-memory SQLite pool");

    sqlx::query(CREATE_UNIFIED_QUEUE_SQL)
        .execute(&pool)
        .await
        .expect("Failed to create unified_queue table");

    for index_sql in CREATE_UNIFIED_QUEUE_INDEXES_SQL {
        sqlx::query(index_sql)
            .execute(&pool)
            .await
            .expect("Failed to create unified_queue index");
    }

    sqlx::query(CREATE_WATCH_FOLDERS_SQL)
        .execute(&pool)
        .await
        .expect("Failed to create watch_folders table");

    pool
}

/// Build a minimal `ProcessingContext` for tenant-strategy unit tests.
///
/// The Qdrant `StorageClient` is constructed lazily (no network connection
/// is made up-front); tests must not invoke any method that would actually
/// talk to Qdrant. `insert_watch_folder` and `enqueue_project_scan` only
/// touch SQLite, so this is safe for our scope.
fn build_test_context(pool: SqlitePool) -> ProcessingContext {
    let queue_manager = Arc::new(QueueManager::new(pool.clone()));
    let storage_client = Arc::new(StorageClient::new());
    let dense_provider = Arc::new(NoopDenseProvider);
    let embedding_generator = Arc::new(
        EmbeddingGenerator::new(EmbeddingConfig::default(), dense_provider)
            .expect("EmbeddingGenerator::new should succeed with NoopDenseProvider"),
    );
    let document_processor = Arc::new(DocumentProcessor::new());
    let embedding_semaphore = Arc::new(Semaphore::new(1));
    let lexicon_manager = Arc::new(LexiconManager::new(pool.clone(), 1.2));
    let allowed_extensions = Arc::new(AllowedExtensions::default());

    ProcessingContext::new(
        pool,
        queue_manager,
        storage_client,
        embedding_generator,
        document_processor,
        embedding_semaphore,
        lexicon_manager,
        None,
        None,
        allowed_extensions,
    )
}

/// Insert a watch_folder row representing an already-registered project.
async fn insert_parent_watch_folder(pool: &SqlitePool, path: &str, tenant_id: &str) {
    let now = wqm_common::timestamps::now_utc();
    sqlx::query(
        r#"INSERT INTO watch_folders (
            watch_id, path, collection, tenant_id, is_active,
            follow_symlinks, enabled, cleanup_on_disable,
            is_git_tracked, is_worktree,
            created_at, updated_at
        ) VALUES (?1, ?2, ?3, ?4, 1, 0, 1, 0, 0, 0, ?5, ?5)"#,
    )
    .bind(uuid::Uuid::new_v4().to_string())
    .bind(path)
    .bind(COLLECTION_PROJECTS)
    .bind(tenant_id)
    .bind(&now)
    .execute(pool)
    .await
    .expect("seed parent watch_folder");
}

/// The bulk branch re-key persists each base_point's tag as it goes (checkpoint),
/// so a re-leased op RESUMES: `select_branch_bulk_candidates` excludes a
/// base_point once `persist_branch_tag_for_base_points` has tagged it. This is the
/// lease-loop fix — with an end-only persist, an op that exceeded the 300s lease
/// under heavy Qdrant load looped forever re-running every set_payload without
/// committing progress. Pure state.db path (no Qdrant / no search.db).
#[tokio::test]
async fn test_branch_bulk_persist_checkpoints_and_resumes() {
    use super::project::{persist_branch_tag_for_base_points, select_branch_bulk_candidates};
    use crate::tracked_files_schema::CREATE_TRACKED_FILES_V41_SQL;
    use sqlx::sqlite::SqlitePoolOptions;

    let pool = SqlitePoolOptions::new()
        .max_connections(1)
        .connect("sqlite::memory:")
        .await
        .expect("in-memory pool");
    sqlx::query(CREATE_WATCH_FOLDERS_SQL)
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query(CREATE_TRACKED_FILES_V41_SQL)
        .execute(&pool)
        .await
        .unwrap();

    let now = "2025-01-01T00:00:00Z";
    sqlx::query(
        "INSERT INTO watch_folders (watch_id, path, collection, tenant_id, enabled, created_at, updated_at)
         VALUES ('w1', '/tmp/project', 'projects', 't1', 1, ?1, ?1)",
    )
    .bind(now)
    .execute(&pool)
    .await
    .unwrap();
    for (h, rel) in [("h_a", "src/a.rs"), ("h_b", "src/b.rs")] {
        let bp = wqm_common::hashing::compute_base_point("t1", rel, h);
        sqlx::query(
            "INSERT INTO tracked_files
             (watch_folder_id, relative_path, branches, file_mtime, file_hash, collection, base_point, created_at, updated_at)
             VALUES ('w1', ?1, '[\"main\"]', ?2, ?3, 'projects', ?4, ?2, ?2)",
        )
        .bind(rel)
        .bind(now)
        .bind(h)
        .bind(&bp)
        .execute(&pool)
        .await
        .unwrap();
    }
    let bp_a = wqm_common::hashing::compute_base_point("t1", "src/a.rs", "h_a");
    let paths = vec!["src/a.rs".to_string(), "src/b.rs".to_string()];

    // Both base_points are candidates for 'feat' before any persist (both
    // generations hold 'main', the branch the no-diff verification compared).
    let before = select_branch_bulk_candidates(&pool, "w1", "feat", Some("main"), &paths)
        .await
        .unwrap();
    assert_eq!(before.len(), 2, "both base_points missing 'feat'");

    // Checkpoint ONLY a.rs's base_point.
    persist_branch_tag_for_base_points(&pool, None, "w1", "feat", std::slice::from_ref(&bp_a))
        .await
        .unwrap();

    let branches_a: String =
        sqlx::query_scalar("SELECT branches FROM tracked_files WHERE relative_path = 'src/a.rs'")
            .fetch_one(&pool)
            .await
            .unwrap();
    let branches_b: String =
        sqlx::query_scalar("SELECT branches FROM tracked_files WHERE relative_path = 'src/b.rs'")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert!(
        branches_a.contains("feat"),
        "a.rs re-keyed onto feat: {branches_a}"
    );
    assert!(!branches_b.contains("feat"), "b.rs untouched: {branches_b}");

    // RESUME: re-selecting candidates now excludes a.rs — only b.rs remains.
    let after = select_branch_bulk_candidates(&pool, "w1", "feat", Some("main"), &paths)
        .await
        .unwrap();
    assert_eq!(
        after.len(),
        1,
        "a.rs skipped after checkpoint — op resumes at b.rs"
    );

    // IDEMPOTENT: persisting a.rs again does not double-insert 'feat'.
    persist_branch_tag_for_base_points(&pool, None, "w1", "feat", std::slice::from_ref(&bp_a))
        .await
        .unwrap();
    let branches_a2: String =
        sqlx::query_scalar("SELECT branches FROM tracked_files WHERE relative_path = 'src/a.rs'")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(
        branches_a2.matches("feat").count(),
        1,
        "feat tagged once: {branches_a2}"
    );
}

/// Cross-worktree isolation for the bulk branch re-key's search.db mirror.
///
/// Two sibling worktrees/clones of ONE project (same tenant) holding identical
/// content share a `base_point` but keep DISTINCT `file_id`s. Tagging branch
/// `feat` onto w1 must touch ONLY w1's rows in BOTH stores. Keying the
/// `file_metadata` half by `base_point` (the old bug) leaked the tag onto the
/// sibling — its authority never carried `feat`, so a branch-scoped grep
/// returned the sibling's generation as a duplicate hit. The watch-folder-scoped
/// `file_id` resolution in `persist_branch_tag_for_base_points` is the fix; this
/// guards it end-to-end through a real `SearchDbManager` (the existing
/// checkpoint test passes `None` and never exercises the mirror write).
#[tokio::test]
async fn test_branch_bulk_search_db_mirror_scopes_to_watch_folder() {
    use super::project::persist_branch_tag_for_base_points;
    use crate::search_db::SearchDbManager;
    use crate::tracked_files_schema::CREATE_TRACKED_FILES_V41_SQL;
    use sqlx::sqlite::SqlitePoolOptions;

    let pool = SqlitePoolOptions::new()
        .max_connections(1)
        .connect("sqlite::memory:")
        .await
        .expect("in-memory pool");
    sqlx::query(CREATE_WATCH_FOLDERS_SQL)
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query(CREATE_TRACKED_FILES_V41_SQL)
        .execute(&pool)
        .await
        .unwrap();

    let now = "2025-01-01T00:00:00Z";
    // Two sibling worktrees of the SAME project (tenant t1). base_point =
    // SHA256(tenant|relative_path|file_hash) — identical content ⇒ identical
    // base_point across the two watch folders, distinct AUTOINCREMENT file_ids.
    for wf in ["w1", "w2"] {
        sqlx::query(
            "INSERT INTO watch_folders (watch_id, path, collection, tenant_id, enabled, created_at, updated_at)
             VALUES (?1, ?2, 'projects', 't1', 1, ?3, ?3)",
        )
        .bind(wf)
        .bind(format!("/tmp/{wf}"))
        .bind(now)
        .execute(&pool)
        .await
        .unwrap();
    }
    let bp = wqm_common::hashing::compute_base_point("t1", "src/a.rs", "h_a");
    for wf in ["w1", "w2"] {
        sqlx::query(
            "INSERT INTO tracked_files
             (watch_folder_id, relative_path, branches, file_mtime, file_hash, collection, base_point, created_at, updated_at)
             VALUES (?1, 'src/a.rs', '[\"main\"]', ?2, 'h_a', 'projects', ?3, ?2, ?2)",
        )
        .bind(wf)
        .bind(now)
        .bind(&bp)
        .execute(&pool)
        .await
        .unwrap();
    }
    let fid_w1: i64 =
        sqlx::query_scalar("SELECT file_id FROM tracked_files WHERE watch_folder_id = 'w1'")
            .fetch_one(&pool)
            .await
            .unwrap();
    let fid_w2: i64 =
        sqlx::query_scalar("SELECT file_id FROM tracked_files WHERE watch_folder_id = 'w2'")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_ne!(
        fid_w1, fid_w2,
        "sibling worktrees must have distinct file_ids"
    );

    // Seed the search.db mirror: one file_metadata row per file_id, both on main.
    let dir = tempfile::tempdir().unwrap();
    let sdb = Arc::new(
        SearchDbManager::new(dir.path().join("search.db"))
            .await
            .unwrap(),
    );
    for (fid, wf) in [(fid_w1, "w1"), (fid_w2, "w2")] {
        sqlx::query(crate::code_lines_schema::UPSERT_FILE_METADATA_SQL)
            .bind(fid)
            .bind("t1")
            .bind("main")
            .bind(format!("/tmp/{wf}/src/a.rs"))
            .bind(&bp)
            .bind("src/a.rs")
            .bind("h_a")
            .bind(None::<i64>)
            .bind(0_i64)
            .execute(sdb.pool())
            .await
            .unwrap();
    }

    // Bulk re-key: tag `feat` onto w1's base_point only.
    persist_branch_tag_for_base_points(&pool, Some(&sdb), "w1", "feat", std::slice::from_ref(&bp))
        .await
        .unwrap();

    let w1_tf: String =
        sqlx::query_scalar("SELECT branches FROM tracked_files WHERE watch_folder_id = 'w1'")
            .fetch_one(&pool)
            .await
            .unwrap();
    let w2_tf: String =
        sqlx::query_scalar("SELECT branches FROM tracked_files WHERE watch_folder_id = 'w2'")
            .fetch_one(&pool)
            .await
            .unwrap();
    let w1_fm: String = sqlx::query_scalar("SELECT branches FROM file_metadata WHERE file_id = ?1")
        .bind(fid_w1)
        .fetch_one(sdb.pool())
        .await
        .unwrap();
    let w2_fm: String = sqlx::query_scalar("SELECT branches FROM file_metadata WHERE file_id = ?1")
        .bind(fid_w2)
        .fetch_one(sdb.pool())
        .await
        .unwrap();

    // w1: authority AND mirror both gained `feat`.
    assert!(w1_tf.contains("feat"), "w1 tracked_files re-keyed: {w1_tf}");
    assert!(
        w1_fm.contains("feat"),
        "w1 file_metadata mirror re-keyed: {w1_fm}"
    );
    // w2 (sibling sharing the base_point): UNTOUCHED in BOTH stores — the leak fix.
    assert!(
        !w2_tf.contains("feat"),
        "w2 tracked_files must be untouched: {w2_tf}"
    );
    assert!(
        !w2_fm.contains("feat"),
        "sibling worktree's file_metadata must NOT be cross-tagged: {w2_fm}"
    );
}

/// Build a synthetic `UnifiedQueueItem` for a Tenant/Add of `project_root`.
fn make_tenant_add_item(tenant_id: &str, project_root: &str) -> UnifiedQueueItem {
    let payload = ProjectPayload {
        project_root: project_root.to_string(),
        git_remote: None,
        project_type: None,
        old_tenant_id: None,
        is_active: Some(false),
        branch_membership: None,
    };
    let payload_json = serde_json::to_string(&payload).unwrap();
    let now = wqm_common::timestamps::now_utc();
    UnifiedQueueItem {
        queue_id: uuid::Uuid::new_v4().to_string(),
        idempotency_key: format!("test-{}", tenant_id),
        item_type: ItemType::Tenant,
        op: QueueOperation::Add,
        tenant_id: tenant_id.to_string(),
        collection: COLLECTION_PROJECTS.to_string(),
        status: QueueStatus::Pending,
        branch: "main".to_string(),
        payload_json,
        metadata: None,
        created_at: now.clone(),
        updated_at: now,
        lease_until: None,
        worker_id: None,
        retry_count: 0,
        error_message: None,
        last_error_at: None,
        file_path: None,
        qdrant_status: None,
        search_status: None,
        decision_json: None,
    }
}

/// Count Tenant/Scan rows enqueued for a given tenant in `unified_queue`.
async fn count_tenant_scans(pool: &SqlitePool, tenant_id: &str) -> i64 {
    sqlx::query_scalar::<_, i64>(
        "SELECT COUNT(*) FROM unified_queue \
         WHERE item_type = 'tenant' AND op = 'scan' AND tenant_id = ?1",
    )
    .bind(tenant_id)
    .fetch_one(pool)
    .await
    .unwrap()
}

#[tokio::test]
async fn insert_watch_folder_returns_skipped_subdir_for_nested_path() {
    let pool = setup_test_pool().await;
    insert_parent_watch_folder(&pool, "/parent", "parent_tenant").await;

    let ctx = build_test_context(pool);
    let item = make_tenant_add_item("subdir_tenant", "/parent/sub");
    let payload: ProjectPayload = serde_json::from_str(&item.payload_json).unwrap();

    let status = insert_watch_folder(&ctx, &item, &payload, &GitStatus::not_git())
        .await
        .expect("insert_watch_folder must not error");

    assert_eq!(
        status,
        WatchFolderInsertStatus::SkippedSubdir,
        "subdir of registered project must return SkippedSubdir"
    );
}

#[tokio::test]
async fn insert_watch_folder_returns_inserted_for_new_top_level_path() {
    let pool = setup_test_pool().await;

    let ctx = build_test_context(pool);
    let item = make_tenant_add_item("fresh_tenant", "/fresh/project");
    let payload: ProjectPayload = serde_json::from_str(&item.payload_json).unwrap();

    let status = insert_watch_folder(&ctx, &item, &payload, &GitStatus::not_git())
        .await
        .expect("insert_watch_folder must not error");

    assert_eq!(
        status,
        WatchFolderInsertStatus::Inserted,
        "first registration of a new path must return Inserted"
    );
}

#[tokio::test]
async fn insert_watch_folder_returns_already_exists_on_idempotent_replay() {
    let pool = setup_test_pool().await;
    insert_parent_watch_folder(&pool, "/parent", "parent_tenant").await;

    let ctx = build_test_context(pool);
    let item = make_tenant_add_item("parent_tenant", "/parent");
    let payload: ProjectPayload = serde_json::from_str(&item.payload_json).unwrap();

    let status = insert_watch_folder(&ctx, &item, &payload, &GitStatus::not_git())
        .await
        .expect("insert_watch_folder must not error");

    assert_eq!(
        status,
        WatchFolderInsertStatus::AlreadyExists,
        "re-registering the exact same path must return AlreadyExists"
    );
}

/// End-to-end gating test: a Tenant/Add for a subdirectory of an
/// already-registered project MUST NOT enqueue a Tenant/Scan. Without the
/// fix in this unit, `handle_project_add` would still call
/// `enqueue_project_scan` after `insert_watch_folder` silently returned
/// `Ok(())`, leaving an orphan tenant_id in the queue.
#[tokio::test]
async fn subdir_does_not_enqueue_scan() {
    let pool = setup_test_pool().await;
    insert_parent_watch_folder(&pool, "/parent", "parent_tenant").await;

    let ctx = build_test_context(pool.clone());
    let item = make_tenant_add_item("subdir_tenant", "/parent/sub");
    let payload: ProjectPayload = serde_json::from_str(&item.payload_json).unwrap();

    // Drive the same decision the production `handle_project_add` makes.
    let status = insert_watch_folder(&ctx, &item, &payload, &GitStatus::not_git())
        .await
        .expect("insert_watch_folder must not error");

    match status {
        WatchFolderInsertStatus::Inserted | WatchFolderInsertStatus::AlreadyExists => {
            enqueue_project_scan(&ctx, &item, &payload).await;
        }
        WatchFolderInsertStatus::SkippedSubdir => {
            // Gate fires: no scan enqueued, matching the new
            // `handle_project_add` contract.
        }
    }

    let scan_count = count_tenant_scans(&pool, "subdir_tenant").await;
    assert_eq!(
        scan_count, 0,
        "Tenant/Scan must not be enqueued for an orphan tenant_id (subdir of registered project)"
    );

    // Sanity check: the parent's tenant_id must also not have a stray scan
    // (the gating decision was specifically about the subdir).
    let parent_scan_count = count_tenant_scans(&pool, "parent_tenant").await;
    assert_eq!(
        parent_scan_count, 0,
        "no scan should have been enqueued for the pre-existing parent tenant either"
    );
}

/// Companion to `subdir_does_not_enqueue_scan`: when the path is NOT a
/// subdir, the gating logic must enqueue the scan as before.
#[tokio::test]
async fn top_level_add_does_enqueue_scan() {
    let pool = setup_test_pool().await;

    let ctx = build_test_context(pool.clone());
    let item = make_tenant_add_item("fresh_tenant", "/fresh/project");
    let payload: ProjectPayload = serde_json::from_str(&item.payload_json).unwrap();

    let status = insert_watch_folder(&ctx, &item, &payload, &GitStatus::not_git())
        .await
        .expect("insert_watch_folder must not error");

    match status {
        WatchFolderInsertStatus::Inserted | WatchFolderInsertStatus::AlreadyExists => {
            enqueue_project_scan(&ctx, &item, &payload).await;
        }
        WatchFolderInsertStatus::SkippedSubdir => {
            panic!("top-level path must not be classified as SkippedSubdir");
        }
    }

    let scan_count = count_tenant_scans(&pool, "fresh_tenant").await;
    assert_eq!(
        scan_count, 1,
        "Tenant/Scan should be enqueued exactly once for a brand-new top-level project"
    );
}
