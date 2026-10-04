use super::super::*;
use super::{create_test_pool, setup_tables};

async fn track(pool: &sqlx::SqlitePool, path: &str, branch: &str, hash: &str) {
    insert_tracked_file(
        pool,
        "w1",
        path,
        Some(branch),
        Some("code"),
        Some("typescript"),
        "2026-10-03T00:00:00Z",
        hash,
        1,
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
    .expect("insert");
}

/// Regression (2026-10-03): the startup recovery compares rows with the MAIN
/// folder, which is the checkout of its HEAD branch only. Scoped to that
/// branch, it no longer sees a linked worktree's generations — a
/// worktree-only file (`sync.ts`) read as deleted and a divergent one
/// (`page.tsx`) as modified at every restart.
#[tokio::test]
async fn tracked_files_with_hashes_scopes_to_one_branch() {
    let pool = create_test_pool().await;
    setup_tables(&pool).await;

    track(&pool, "app/page.tsx", "develop", "h-develop").await;
    track(&pool, "app/page.tsx", "feat/x", "h-feat").await;
    track(&pool, "app/lib/actions/sync.ts", "feat/x", "h-sync").await;

    let mut develop = get_tracked_files_with_hashes(&pool, "w1", Some("develop"))
        .await
        .unwrap();
    develop.sort();
    assert_eq!(develop.len(), 1, "got {develop:?}");
    assert_eq!(develop[0].0, "app/page.tsx");
    assert_eq!(develop[0].1, "h-develop");

    let feat = get_tracked_files_with_hashes(&pool, "w1", Some("feat/x"))
        .await
        .unwrap();
    assert_eq!(feat.len(), 2, "both feat/x generations: {feat:?}");

    // No branch to scope by (non-git folder, detached HEAD): every row.
    let all = get_tracked_files_with_hashes(&pool, "w1", None)
        .await
        .unwrap();
    assert_eq!(all.len(), 3);
}
