use super::*;
use crate::search_db::SearchDbManager;
use tempfile::TempDir;

/// Helper to construct a FileChange for tests (new fields default to None).
fn test_change(
    file_id: i64,
    old_content: &str,
    new_content: &str,
    tenant_id: &str,
    branch: Option<&str>,
    file_path: &str,
) -> FileChange {
    FileChange {
        file_id,
        size_bytes: Some(new_content.len() as i64),
        old_content: old_content.to_string(),
        new_content: new_content.to_string(),
        tenant_id: tenant_id.to_string(),
        branch: branch.map(|s| s.to_string()),
        file_path: file_path.to_string(),
        base_point: None,
        relative_path: None,
        file_hash: None,
    }
}

async fn setup_db() -> (TempDir, SearchDbManager) {
    let tmp = TempDir::new().unwrap();
    let db_path = tmp.path().join("search.db");
    let manager = SearchDbManager::new(&db_path).await.unwrap();
    (tmp, manager)
}

#[tokio::test]
async fn test_default_config() {
    let config = FtsBatchConfig::default();
    assert_eq!(config.burst_threshold, DEFAULT_BURST_THRESHOLD);
    assert_eq!(config.burst_threshold, 10);
}

#[tokio::test]
async fn test_should_use_batch_mode() {
    let (_tmp, db) = setup_db().await;
    let processor = FtsBatchProcessor::new(&db, FtsBatchConfig::default());

    assert!(!processor.should_use_batch_mode(0));
    assert!(!processor.should_use_batch_mode(5));
    assert!(!processor.should_use_batch_mode(10)); // <= threshold
    assert!(processor.should_use_batch_mode(11));
    assert!(processor.should_use_batch_mode(100));
}

#[tokio::test]
async fn test_flush_empty() {
    let (_tmp, db) = setup_db().await;
    let mut processor = FtsBatchProcessor::new(&db, FtsBatchConfig::default());

    let stats = processor.flush(0).await.unwrap();
    assert_eq!(stats.files_processed, 0);
    assert_eq!(stats.total_affected(), 0);
}

#[tokio::test]
async fn test_flush_forced_batch_forces_batch_mode() {
    // The FTS5 batch writer accumulates its own batch and calls
    // `flush_forced_batch` (formerly `flush(usize::MAX)`). Even a single pending
    // change — which the depth-driven `flush(1)` processes in single-file mode
    // (see `test_single_file_new_ingestion`) — must take the batch path here.
    let (_tmp, db) = setup_db().await;
    let mut processor = FtsBatchProcessor::new(&db, FtsBatchConfig::default());

    processor.add_change(test_change(
        1,
        "",
        "fn only() {}\nfn one() {}",
        "proj-a",
        Some("main"),
        "/src/only.rs",
    ));

    let stats = processor.flush_forced_batch().await.unwrap();
    assert_eq!(stats.files_processed, 1);
    assert!(
        stats.batch_mode,
        "flush_forced_batch must use batch mode regardless of queue depth"
    );

    db.close().await;
}

#[tokio::test]
async fn test_flush_forced_batch_empty_is_noop() {
    let (_tmp, db) = setup_db().await;
    let mut processor = FtsBatchProcessor::new(&db, FtsBatchConfig::default());

    let stats = processor.flush_forced_batch().await.unwrap();
    assert_eq!(stats.files_processed, 0);
    assert_eq!(stats.total_affected(), 0);

    db.close().await;
}

