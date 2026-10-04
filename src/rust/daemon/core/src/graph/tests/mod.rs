//! Unit tests for graph module: CRUD, recursive CTE queries, impact analysis.
//!
//! Split into focused submodules:
//! - `id_tests`: Node/edge ID determinism, constructors, enum round-trips
//! - `store_tests`: CRUD operations (upsert, insert, delete)
//! - `query_tests`: Traversal, impact analysis, stats, orphan pruning
//! - `shared_tests`: `SharedGraphStore` coordination and stub resolution
//! - `generation_tests`: per-generation writes and branch-scoped reads

mod generation_tests;
mod id_tests;
mod query_tests;
mod shared_tests;
mod store_tests;

use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};

use super::*;

const TENANT: &str = "test-tenant";

/// Create an in-memory graph database for testing, on the migrated schema.
async fn test_store() -> SqliteGraphStore {
    let opts = SqliteConnectOptions::new()
        .filename(":memory:")
        .create_if_missing(true);
    let pool = SqlitePoolOptions::new()
        .max_connections(1)
        .connect_with(opts)
        .await
        .unwrap();
    crate::graph::schema::apply_graph_schema(&pool).await;
    SqliteGraphStore::new(pool)
}
