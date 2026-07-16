use super::super::*;
use super::{create_test_pool, setup_tables};

#[tokio::test]
async fn test_insert_and_lookup_tracked_file() {
    let pool = create_test_pool().await;
    setup_tables(&pool).await;

    let file_id = insert_tracked_file(
        &pool,
        "w1",
        "src/main.rs",
        Some("main"),
        Some("code"),
        Some("rust"),
        "2025-01-01T00:00:00Z",
        "abc123hash",
        3,
        Some("tree_sitter"),
        ProcessingStatus::Done,
        ProcessingStatus::Done,
        None,
        None,
        false,
        None,
        None,
    )
    .await
    .expect("Insert failed");

    assert!(file_id > 0);

    let found = lookup_tracked_file(&pool, "w1", "src/main.rs", Some("main"))
        .await
        .expect("Lookup failed");

    assert!(found.is_some());
    let f = found.unwrap();
    assert_eq!(f.file_id, file_id);
    assert_eq!(f.relative_path.as_str(), "src/main.rs");
    assert_eq!(f.file_hash, "abc123hash");
    assert_eq!(f.chunk_count, 3);
    assert_eq!(f.lsp_status, ProcessingStatus::Done);
}

#[tokio::test]
async fn test_lookup_tracked_file_null_branch() {
    let pool = create_test_pool().await;
    setup_tables(&pool).await;

    let file_id = insert_tracked_file(
        &pool,
        "w1",
        "doc.pdf",
        None,
        None,
        None,
        "2025-01-01T00:00:00Z",
        "hash1",
        0,
        None,
        ProcessingStatus::None,
        ProcessingStatus::Skipped,
        None,
        None,
        false,
        None,
        None,
    )
    .await
    .expect("Insert failed");

    // Lookup with None branch
    let found = lookup_tracked_file(&pool, "w1", "doc.pdf", None)
        .await
        .expect("Lookup failed");
    assert!(found.is_some());
    assert_eq!(found.unwrap().file_id, file_id);

    // Lookup with "main" branch should NOT find it
    let not_found = lookup_tracked_file(&pool, "w1", "doc.pdf", Some("main"))
        .await
        .expect("Lookup failed");
    assert!(not_found.is_none());
}

#[tokio::test]
async fn test_update_tracked_file() {
    let pool = create_test_pool().await;
    setup_tables(&pool).await;

    let file_id = insert_tracked_file(
        &pool,
        "w1",
        "src/main.rs",
        Some("main"),
        Some("code"),
        Some("rust"),
        "2025-01-01T00:00:00Z",
        "hash1",
        3,
        Some("text"),
        ProcessingStatus::None,
        ProcessingStatus::None,
        None,
        None,
        false,
        None,
        None,
    )
    .await
    .expect("Insert failed");

    update_tracked_file(
        &pool,
        file_id,
        "2025-01-02T00:00:00Z",
        "hash2",
        5,
        Some("tree_sitter"),
        ProcessingStatus::Done,
        ProcessingStatus::Done,
        None,
        None,
    )
    .await
    .expect("Update failed");

    let found = lookup_tracked_file(&pool, "w1", "src/main.rs", Some("main"))
        .await
        .expect("Lookup failed")
        .unwrap();

    assert_eq!(found.file_hash, "hash2");
    assert_eq!(found.chunk_count, 5);
    assert_eq!(found.chunking_method, Some("tree_sitter".to_string()));
    assert_eq!(found.lsp_status, ProcessingStatus::Done);
    assert!(found.last_error.is_none(), "Update should clear last_error");
}

