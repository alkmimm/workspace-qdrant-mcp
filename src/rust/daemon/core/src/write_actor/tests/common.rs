//! Shared test helpers for WriteActor tests.

use sqlx::SqlitePool;

use crate::write_actor::actor::{WriteActor, WriteActorHandle};

/// Create the `unified_queue` table using the PRODUCTION DDL.
///
/// The enqueue INSERT names only the columns it binds and relies on the
/// table defaults for `queue_id`, `status`, `created_at`, `updated_at`. A
/// hand-rolled DDL without those defaults makes `INSERT OR IGNORE` swallow
/// the NOT NULL violation and report 0 rows — exec paths that enqueue
/// (e.g. ReembedTenant) then fail with a misleading dedup error.
async fn setup_queue_table(pool: &SqlitePool) {
    crate::queue_operations::QueueManager::new(pool.clone())
        .init_unified_queue()
        .await
        .unwrap();
}

/// Create the `watch_folders` table.
async fn setup_watch_folders_table(pool: &SqlitePool) {
    sqlx::query(
        "CREATE TABLE watch_folders (
            watch_id TEXT PRIMARY KEY,
            path TEXT NOT NULL,
            collection TEXT NOT NULL,
            tenant_id TEXT NOT NULL,
            enabled INTEGER NOT NULL DEFAULT 1,
            is_active INTEGER NOT NULL DEFAULT 0,
            is_paused INTEGER NOT NULL DEFAULT 0,
            is_archived INTEGER DEFAULT 0,
            pause_start_time TEXT,
            library_mode TEXT,
            follow_symlinks INTEGER DEFAULT 0,
            cleanup_on_disable INTEGER DEFAULT 0,
            parent_watch_id TEXT,
            remote_hash TEXT,
            git_remote_url TEXT,
            last_activity_at TEXT,
            created_at TEXT NOT NULL,
            updated_at TEXT NOT NULL
        )",
    )
    .execute(pool)
    .await
    .unwrap();
}

/// Create the `tracked_files` table.
async fn setup_tracked_files_table(pool: &SqlitePool) {
    sqlx::query(
        "CREATE TABLE tracked_files (
            file_id INTEGER PRIMARY KEY AUTOINCREMENT,
            watch_folder_id TEXT NOT NULL,
            file_path TEXT NOT NULL,
            tenant_id TEXT,
            branch TEXT DEFAULT 'main',
            incremental INTEGER DEFAULT 0,
            created_at TEXT NOT NULL,
            updated_at TEXT NOT NULL
        )",
    )
    .execute(pool)
    .await
    .unwrap();
}

/// Create the `project_components` table.
async fn setup_project_components_table(pool: &SqlitePool) {
    sqlx::query(
        "CREATE TABLE project_components (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            watch_folder_id TEXT NOT NULL,
            name TEXT NOT NULL
        )",
    )
    .execute(pool)
    .await
    .unwrap();
}

/// Create the `search_events` table using the PRODUCTION DDL (+ the v38
/// column ALTERs migrations apply on top).
///
/// The first cut of this fixture hand-rolled the columns WITHOUT the
/// actor/tool/op CHECK constraints — so `exec_log_search_event` tests passed
/// with op values the LIVE table rejects, and the op='rules' silent
/// CHECK-violation loss (v47) shipped green. Same trap as the tracked_files
/// `file_path` fixture (#265): keep this table shaped like the real
/// migrations, not like what the test would like to exist.
async fn setup_search_events_table(pool: &SqlitePool) {
    sqlx::query(crate::search_events_schema::CREATE_SEARCH_EVENTS_SQL)
        .execute(pool)
        .await
        .unwrap();
    for alter in [
        "ALTER TABLE search_events ADD COLUMN bytes_in INTEGER",
        "ALTER TABLE search_events ADD COLUMN bytes_out INTEGER",
        "ALTER TABLE search_events ADD COLUMN hits_truncated INTEGER",
        "ALTER TABLE search_events ADD COLUMN shape_mode TEXT",
        "ALTER TABLE search_events ADD COLUMN tool_version TEXT",
    ] {
        sqlx::query(alter).execute(pool).await.unwrap();
    }
}

/// Create the `rules_mirror` table.
async fn setup_rules_mirror_table(pool: &SqlitePool) {
    sqlx::query(
        "CREATE TABLE rules_mirror (
            rule_id TEXT PRIMARY KEY,
            rule_text TEXT NOT NULL,
            scope TEXT,
            tenant_id TEXT,
            created_at TEXT NOT NULL,
            updated_at TEXT NOT NULL
        )",
    )
    .execute(pool)
    .await
    .unwrap();
}

/// Create the `corpus_statistics` table.
async fn setup_corpus_statistics_table(pool: &SqlitePool) {
    sqlx::query(
        "CREATE TABLE corpus_statistics (
            collection TEXT PRIMARY KEY,
            last_corrected_n INTEGER DEFAULT 0
        )",
    )
    .execute(pool)
    .await
    .unwrap();
}

/// Create the `watch_folder_submodules` table.
async fn setup_watch_folder_submodules_table(pool: &SqlitePool) {
    sqlx::query(
        "CREATE TABLE watch_folder_submodules (
            parent_watch_id TEXT NOT NULL,
            child_watch_id TEXT NOT NULL,
            PRIMARY KEY (parent_watch_id, child_watch_id)
        )",
    )
    .execute(pool)
    .await
    .unwrap();
}

/// Create an in-memory SQLite pool with the minimal schema needed by the
/// WriteActor exec methods, then spawn the actor and return both.
pub async fn setup_test_db() -> (SqlitePool, WriteActorHandle) {
    let pool = SqlitePool::connect("sqlite::memory:")
        .await
        .expect("Failed to create in-memory SQLite pool");

    setup_queue_table(&pool).await;
    setup_watch_folders_table(&pool).await;
    setup_tracked_files_table(&pool).await;
    setup_project_components_table(&pool).await;
    setup_search_events_table(&pool).await;
    setup_rules_mirror_table(&pool).await;
    setup_corpus_statistics_table(&pool).await;
    setup_watch_folder_submodules_table(&pool).await;

    let handle = WriteActor::spawn(pool.clone());
    (pool, handle)
}