#[tokio::test]
async fn test_single_file_new_ingestion() {
    let (_tmp, db) = setup_db().await;
    let mut processor = FtsBatchProcessor::new(&db, FtsBatchConfig::default());

    processor.add_change(test_change(
        1,
        "",
        "line 1\nline 2\nline 3",
        "proj-a",
        Some("main"),
        "/src/main.rs",
    ));

    // Queue depth = 1, below threshold => single-file mode
    let stats = processor.flush(1).await.unwrap();
    assert_eq!(stats.files_processed, 1);
    assert_eq!(stats.lines_inserted, 3);
    assert!(!stats.batch_mode);

    // Verify lines in DB
    let count: i32 = sqlx::query_scalar("SELECT COUNT(*) FROM code_lines WHERE file_id = 1")
        .fetch_one(db.pool())
        .await
        .unwrap();
    assert_eq!(count, 3);

    // Verify file_metadata
    let tenant: String =
        sqlx::query_scalar("SELECT tenant_id FROM file_metadata WHERE file_id = 1")
            .fetch_one(db.pool())
            .await
            .unwrap();
    assert_eq!(tenant, "proj-a");

    db.close().await;
}

#[tokio::test]
async fn test_batch_mode_multiple_files() {
    let (_tmp, db) = setup_db().await;
    let mut processor = FtsBatchProcessor::new(&db, FtsBatchConfig::default());

    // Add 3 file changes
    for i in 1..=3 {
        processor.add_change(test_change(
            i,
            "",
            &format!("fn file{}() {{}}\nfn helper{}() {{}}", i, i),
            "proj-a",
            Some("main"),
            &format!("/src/file{}.rs", i),
        ));
    }

    assert_eq!(processor.pending_count(), 3);

    // Queue depth = 20, above threshold => batch mode
    let stats = processor.flush(20).await.unwrap();
    assert_eq!(stats.files_processed, 3);
    assert_eq!(stats.lines_inserted, 6); // 2 lines per file x 3 files
    assert!(stats.batch_mode);

    // Verify all files have lines
    for i in 1..=3_i64 {
        let count: i32 = sqlx::query_scalar("SELECT COUNT(*) FROM code_lines WHERE file_id = ?1")
            .bind(i)
            .fetch_one(db.pool())
            .await
            .unwrap();
        assert_eq!(count, 2, "File {} should have 2 lines", i);
    }

    db.close().await;
}

#[tokio::test]
async fn test_update_with_diff() {
    let (_tmp, db) = setup_db().await;
    let mut processor = FtsBatchProcessor::new(&db, FtsBatchConfig::default());

    // First: ingest original content
    processor.add_change(test_change(
        1,
        "",
        "line 1\nline 2\nline 3",
        "proj-a",
        Some("main"),
        "/src/main.rs",
    ));
    processor.flush(0).await.unwrap();

    // Second: update with modified content
    processor.add_change(test_change(
        1,
        "line 1\nline 2\nline 3",
        "line 1\nline 2 modified\nline 3\nline 4",
        "proj-a",
        Some("main"),
        "/src/main.rs",
    ));
    let stats = processor.flush(0).await.unwrap();

    assert_eq!(stats.files_processed, 1);
    // Should have some combination of unchanged/updated/inserted
    assert!(stats.lines_unchanged > 0 || stats.lines_updated > 0);

    // Verify final line count = 4
    let count: i32 = sqlx::query_scalar("SELECT COUNT(*) FROM code_lines WHERE file_id = 1")
        .fetch_one(db.pool())
        .await
        .unwrap();
    assert_eq!(count, 4);

    db.close().await;
}