#[tokio::test]
async fn test_insert_and_get_qdrant_chunks() {
    let pool = create_test_pool().await;
    setup_tables(&pool).await;

    let file_id = insert_tracked_file(
        &pool,
        "w1",
        "src/lib.rs",
        Some("main"),
        Some("code"),
        Some("rust"),
        "2025-01-01T00:00:00Z",
        "hash1",
        2,
        Some("tree_sitter"),
        ProcessingStatus::Done,
        ProcessingStatus::Done,
        None,
        None,
        false,
        None,
        None,
    )
    .await
    .unwrap();

    let chunks = vec![
        (
            "point-1".to_string(),
            0,
            "chash1".to_string(),
            Some(ChunkType::Function),
            Some("main".to_string()),
            Some(1),
            Some(20),
        ),
        (
            "point-2".to_string(),
            1,
            "chash2".to_string(),
            Some(ChunkType::Struct),
            Some("Config".to_string()),
            Some(22),
            Some(40),
        ),
    ];

    insert_qdrant_chunks(&pool, file_id, &chunks)
        .await
        .expect("Insert chunks failed");

    let point_ids = get_chunk_point_ids(&pool, file_id)
        .await
        .expect("Get points failed");
    assert_eq!(point_ids.len(), 2);
    assert!(point_ids.contains(&"point-1".to_string()));
    assert!(point_ids.contains(&"point-2".to_string()));
}

#[tokio::test]
async fn test_delete_tracked_file_cascades_chunks() {
    let pool = create_test_pool().await;
    setup_tables(&pool).await;

    let file_id = insert_tracked_file(
        &pool,
        "w1",
        "src/main.rs",
        Some("main"),
        Some("code"),
        Some("rust"),
        "2025-01-01T00:00:00Z",
        "hash1",
        1,
        None,
        ProcessingStatus::None,
        ProcessingStatus::None,
        None,
        None,
        false,
        None,
        None,
    )
    .await
    .unwrap();

    let chunks = vec![(
        "point-1".to_string(),
        0,
        "chash1".to_string(),
        None,
        None,
        None,
        None,
    )];
    insert_qdrant_chunks(&pool, file_id, &chunks).await.unwrap();

    let points_before = get_chunk_point_ids(&pool, file_id).await.unwrap();
    assert_eq!(points_before.len(), 1);

    delete_tracked_file(&pool, file_id)
        .await
        .expect("Delete failed");

    let points_after = get_chunk_point_ids(&pool, file_id).await.unwrap();
    assert_eq!(
        points_after.len(),
        0,
        "Chunks should be deleted via CASCADE"
    );
}

#[tokio::test]
async fn test_get_tracked_file_paths() {
    let pool = create_test_pool().await;
    setup_tables(&pool).await;

    insert_tracked_file(
        &pool,
        "w1",
        "src/main.rs",
        Some("main"),
        None,
        None,
        "2025-01-01T00:00:00Z",
        "h1",
        0,
        None,
        ProcessingStatus::None,
        ProcessingStatus::None,
        None,
        None,
        false,
        None,
        None,
    )
    .await
    .unwrap();

    insert_tracked_file(
        &pool,
        "w1",
        "src/lib.rs",
        Some("main"),
        None,
        None,
        "2025-01-01T00:00:00Z",
        "h2",
        0,
        None,
        ProcessingStatus::None,
        ProcessingStatus::None,
        None,
        None,
        false,
        None,
        None,
    )
    .await
    .unwrap();

    let paths = get_tracked_file_paths(&pool, "w1")
        .await
        .expect("Query failed");
    assert_eq!(paths.len(), 2);

    let file_names: Vec<&str> = paths.iter().map(|(_, p, _)| p.as_str()).collect();
    assert!(file_names.contains(&"src/main.rs"));
    assert!(file_names.contains(&"src/lib.rs"));
}

#[tokio::test]
async fn test_lookup_watch_folder() {
    let pool = create_test_pool().await;
    setup_tables(&pool).await;

    let result = lookup_watch_folder(&pool, "t1", "projects")
        .await
        .expect("Lookup failed");
    assert!(result.is_some());
    let (wid, path) = result.unwrap();
    assert_eq!(wid, "w1");
    assert_eq!(path, "/home/user/project");

    let missing = lookup_watch_folder(&pool, "nonexistent", "projects")
        .await
        .expect("Lookup failed");
    assert!(missing.is_none());
}

