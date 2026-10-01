//! `path_exclude` in the exact engine: applied before the cap.
//!
//! Before it reached the daemon, the MCP server applied `pathExclude` only to the
//! rows this engine had already returned. A page fetches `offset + maxResults`
//! rows, so excluded paths consumed the page first — measured live: an empty
//! page, `truncated: true`, no continuation, and a total of 2021 for 13 real hits.

use super::super::super::types::SearchOptions;
use super::super::search::search_exact;
use super::{insert_file_content, setup_search_db};

/// Five excluded files first in index order, one kept file last — the shape that
/// emptied the page when the exclude ran after the cap.
async fn seed_excluded_first() -> (tempfile::TempDir, crate::search_db::SearchDbManager) {
    let (tmp, db) = setup_search_db().await;
    for id in 1..=5 {
        insert_file_content(
            &db,
            id,
            &["pub fn excluded() {}"],
            "proj1",
            Some("main"),
            &format!("/repo/src/rust/m{id}.rs"),
        )
        .await;
    }
    insert_file_content(
        &db,
        6,
        &["pub fn kept() {}"],
        "proj1",
        Some("main"),
        "/repo/docs/example.md",
    )
    .await;
    (tmp, db)
}

#[tokio::test]
async fn path_exclude_does_not_spend_the_result_cap() {
    let (_tmp, db) = seed_excluded_first().await;
    let results = search_exact(
        &db,
        "pub fn",
        &SearchOptions {
            path_exclude: Some("src/rust/**".to_string()),
            // Two slots: filtering after the cap would fill both with excluded
            // rows. (With ONE slot and one hit the engine reports `truncated` —
            // it flags "cap reached, there may be more" without looking further.)
            max_results: 2,
            ..Default::default()
        },
    )
    .await
    .unwrap();

    assert_eq!(
        results.matches.len(),
        1,
        "only the kept hit, no excluded row"
    );
    assert_eq!(results.matches[0].file_path, "/repo/docs/example.md");
    assert!(!results.truncated, "nothing else survives the exclude");
}

#[tokio::test]
async fn path_exclude_count_is_post_filter() {
    let (_tmp, db) = seed_excluded_first().await;
    let results = search_exact(
        &db,
        "pub fn",
        &SearchOptions {
            path_exclude: Some("src/rust/**".to_string()),
            ..Default::default()
        },
    )
    .await
    .unwrap();

    // The gRPC layer reports `matches.len()` as `total_matches`, so this IS the
    // count a caller sees: 1, not 6.
    assert_eq!(results.matches.len(), 1);
}

#[tokio::test]
async fn path_exclude_single_star_does_not_over_exclude() {
    let (_tmp, db) = setup_search_db().await;
    insert_file_content(&db, 1, &["needle"], "proj1", Some("main"), "/repo/src/x.rs").await;
    insert_file_content(
        &db,
        2,
        &["needle"],
        "proj1",
        Some("main"),
        "/repo/src/deep/x.rs",
    )
    .await;

    let results = search_exact(
        &db,
        "needle",
        &SearchOptions {
            path_exclude: Some("src/*.rs".to_string()),
            ..Default::default()
        },
    )
    .await
    .unwrap();

    // `*` stops at `/` for an exclude — the include matcher would have matched
    // the nested file too, deleting a hit the MCP server keeps.
    let paths: Vec<_> = results
        .matches
        .iter()
        .map(|m| m.file_path.as_str())
        .collect();
    assert_eq!(paths, vec!["/repo/src/deep/x.rs"]);
}

#[tokio::test]
async fn path_exclude_composes_with_include_glob_and_generation_dedup() {
    let (_tmp, db) = setup_search_db().await;
    // Two content generations of one kept file (identical line) + one excluded.
    insert_file_content(&db, 1, &["needle"], "proj1", Some("main"), "/repo/src/a.rs").await;
    insert_file_content(&db, 2, &["needle"], "proj1", Some("main"), "/repo/src/a.rs").await;
    insert_file_content(
        &db,
        3,
        &["needle"],
        "proj1",
        Some("main"),
        "/repo/src/old_project/b.rs",
    )
    .await;

    let results = search_exact(
        &db,
        "needle",
        &SearchOptions {
            path_glob: Some("src/**".to_string()),
            path_exclude: Some("old_project/**".to_string()),
            ..Default::default()
        },
    )
    .await
    .unwrap();

    let paths: Vec<_> = results
        .matches
        .iter()
        .map(|m| m.file_path.as_str())
        .collect();
    assert_eq!(
        paths,
        vec!["/repo/src/a.rs"],
        "one hit: excluded and duplicate gone"
    );
}