#[tokio::test]
async fn test_mid_file_insert_does_not_collide_on_seq() {
    // Regression: inserting a line MID-FILE (not appending at the end) forces
    // `renumber_after_changes` to shift existing rows' seqs. Done in place,
    // the reassignment collides with a not-yet-renumbered row still holding
    // the target seq → `UNIQUE constraint failed: code_lines.file_id,
    // code_lines.seq` (SQLite 2067). The two-pass renumber must avoid it.
    let (_tmp, db) = setup_db().await;
    let mut processor = FtsBatchProcessor::new(&db, FtsBatchConfig::default());

    processor.add_change(test_change(
        1,
        "",
        "line 1\nline 2\nline 3",
        "proj-a",
        Some("main"),
        "/src/main.rs",
    ));
    processor.flush(0).await.unwrap();

    // Insert "inserted" between line 1 and line 2 — pushes line 2 / line 3
    // down, so their target seqs overlap seqs still held by other rows.
    processor.add_change(test_change(
        1,
        "line 1\nline 2\nline 3",
        "line 1\ninserted\nline 2\nline 3",
        "proj-a",
        Some("main"),
        "/src/main.rs",
    ));
    // Pre-fix this flush failed with the UNIQUE constraint violation.
    let stats = processor.flush(0).await.unwrap();
    assert_eq!(stats.files_processed, 1);

    // Final content is in the correct new-file order.
    let rows: Vec<String> =
        sqlx::query_scalar("SELECT content FROM code_lines WHERE file_id = 1 ORDER BY seq")
            .fetch_all(db.pool())
            .await
            .unwrap();
    assert_eq!(rows, vec!["line 1", "inserted", "line 2", "line 3"]);

    // Every seq is distinct (the renumber left no duplicates behind).
    let (total, distinct): (i32, i32) =
        sqlx::query_as("SELECT COUNT(seq), COUNT(DISTINCT seq) FROM code_lines WHERE file_id = 1")
            .fetch_one(db.pool())
            .await
            .unwrap();
    assert_eq!(total, 4);
    assert_eq!(distinct, 4, "seqs must be unique after renumber");

    db.close().await;
}

/// A file's indexed lines in stored order — the shape a grep read sees.
async fn fetch_indexed_lines(db: &SearchDbManager, file_id: i64) -> Vec<String> {
    sqlx::query_scalar("SELECT content FROM code_lines WHERE file_id = ?1 ORDER BY seq")
        .bind(file_id)
        .fetch_all(db.pool())
        .await
        .unwrap()
}

/// The lines a correctly indexed file must hold. Real files end in a newline, so
/// `split('\n')` yields a trailing empty element — `insert_all_lines` stores it,
/// and a re-ingest must keep it.
fn expected_lines(content: &str) -> Vec<String> {
    content.split('\n').map(str::to_string).collect()
}

#[tokio::test]
async fn test_reingest_keeps_content_verbatim_single_file_mode() {
    // Regression, measured live on the DOC-V2 tenant 2026-08-12: after a
    // re-ingest the LAST code_lines row held a copy of the file's FIRST line
    // instead of the trailing empty string. 322 of 324 sampled files (99.4%)
    // carried that shape, so every grep for first-line content (`package …`,
    // `name:`, a doc title) returned a phantom extra hit at a line number past
    // the end of the file, with content from the wrong position.
    //
    // Two properties kept it invisible: the older tests assert only COUNT(*)
    // (which the corruption satisfies — the row count stays right) and they all
    // use content WITHOUT a trailing newline, so the terminal empty-line slot,
    // the one that gets clobbered, is never exercised. Assert the full ordered
    // content instead.
    let (_tmp, db) = setup_db().await;
    let mut processor = FtsBatchProcessor::new(&db, FtsBatchConfig::default());

    let original = "package com.doc.model;\n\nimport java.util.List;\n\nclass A {\n    void m() {}\n}\n";
    let updated = "package com.doc.model;\n\nimport java.util.List;\n\nclass A {\n    void m2() {}\n}\n";

    processor.add_change(test_change(1, "", original, "proj-a", Some("main"), "/src/A.java"));
    processor.flush(0).await.unwrap();
    assert_eq!(
        fetch_indexed_lines(&db, 1).await,
        expected_lines(original),
        "first ingest must store the file verbatim, trailing empty line included"
    );

    processor.add_change(test_change(1, original, updated, "proj-a", Some("main"), "/src/A.java"));
    processor.flush(0).await.unwrap();
    assert_eq!(
        fetch_indexed_lines(&db, 1).await,
        expected_lines(updated),
        "re-ingest must leave the index byte-identical to the new content"
    );

    db.close().await;
}

