//! Candidate selection of the bulk branch-membership append.

use super::{select_branch_bulk_candidates, select_branch_bulk_candidates_by_hash};
use sqlx::sqlite::SqlitePoolOptions;
use sqlx::SqlitePool;

async fn pool_with_two_generations() -> SqlitePool {
    let pool = SqlitePoolOptions::new()
        .max_connections(1)
        .connect("sqlite::memory:")
        .await
        .expect("in-memory pool");
    // Production DDL (same constants SchemaManager applies).
    sqlx::query(crate::watch_folders_schema::CREATE_WATCH_FOLDERS_SQL)
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query(crate::tracked_files_schema::CREATE_TRACKED_FILES_V41_SQL)
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO watch_folders (watch_id, path, collection, tenant_id, created_at, updated_at)
         VALUES ('w1', '/home/u/project', 'projects', 't1', '2025-01-01T00:00:00Z', '2025-01-01T00:00:00Z')",
    )
    .execute(&pool)
    .await
    .unwrap();

    // The live #224 shape: one path, TWO generations. The generation the
    // no-diff verification certified (holds the old branch `main`) and a
    // stale Layer-2 debris generation held by an unrelated dead branch.
    for (hash, bp, branches) in [
        ("hash-current", "bp-current", r#"["main"]"#),
        ("hash-debris", "bp-debris", r#"["feat/long-dead"]"#),
    ] {
        sqlx::query(
            "INSERT INTO tracked_files
               (watch_folder_id, relative_path, file_hash, base_point, branches,
                file_mtime, chunk_count, created_at, updated_at)
             VALUES ('w1', 'src/lib.rs', ?1, ?2, ?3,
                     '2025-01-01T00:00:00Z', 1, '2025-01-01T00:00:00Z', '2025-01-01T00:00:00Z')",
        )
        .bind(hash)
        .bind(bp)
        .bind(branches)
        .execute(&pool)
        .await
        .unwrap();
    }
    pool
}

/// The #224 fabricator regression: with `from_branch` present, the bulk
/// re-key must tag ONLY the generation the no-diff verification compared —
/// never the other generations of the path (stale debris included).
#[tokio::test]
async fn bulk_candidates_skip_other_generation_debris() {
    let pool = pool_with_two_generations().await;
    let paths = vec!["src/lib.rs".to_string()];

    let scoped =
        select_branch_bulk_candidates(&pool, "w1", "feat/new-branch", Some("main"), &paths)
            .await
            .unwrap();
    assert_eq!(
        scoped,
        vec!["bp-current".to_string()],
        "only the generation holding the from_branch is certified content"
    );

    // A from_branch nothing holds selects nothing — the per-file reconcile
    // (hash-gated) is the repair path, never a blind tag.
    let none_hold =
        select_branch_bulk_candidates(&pool, "w1", "feat/new-branch", Some("ghost"), &paths)
            .await
            .unwrap();
    assert!(none_hold.is_empty());
}

/// Legacy ops (enqueued before `from_branch` existed) keep the unscoped
/// behaviour; and the `branch NOT EXISTS` resume filter still excludes
/// generations already tagged.
#[tokio::test]
async fn bulk_candidates_legacy_none_still_tags_every_other_generation() {
    let pool = pool_with_two_generations().await;
    let paths = vec!["src/lib.rs".to_string()];

    let mut legacy = select_branch_bulk_candidates(&pool, "w1", "feat/new-branch", None, &paths)
        .await
        .unwrap();
    legacy.sort();
    assert_eq!(
        legacy,
        vec!["bp-current".to_string(), "bp-debris".to_string()],
        "None = pre-field op = legacy unscoped candidate set"
    );

    // Already-tagged generations are excluded (the re-lease resume filter).
    sqlx::query(
        "UPDATE tracked_files SET branches = json_insert(branches, '$[#]', 'feat/new-branch')
         WHERE base_point = 'bp-current'",
    )
    .execute(&pool)
    .await
    .unwrap();
    let resumed =
        select_branch_bulk_candidates(&pool, "w1", "feat/new-branch", Some("main"), &paths)
            .await
            .unwrap();
    assert!(
        resumed.is_empty(),
        "generation already carrying the branch is skipped on resume"
    );
}

fn one(path: &str) -> Vec<String> {
    vec![path.to_string()]
}

/// Hash mode certifies exactly the generation with the listed content —
/// whichever branch holds it (here a branch that is not the trunk) — and
/// nothing for a hash the index does not hold.
#[tokio::test]
async fn hash_mode_selects_the_generation_with_that_content() {
    let pool = pool_with_two_generations().await;
    let path = one("src/lib.rs");

    let current =
        select_branch_bulk_candidates_by_hash(&pool, "w1", "agent/x", &path, &one("hash-current"))
            .await
            .unwrap();
    assert_eq!(current, vec!["bp-current".to_string()]);

    let sibling =
        select_branch_bulk_candidates_by_hash(&pool, "w1", "agent/x", &path, &one("hash-debris"))
            .await
            .unwrap();
    assert_eq!(
        sibling,
        vec!["bp-debris".to_string()],
        "content held only by another branch is certified by its hash"
    );

    let unknown =
        select_branch_bulk_candidates_by_hash(&pool, "w1", "agent/x", &path, &one("hash-new"))
            .await
            .unwrap();
    assert!(unknown.is_empty(), "content nobody indexed stays per-file");
}

/// A branch keeps one generation per path: once it holds ANY generation of
/// the path (the live watcher already moved it, or an earlier pass tagged
/// it), a hash-matched op adds no second one.
#[tokio::test]
async fn hash_mode_never_gives_a_branch_two_generations_of_a_path() {
    let pool = pool_with_two_generations().await;
    sqlx::query(
        "UPDATE tracked_files SET branches = json_insert(branches, '$[#]', 'agent/x')
         WHERE base_point = 'bp-current'",
    )
    .execute(&pool)
    .await
    .unwrap();
    let picked = select_branch_bulk_candidates_by_hash(
        &pool,
        "w1",
        "agent/x",
        &one("src/lib.rs"),
        &one("hash-debris"),
    )
    .await
    .unwrap();
    assert!(picked.is_empty(), "{picked:?}");
}

/// A generation no branch holds any more is debris whose vectors may be
/// gone: never certified, even when its hash matches.
#[tokio::test]
async fn hash_mode_skips_generations_no_branch_holds() {
    let pool = pool_with_two_generations().await;
    sqlx::query("UPDATE tracked_files SET branches = '[]' WHERE base_point = 'bp-debris'")
        .execute(&pool)
        .await
        .unwrap();
    let picked = select_branch_bulk_candidates_by_hash(
        &pool,
        "w1",
        "agent/x",
        &one("src/lib.rs"),
        &one("hash-debris"),
    )
    .await
    .unwrap();
    assert!(picked.is_empty(), "{picked:?}");

    let misaligned =
        select_branch_bulk_candidates_by_hash(&pool, "w1", "agent/x", &one("src/lib.rs"), &[])
            .await;
    assert!(misaligned.is_err(), "paths and hashes must be aligned");
}
