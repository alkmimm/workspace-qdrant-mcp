//! Tests for exact substring search.

mod context_tests;
mod glob_tests;
mod search_tests;

use crate::code_lines_schema::{initial_seq, UPSERT_FILE_METADATA_SQL};
use crate::search_db::SearchDbManager;

pub(super) async fn setup_search_db() -> (tempfile::TempDir, SearchDbManager) {
    let tmp = tempfile::tempdir().unwrap();
    let db_path = tmp.path().join("test_search.db");
    let manager = SearchDbManager::new(&db_path).await.unwrap();
    (tmp, manager)
}

pub(super) async fn insert_file_content(
    db: &SearchDbManager,
    file_id: i64,
    lines: &[&str],
    tenant_id: &str,
    branch: Option<&str>,
    file_path: &str,
) {
    let pool = db.pool();
    for (i, line) in lines.iter().enumerate() {
        let seq = initial_seq(i);
        let line_number = (i + 1) as i64;
        sqlx::query(
            "INSERT INTO code_lines (file_id, seq, content, line_number) VALUES (?1, ?2, ?3, ?4)",
        )
        .bind(file_id)
        .bind(seq)
        .bind(*line)
        .bind(line_number)
        .execute(pool)
        .await
        .unwrap();
    }
    sqlx::query(UPSERT_FILE_METADATA_SQL)
        .bind(file_id)
        .bind(tenant_id)
        .bind(branch) // ?3 — wrapped into the branches JSON set by the UPSERT
        .bind(file_path)
        .bind(None::<&str>) // ?5 base_point
        .bind(None::<&str>) // ?6 relative_path
        .bind(None::<&str>) // ?7 file_hash
        .bind(None::<i64>) // ?8 size_bytes
        .bind(0_i64) // ?9 fts5_skipped
        .execute(pool)
        .await
        .unwrap();
    db.rebuild_fts().await.unwrap();
}