#[tokio::test]
async fn test_delete_qdrant_chunks_explicit() {
    let pool = create_test_pool().await;
    setup_tables(&pool).await;

    let file_id = insert_tracked_file(
        &pool,
        "w1",
        "file.rs",
        Some("main"),
        None,
        None,
        "2025-01-01T00:00:00Z",
        "h1",
        2,
        None,
        ProcessingStatus::None,
        ProcessingStatus::None,
        None,
        None,
        false,
        None,
        None,
    )
    .await
    .unwrap();

    let chunks = vec![
        (
            "p1".to_string(),
            0,
            "c1".to_string(),
            None,
            None,
            None,
            None,
        ),
        (
            "p2".to_string(),
            1,
            "c2".to_string(),
            None,
            None,
            None,
            None,
        ),
    ];
    insert_qdrant_chunks(&pool, file_id, &chunks).await.unwrap();

    delete_qdrant_chunks(&pool, file_id)
        .await
        .expect("Delete chunks failed");

    let points = get_chunk_point_ids(&pool, file_id).await.unwrap();
    assert_eq!(points.len(), 0);

    let file = lookup_tracked_file(&pool, "w1", "file.rs", Some("main"))
        .await
        .unwrap();
    assert!(file.is_some());
}

#[tokio::test]
async fn test_get_tracked_files_by_prefix() {
    let pool = create_test_pool().await;
    setup_tables(&pool).await;

    for (path, hash) in [
        ("src/core/main.rs", "h1"),
        ("src/core/lib.rs", "h2"),
        ("src/core/utils/helpers.rs", "h3"),
        ("src/cli/main.rs", "h4"),
        ("README.md", "h5"),
    ] {
        insert_tracked_file(
            &pool,
            "w1",
            path,
            Some("main"),
            None,
            None,
            "2025-01-01T00:00:00Z",
            hash,
            0,
            None,
            ProcessingStatus::None,
            ProcessingStatus::None,
            None,
            None,
            false,
            None,
            None,
        )
        .await
        .unwrap();
    }

    let result = get_tracked_files_by_prefix(&pool, "w1", "src/core")
        .await
        .unwrap();
    assert_eq!(result.len(), 3, "Should match all 3 files under src/core/");
    let paths: Vec<&str> = result.iter().map(|(_, p, _)| p.as_str()).collect();
    assert!(paths.contains(&"src/core/main.rs"));
    assert!(paths.contains(&"src/core/lib.rs"));
    assert!(paths.contains(&"src/core/utils/helpers.rs"));

    let result2 = get_tracked_files_by_prefix(&pool, "w1", "src/core/")
        .await
        .unwrap();
    assert_eq!(result2.len(), 3, "Trailing slash should not affect result");

    let result3 = get_tracked_files_by_prefix(&pool, "w1", "src")
        .await
        .unwrap();
    assert_eq!(result3.len(), 4, "Should match all 4 files under src/");

    let result4 = get_tracked_files_by_prefix(&pool, "w1", "src/cli")
        .await
        .unwrap();
    assert_eq!(result4.len(), 1);

    let result5 = get_tracked_files_by_prefix(&pool, "w1", "nonexistent")
        .await
        .unwrap();
    assert_eq!(result5.len(), 0);
}

#[tokio::test]
async fn test_get_tracked_files_by_prefix_no_false_positives() {
    let pool = create_test_pool().await;
    setup_tables(&pool).await;

    for (path, hash) in [
        ("src/core/main.rs", "h1"),
        ("src/core_utils/helpers.rs", "h2"),
    ] {
        insert_tracked_file(
            &pool,
            "w1",
            path,
            Some("main"),
            None,
            None,
            "2025-01-01T00:00:00Z",
            hash,
            0,
            None,
            ProcessingStatus::None,
            ProcessingStatus::None,
            None,
            None,
            false,
            None,
            None,
        )
        .await
        .unwrap();
    }

    // "src/core" should NOT match "src/core_utils/helpers.rs"
    let result = get_tracked_files_by_prefix(&pool, "w1", "src/core")
        .await
        .unwrap();
    assert_eq!(
        result.len(),
        1,
        "Should only match src/core/ not src/core_utils/"
    );
    assert_eq!(result[0].1, "src/core/main.rs");
}

// ── #224 overlap groups: shadowed-holder queries ────────────────────────────

/// Insert one generation of `path` with `hash`, tagged `branch`.
async fn insert_generation(
    pool: &sqlx::SqlitePool,
    path: &str,
    hash: &str,
    branch: &str,
) -> i64 {
    insert_tracked_file(
        pool,
        "w1",
        path,
        Some(branch),
        Some("code"),
        Some("rust"),
        "2025-01-01T00:00:00Z",
        hash,
        1,
        None,
        ProcessingStatus::Done,
        ProcessingStatus::Done,
        None,
        None,
        false,
        None,
        None,
    )
    .await
    .expect("insert generation")
}

