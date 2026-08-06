//! Graph database manager and schema migrations.
//!
//! Manages `graph.db` — a dedicated SQLite database for code relationship
//! storage, separate from `state.db` to avoid lock contention with queue ops.

use std::path::{Path, PathBuf};

use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};
use sqlx::SqlitePool;
use thiserror::Error;
use tracing::{debug, info, warn};

/// Current schema version for graph.db.
pub const GRAPH_SCHEMA_VERSION: i32 = 6;

/// Default graph database filename.
pub const GRAPH_DB_FILENAME: &str = "graph.db";

/// Errors from graph database operations.
#[derive(Error, Debug)]
pub enum GraphDbError {
    #[error("Database error: {0}")]
    Database(#[from] sqlx::Error),

    #[error("Schema migration error: {0}")]
    Migration(String),

    #[error(
        "Downgrade not supported: database version {db_version} > code version {code_version}"
    )]
    DowngradeNotSupported { db_version: i32, code_version: i32 },

    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),

    #[error("Node not found: {0}")]
    NotFound(String),

    #[error("Invalid input: {0}")]
    InvalidInput(String),
}

/// Result type for graph database operations.
pub type GraphDbResult<T> = Result<T, GraphDbError>;

/// Graph database manager.
///
/// Manages a separate SQLite database (`graph.db`) with independent schema
/// versioning, WAL mode, and foreign keys.
pub struct GraphDbManager {
    pool: SqlitePool,
    path: PathBuf,
}

impl GraphDbManager {
    /// Create a new graph database manager.
    ///
    /// Opens (or creates) the database, enables WAL mode and foreign keys,
    /// then runs any pending schema migrations.
    pub async fn new<P: AsRef<Path>>(database_path: P) -> GraphDbResult<Self> {
        let path = database_path.as_ref().to_path_buf();
        info!("Initializing graph database: {}", path.display());

        let connect_options = SqliteConnectOptions::new()
            .filename(&path)
            .create_if_missing(true)
            .journal_mode(sqlx::sqlite::SqliteJournalMode::Wal)
            .foreign_keys(true);

        let pool = SqlitePoolOptions::new()
            .max_connections(5)
            .connect_with(connect_options)
            .await?;

        // Verify WAL mode
        let journal_mode: String = sqlx::query_scalar("PRAGMA journal_mode")
            .fetch_one(&pool)
            .await?;
        if journal_mode.to_lowercase() != "wal" {
            warn!("Expected WAL journal mode, got '{}'", journal_mode);
        } else {
            debug!("WAL mode confirmed for graph.db");
        }

        let manager = Self { pool, path };
        manager.run_migrations().await?;

        Ok(manager)
    }

    /// Create a manager from an existing pool (for tests).
    pub fn with_pool(pool: SqlitePool, path: PathBuf) -> Self {
        Self { pool, path }
    }

    /// Get a reference to the connection pool.
    pub fn pool(&self) -> &SqlitePool {
        &self.pool
    }

    /// Get the database file path.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Close the database connection pool.
    pub async fn close(&self) {
        info!("Closing graph database: {}", self.path.display());
        self.pool.close().await;
    }

    /// Run pending schema migrations.
    async fn run_migrations(&self) -> GraphDbResult<()> {
        // Create schema version table
        sqlx::query(
            "CREATE TABLE IF NOT EXISTS graph_schema_version (
                version INTEGER PRIMARY KEY,
                applied_at TEXT DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now'))
            )",
        )
        .execute(&self.pool)
        .await?;

        let current: Option<i32> =
            sqlx::query_scalar("SELECT MAX(version) FROM graph_schema_version")
                .fetch_optional(&self.pool)
                .await?
                .flatten();
        let current = current.unwrap_or(0);

        info!(
            "Graph schema version: {}, target: {}",
            current, GRAPH_SCHEMA_VERSION
        );

        if current > GRAPH_SCHEMA_VERSION {
            return Err(GraphDbError::DowngradeNotSupported {
                db_version: current,
                code_version: GRAPH_SCHEMA_VERSION,
            });
        }