#[tokio::test]
async fn test_reingest_with_desynced_old_content_keeps_content_verbatim() {
    // THE reproduction of the live corruption. `old_content` is the caller's
    // claim about what is indexed, read from the `indexed_content` cache in
    // state.db — a DIFFERENT database from the code_lines rows in search.db.
    // `fetch_old_content` yields an EMPTY string both when that cache has no
    // entry and when reading it FAILS, and the batch/enqueue lane (unlike the
    // single-file lane, which guards at execute_fts_update) hands that empty
    // string straight to the differ while the file's rows are still present.
    //
    // The differ then reports every new line as Inserted and pairs the old
    // side's lone "" with the new side's TRAILING "" as
    // `Unchanged{old_index: 0, new_index: N}`. `existing_lines.get(0)` is the
    // stored row for line 1, so that row is marked retained (escaping
    // delete_orphaned_lines) and renumbered to the LAST position — the file's
    // first line reappears at the end, where the empty final line belongs.
    let (_tmp, db) = setup_db().await;
    let mut processor = FtsBatchProcessor::new(&db, FtsBatchConfig::default());

    let original = "name: http_auth_adapter\ndescription: adapter\nversion: 1.0.0\n\ndependencies:\n  dio: ^5.10.0\n";
    let updated = "name: http_auth_adapter\ndescription: adapter\nversion: 1.0.1\n\ndependencies:\n  dio: ^5.11.0\n";

    processor.add_change(test_change(7, "", original, "proj-a", Some("main"), "/pubspec.yaml"));
    processor.flush(20).await.unwrap();
    assert_eq!(fetch_indexed_lines(&db, 7).await, expected_lines(original));

    // Re-ingest with a DESYNCED old side (empty cache), rows already present.
    processor.add_change(test_change(7, "", updated, "proj-a", Some("main"), "/pubspec.yaml"));
    processor.flush(20).await.unwrap();

    let stored = fetch_indexed_lines(&db, 7).await;
    // The defect's signature, asserted directly: the first line must not
    // reappear anywhere after position 0.
    assert!(
        !stored[1..].contains(&"name: http_auth_adapter".to_string()),
        "first line leaked into a later slot: {stored:?}"
    );
    assert_eq!(
        stored,
        expected_lines(updated),
        "a desynced old_content must not corrupt the stored lines"
    );

    db.close().await;
}

#[tokio::test]
async fn test_full_rewrite() {
    let (_tmp, db) = setup_db().await;
    let processor = FtsBatchProcessor::new(&db, FtsBatchConfig::default());

    let stats = processor
        .full_rewrite(
            1,
            "alpha\nbeta\ngamma",
            "proj-a",
            Some("main"),
            "/src/lib.rs",
            None,
            None,
            None,
        )
        .await
        .unwrap();

    assert_eq!(stats.files_processed, 1);
    assert_eq!(stats.lines_inserted, 3);

    // Verify lines exist
    let count: i32 = sqlx::query_scalar("SELECT COUNT(*) FROM code_lines WHERE file_id = 1")
        .fetch_one(db.pool())
        .await
        .unwrap();
    assert_eq!(count, 3);

    // Full rewrite again with different content
    let stats2 = processor
        .full_rewrite(
            1,
            "one\ntwo",
            "proj-a",
            Some("main"),
            "/src/lib.rs",
            None,
            None,
            None,
        )
        .await
        .unwrap();
    assert_eq!(stats2.lines_inserted, 2);

    let count2: i32 = sqlx::query_scalar("SELECT COUNT(*) FROM code_lines WHERE file_id = 1")
        .fetch_one(db.pool())
        .await
        .unwrap();
    assert_eq!(count2, 2);

    db.close().await;
}