#[tokio::test]
async fn lookup_holding_branch_returns_every_generation_newest_first() {
    let pool = create_test_pool().await;
    setup_tables(&pool).await;

    // The exact overlap shape from the live census: two generations of the
    // same path (different hashes) BOTH tagged with the same branch — the
    // state the enqueue-time/process-time branch race fabricates.
    let old_gen = insert_generation(&pool, "docker-compose.yml", "hash-old", "feat/x").await;
    let new_gen = insert_generation(&pool, "docker-compose.yml", "hash-new", "feat/x").await;
    // Deterministic recency: `insert_tracked_file` stamps `updated_at = now()`
    // on BOTH rows (the mtime argument is a different column), so pin both
    // explicitly — pinning only one would leave it OLDER than the insert-time
    // stamp of the other.
    sqlx::query("UPDATE tracked_files SET updated_at = '2025-01-01T00:00:00Z' WHERE file_id = ?1")
        .bind(old_gen)
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("UPDATE tracked_files SET updated_at = '2025-01-02T00:00:00Z' WHERE file_id = ?1")
        .bind(new_gen)
        .execute(&pool)
        .await
        .unwrap();

    let holders = lookup_tracked_files_holding_branch(&pool, "w1", "docker-compose.yml", "feat/x")
        .await
        .unwrap();
    assert_eq!(holders.len(), 2, "both generations hold the tag");
    assert_eq!(holders[0].file_id, new_gen, "newest-updated first");
    assert_eq!(holders[1].file_id, old_gen);

    // The single-row lookup (what the primary strip targets) must agree with
    // holders[0] — the older row is exactly the one it can never reach.
    let primary = lookup_tracked_file(&pool, "w1", "docker-compose.yml", Some("feat/x"))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(primary.file_id, new_gen);

    // A branch held by neither generation matches nothing.
    let none = lookup_tracked_files_holding_branch(&pool, "w1", "docker-compose.yml", "main")
        .await
        .unwrap();
    assert!(none.is_empty());
}

#[tokio::test]
async fn other_generation_exists_only_when_another_row_tracks_the_path() {
    let pool = create_test_pool().await;
    setup_tables(&pool).await;

    let gen_a = insert_generation(&pool, "src/lib.rs", "hash-a", "main").await;

    // Single generation → nothing else tracks the path (path-keyed cleanups
    // may run when THIS row is deleted).
    assert!(!other_generation_exists(&pool, "w1", "src/lib.rs", gen_a)
        .await
        .unwrap());

    // Second generation on ANOTHER branch → the path stays tracked when one
    // of them is deleted, so path-keyed cleanups must be withheld.
    let gen_b = insert_generation(&pool, "src/lib.rs", "hash-b", "feat/x").await;
    assert!(other_generation_exists(&pool, "w1", "src/lib.rs", gen_a)
        .await
        .unwrap());
    assert!(other_generation_exists(&pool, "w1", "src/lib.rs", gen_b)
        .await
        .unwrap());

    // A different path is not a survivor for this one.
    let gen_other = insert_generation(&pool, "src/other.rs", "hash-c", "main").await;
    assert!(!other_generation_exists(&pool, "w1", "src/other.rs", gen_other)
        .await
        .unwrap());

    // Excluding an already-deleted file_id still answers for the path: after
    // the row is gone, "does anything else track it?" is what the cleanup
    // gate actually asks (the check runs post-delete in delete_tracked_file).
    sqlx::query("DELETE FROM tracked_files WHERE file_id = ?1")
        .bind(gen_b)
        .execute(&pool)
        .await
        .unwrap();
    assert!(other_generation_exists(&pool, "w1", "src/lib.rs", gen_b)
        .await
        .unwrap(), "gen_a still tracks the path");
    sqlx::query("DELETE FROM tracked_files WHERE file_id = ?1")
        .bind(gen_a)
        .execute(&pool)
        .await
        .unwrap();
    assert!(!other_generation_exists(&pool, "w1", "src/lib.rs", gen_a)
        .await
        .unwrap(), "no generation left — cleanups may run");
}