        if current == GRAPH_SCHEMA_VERSION {
            debug!("Graph schema is up to date");
            return Ok(());
        }

        for version in (current + 1)..=GRAPH_SCHEMA_VERSION {
            info!("Running graph migration to version {}", version);
            self.run_migration(version).await?;
            sqlx::query("INSERT INTO graph_schema_version (version) VALUES (?1)")
                .bind(version)
                .execute(&self.pool)
                .await?;
        }

        info!(
            "Graph schema migrations complete. Now at version {}",
            GRAPH_SCHEMA_VERSION
        );
        Ok(())
    }

    async fn run_migration(&self, version: i32) -> GraphDbResult<()> {
        match version {
            1 => self.migrate_v1().await,
            2 => self.migrate_v2().await,
            3 => self.migrate_v3().await,
            4 => self.migrate_v4().await,
            5 => self.migrate_v5().await,
            6 => self.migrate_v6().await,
            _ => Err(GraphDbError::Migration(format!(
                "Unknown graph migration version: {}",
                version
            ))),
        }
    }

    async fn migrate_v1(&self) -> GraphDbResult<()> {
        info!("Graph migration v1: creating nodes and edges tables");

        let mut tx = self.pool.begin().await?;

        // Nodes table
        sqlx::query(
            "CREATE TABLE graph_nodes (
                node_id TEXT PRIMARY KEY,
                tenant_id TEXT NOT NULL,
                symbol_name TEXT NOT NULL,
                symbol_type TEXT NOT NULL,
                file_path TEXT NOT NULL,
                start_line INTEGER,
                end_line INTEGER,
                signature TEXT,
                language TEXT,
                created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
                updated_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now'))
            )",
        )
        .execute(&mut *tx)
        .await?;

        sqlx::query("CREATE INDEX idx_nodes_tenant ON graph_nodes(tenant_id)")
            .execute(&mut *tx)
            .await?;
        sqlx::query("CREATE INDEX idx_nodes_file ON graph_nodes(tenant_id, file_path)")
            .execute(&mut *tx)
            .await?;
        sqlx::query("CREATE INDEX idx_nodes_symbol ON graph_nodes(tenant_id, symbol_name)")
            .execute(&mut *tx)
            .await?;

        // Edges table
        sqlx::query(
            "CREATE TABLE graph_edges (
                edge_id TEXT PRIMARY KEY,
                tenant_id TEXT NOT NULL,
                source_node_id TEXT NOT NULL,
                target_node_id TEXT NOT NULL,
                edge_type TEXT NOT NULL,
                source_file TEXT NOT NULL,
                weight REAL DEFAULT 1.0,
                metadata_json TEXT,
                created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
                FOREIGN KEY (source_node_id) REFERENCES graph_nodes(node_id),
                FOREIGN KEY (target_node_id) REFERENCES graph_nodes(node_id)
            )",
        )
        .execute(&mut *tx)
        .await?;

        sqlx::query("CREATE INDEX idx_edges_tenant ON graph_edges(tenant_id)")
            .execute(&mut *tx)
            .await?;
        sqlx::query("CREATE INDEX idx_edges_source ON graph_edges(source_node_id)")
            .execute(&mut *tx)
            .await?;
        sqlx::query("CREATE INDEX idx_edges_target ON graph_edges(target_node_id)")
            .execute(&mut *tx)
            .await?;
        sqlx::query("CREATE INDEX idx_edges_source_file ON graph_edges(tenant_id, source_file)")
            .execute(&mut *tx)
            .await?;
        sqlx::query("CREATE INDEX idx_edges_type ON graph_edges(edge_type)")
            .execute(&mut *tx)
            .await?;

        tx.commit().await?;
        Ok(())
    }

    /// v2: covering index for the per-`(tenant_id, edge_type)` aggregate that the
    /// graph-metrics exporter runs on a timer. Without it, `GROUP BY tenant_id,
    /// edge_type` falls back to a full table scan + sort (~1.2s on a ~700k-edge
    /// graph — the recurring "slow statement" in the logs); the composite index
    /// turns it into an index-only group scan. `IF NOT EXISTS` keeps the
    /// migration idempotent on a partially-applied DB.
    async fn migrate_v2(&self) -> GraphDbResult<()> {
        info!("Graph migration v2: adding covering index idx_edges_tenant_type");
        sqlx::query(
            "CREATE INDEX IF NOT EXISTS idx_edges_tenant_type \
             ON graph_edges(tenant_id, edge_type)",
        )
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// v3: partial index once meant to isolate file-less "stub" nodes for the
    /// periodic stub-edge resolver. SUPERSEDED by v4, which drops it: the
    /// daemon's bundled SQLite could not prove the query's
    /// `file_path IS NULL OR file_path = ''` predicate implied this partial
    /// index's WHERE, so forcing it via `INDEXED BY` failed the whole query
    /// with "no query solution". The resolver now uses the existing plain
    /// index `idx_nodes_file(tenant_id, file_path)` via `file_path = ''`
    /// (`file_path` is NOT NULL, so the `IS NULL` branch was always dead) —
    /// see `resolve_stub_edges` in sqlite_store.rs. Kept as a historical step
    /// so already-migrated databases stay on a linear version chain.
    async fn migrate_v3(&self) -> GraphDbResult<()> {
        info!("Graph migration v3: adding partial index idx_nodes_fileless");
        sqlx::query(
            "CREATE INDEX IF NOT EXISTS idx_nodes_fileless \
             ON graph_nodes(tenant_id, node_id) WHERE file_path IS NULL OR file_path = ''",
        )
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// v4: drop the redundant v3 partial index `idx_nodes_fileless`. The
    /// stub-edge resolver drives its file-less-node scan from the existing
    /// plain composite index `idx_nodes_file(tenant_id, file_path)` (via
    /// `file_path = ''`), which the planner picks on its own with no stats and
    /// no `INDEXED BY` hint — so the partial index is dead weight. Dropping it
    /// also removes the "no query solution" trap: a future `INDEXED BY
    /// idx_nodes_fileless` would fail on the daemon's SQLite (weaker
    /// partial-index prover). `IF EXISTS` keeps the migration idempotent.
    async fn migrate_v4(&self) -> GraphDbResult<()> {
        info!("Graph migration v4: dropping redundant partial index idx_nodes_fileless");
        sqlx::query("DROP INDEX IF EXISTS idx_nodes_fileless")
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    /// v5: add `is_test_symbol` to `graph_nodes`. Tags a symbol as TEST code
    /// independent of file path — a Rust inline unit test (`#[cfg(test)]` module
    /// or `#[test]`-family attribute) that shares a production `.rs` file. Set at
    /// (re)extraction time; existing rows default to 0 until the tenant is
    /// re-indexed. Consumed by `detect_test_gaps`, which seeds the coverage BFS
    /// from `is_test_file(path) OR is_test_symbol` so inline tests count. SQLite
    /// `ADD COLUMN` is non-rewriting and idempotent-safe here because the column
    /// only ever exists once this migration runs on a linear version chain.
    async fn migrate_v5(&self) -> GraphDbResult<()> {
        info!("Graph migration v5: adding graph_nodes.is_test_symbol");
        sqlx::query(
            "ALTER TABLE graph_nodes ADD COLUMN is_test_symbol INTEGER NOT NULL DEFAULT 0",
        )
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// v6: composite indexes for the per-file edge cleanup that runs on every
    /// re-ingest (`delete_file_nodes_except` in sqlite_store.rs). Those deletes
    /// filter `tenant_id = ? AND source_node_id IN (…)` (and the same on
    /// `target_node_id`), but the only candidate indexes were single-column
    /// (`idx_edges_source` / `idx_edges_target`, no tenant) and `idx_edges_tenant`
    /// (tenant only). With no ANALYZE stats the planner picked `idx_edges_tenant`
    /// and SCANNED every edge of the tenant on each delete — measured ~1–19s on
    /// the live graph (largest tenant ~4M edges, the recurring "slow statement").
    /// These composites let it probe `(tenant_id, node_id)` directly, turning the
    /// scan into a handful of lookups. Same shape of fix as v2; `IF NOT EXISTS`
    /// keeps the migration idempotent on a partially-applied DB.
    async fn migrate_v6(&self) -> GraphDbResult<()> {
        info!("Graph migration v6: adding composite indexes idx_edges_tenant_source / idx_edges_tenant_target");
        sqlx::query(
            "CREATE INDEX IF NOT EXISTS idx_edges_tenant_source \
             ON graph_edges(tenant_id, source_node_id)",
        )
        .execute(&self.pool)
        .await?;
        sqlx::query(
            "CREATE INDEX IF NOT EXISTS idx_edges_tenant_target \
             ON graph_edges(tenant_id, target_node_id)",
        )
        .execute(&self.pool)
        .await?;
        Ok(())
    }
}

impl Clone for GraphDbManager {
    fn clone(&self) -> Self {
        Self {
            pool: self.pool.clone(),
            path: self.path.clone(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn migrations_apply_indexes_and_are_idempotent() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let db = tmp.path().join("graph.db");

        // `new` opens the DB and runs all pending migrations (v1 → v5).
        let mgr = GraphDbManager::new(&db).await.expect("open graph db");

        // Schema lands at the latest version.
        let version: i32 = sqlx::query_scalar("SELECT MAX(version) FROM graph_schema_version")
            .fetch_one(&mgr.pool)
            .await
            .expect("read schema version");
        assert_eq!(version, GRAPH_SCHEMA_VERSION);
        assert_eq!(version, 6, "v6 must be applied");

        // v5 adds the is_test_symbol column to graph_nodes.
        let has_is_test: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM pragma_table_info('graph_nodes') WHERE name = 'is_test_symbol'",
        )
        .fetch_one(&mgr.pool)
        .await
        .expect("query pragma_table_info");
        assert_eq!(has_is_test, 1, "v5 must add graph_nodes.is_test_symbol");

        // The v2 covering index for the metrics aggregate exists.
        let idx_v2: Option<String> = sqlx::query_scalar(
            "SELECT name FROM sqlite_master \
             WHERE type = 'index' AND name = 'idx_edges_tenant_type'",
        )
        .fetch_optional(&mgr.pool)
        .await
        .expect("query sqlite_master");
        assert_eq!(idx_v2.as_deref(), Some("idx_edges_tenant_type"));

        // v4 drops the redundant v3 partial index — it must NOT exist at head.
        let idx_fileless: Option<String> = sqlx::query_scalar(
            "SELECT name FROM sqlite_master \
             WHERE type = 'index' AND name = 'idx_nodes_fileless'",
        )
        .fetch_optional(&mgr.pool)
        .await
        .expect("query sqlite_master");
        assert_eq!(idx_fileless, None, "idx_nodes_fileless must be dropped by v4");

        // The plain index the stub-edge resolver actually drives from (created
        // in v1) must exist.
        let idx_file: Option<String> = sqlx::query_scalar(
            "SELECT name FROM sqlite_master \
             WHERE type = 'index' AND name = 'idx_nodes_file'",
        )
        .fetch_optional(&mgr.pool)
        .await
        .expect("query sqlite_master");
        assert_eq!(idx_file.as_deref(), Some("idx_nodes_file"));

        // v6 adds the composite indexes that make the per-file edge-cleanup
        // delete probe (tenant_id, node_id) instead of scanning the tenant.
        for idx in ["idx_edges_tenant_source", "idx_edges_tenant_target"] {
            let found: Option<String> = sqlx::query_scalar(
                "SELECT name FROM sqlite_master WHERE type = 'index' AND name = ?1",
            )
            .bind(idx)
            .fetch_optional(&mgr.pool)
            .await
            .expect("query sqlite_master");
            assert_eq!(found.as_deref(), Some(idx), "v6 must create {idx}");
        }

        // Re-running migrations on an up-to-date DB is a no-op (no error, no
        // duplicate-index failure thanks to IF NOT EXISTS / early return).
        mgr.run_migrations().await.expect("re-run is idempotent");
    }
}