#[tokio::test]
async fn test_delete_file() {
    let (_tmp, db) = setup_db().await;
    let processor = FtsBatchProcessor::new(&db, FtsBatchConfig::default());

    // Insert some lines first
    processor
        .full_rewrite(
            1,
            "a\nb\nc",
            "proj",
            Some("main"),
            "/file.rs",
            None,
            None,
            None,
        )
        .await
        .unwrap();

    let deleted = processor.delete_file(1).await.unwrap();
    assert_eq!(deleted, 3);

    // Verify no lines remain
    let count: i32 = sqlx::query_scalar("SELECT COUNT(*) FROM code_lines WHERE file_id = 1")
        .fetch_one(db.pool())
        .await
        .unwrap();
    assert_eq!(count, 0);

    // Verify file_metadata also deleted
    let md_count: i32 = sqlx::query_scalar("SELECT COUNT(*) FROM file_metadata WHERE file_id = 1")
        .fetch_one(db.pool())
        .await
        .unwrap();
    assert_eq!(md_count, 0);

    db.close().await;
}

#[tokio::test]
async fn test_delete_tenant() {
    let (_tmp, db) = setup_db().await;
    let processor = FtsBatchProcessor::new(&db, FtsBatchConfig::default());

    // Insert files for two tenants
    processor
        .full_rewrite(
            1,
            "a\nb",
            "proj-a",
            Some("main"),
            "/f1.rs",
            None,
            None,
            None,
        )
        .await
        .unwrap();
    processor
        .full_rewrite(
            2,
            "c\nd\ne",
            "proj-a",
            Some("main"),
            "/f2.rs",
            None,
            None,
            None,
        )
        .await
        .unwrap();
    processor
        .full_rewrite(
            3,
            "x\ny",
            "proj-b",
            Some("main"),
            "/f3.rs",
            None,
            None,
            None,
        )
        .await
        .unwrap();

    // Delete proj-a
    let deleted = processor.delete_tenant("proj-a").await.unwrap();
    assert_eq!(deleted, 5); // 2 + 3 lines

    // proj-b should be untouched
    let count_b: i32 = sqlx::query_scalar("SELECT COUNT(*) FROM code_lines WHERE file_id = 3")
        .fetch_one(db.pool())
        .await
        .unwrap();
    assert_eq!(count_b, 2);

    db.close().await;
}

#[tokio::test]
async fn test_fts5_searchable_after_flush() {
    use sqlx::Row;
    let (_tmp, db) = setup_db().await;
    let mut processor = FtsBatchProcessor::new(&db, FtsBatchConfig::default());

    processor.add_change(test_change(
        1,
        "",
        "fn search_target() {}\nfn other_function() {}",
        "proj-a",
        Some("main"),
        "/src/main.rs",
    ));
    processor.flush(0).await.unwrap();

    // FTS5 should be searchable after flush
    let rows = sqlx::query(crate::code_lines_schema::FTS5_SEARCH_SQL)
        .bind("search_target")
        .fetch_all(db.pool())
        .await
        .unwrap();
    assert_eq!(rows.len(), 1);
    assert!(rows[0]
        .get::<String, _>("content")
        .contains("search_target"));

    db.close().await;
}

#[tokio::test]
async fn test_batch_mode_fts5_searchable() {
    let (_tmp, db) = setup_db().await;
    let mut processor = FtsBatchProcessor::new(&db, FtsBatchConfig::default());

    // Multiple files in batch mode
    processor.add_change(test_change(
        1,
        "",
        "fn batch_alpha() {}",
        "proj-a",
        Some("main"),
        "/src/a.rs",
    ));
    processor.add_change(test_change(
        2,
        "",
        "fn batch_beta() {}",
        "proj-a",
        Some("main"),
        "/src/b.rs",
    ));

    // Force batch mode with high queue depth
    processor.flush(50).await.unwrap();

    // Both should be searchable
    let rows = sqlx::query(crate::code_lines_schema::FTS5_SEARCH_SQL)
        .bind("batch_alpha")
        .fetch_all(db.pool())
        .await
        .unwrap();
    assert_eq!(rows.len(), 1);

    let rows2 = sqlx::query(crate::code_lines_schema::FTS5_SEARCH_SQL)
        .bind("batch_beta")
        .fetch_all(db.pool())
        .await
        .unwrap();
    assert_eq!(rows2.len(), 1);

    db.close().await;
}

#[tokio::test]
async fn test_scoped_search_after_flush() {
    use sqlx::Row;
    let (_tmp, db) = setup_db().await;
    let mut processor = FtsBatchProcessor::new(&db, FtsBatchConfig::default());

    // Two files in different projects
    processor.add_change(test_change(
        1,
        "",
        "fn shared_name() {}",
        "proj-x",
        Some("main"),
        "/src/x.rs",
    ));
    processor.add_change(test_change(
        2,
        "",
        "fn shared_name_v2() {}",
        "proj-y",
        Some("main"),
        "/src/y.rs",
    ));
    processor.flush(0).await.unwrap();

    // Scoped search for proj-x only
    let rows = sqlx::query(crate::code_lines_schema::FTS5_SEARCH_BY_PROJECT_SQL)
        .bind("shared_name")
        .bind("proj-x")
        .fetch_all(db.pool())
        .await
        .unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].get::<String, _>("tenant_id"), "proj-x");

    db.close().await;
}

#[tokio::test]
async fn test_large_batch_throughput() {
    let (_tmp, db) = setup_db().await;
    let mut processor = FtsBatchProcessor::new(&db, FtsBatchConfig::default());

    // Simulate 50 files x 300 lines each = 15,000 lines
    for i in 1..=50 {
        let content: String = (0..300)
            .map(|j| format!("fn file{}_line{}() {{}}", i, j))
            .collect::<Vec<_>>()
            .join("\n");

        processor.add_change(test_change(
            i,
            "",
            &content,
            "proj-perf",
            Some("main"),
            &format!("/src/file{}.rs", i),
        ));
    }

    // Batch mode
    let stats = processor.flush(100).await.unwrap();
    assert_eq!(stats.files_processed, 50);
    assert_eq!(stats.lines_inserted, 15_000);
    assert!(stats.batch_mode);

    // Should complete in reasonable time (< 30s for 15K lines).
    // 30s provides headroom for system load variability while still catching
    // catastrophic performance regressions.
    assert!(
        stats.processing_time_ms < 30_000,
        "Batch processing took {}ms, expected < 30000ms",
        stats.processing_time_ms
    );

    // Verify total count
    let count: i32 = sqlx::query_scalar("SELECT COUNT(*) FROM code_lines")
        .fetch_one(db.pool())
        .await
        .unwrap();
    assert_eq!(count, 15_000);

    db.close().await;
}

#[tokio::test]
async fn test_custom_burst_threshold() {
    let (_tmp, db) = setup_db().await;
    let config = FtsBatchConfig { burst_threshold: 5 };
    let processor = FtsBatchProcessor::new(&db, config);

    assert!(!processor.should_use_batch_mode(4));
    assert!(!processor.should_use_batch_mode(5));
    assert!(processor.should_use_batch_mode(6));
}

#[tokio::test]
async fn test_delete_nonexistent_file() {
    let (_tmp, db) = setup_db().await;
    let processor = FtsBatchProcessor::new(&db, FtsBatchConfig::default());

    let deleted = processor.delete_file(999).await.unwrap();
    assert_eq!(deleted, 0);

    db.close().await;
}

#[tokio::test]
async fn test_delete_nonexistent_tenant() {
    let (_tmp, db) = setup_db().await;
    let processor = FtsBatchProcessor::new(&db, FtsBatchConfig::default());

    let deleted = processor.delete_tenant("nonexistent").await.unwrap();
    assert_eq!(deleted, 0);

    db.close().await;
}
