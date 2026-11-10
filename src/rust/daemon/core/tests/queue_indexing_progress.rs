//! Tests for the per-tenant indexing-progress queries powering the
//! `GetProjectStatus` indexing block, the Grafana per-tenant gauge, and
//! the Admin UI progress bars.
//!
//! Covers `get_in_flight_counts_by_tenant` (single-tenant slice) and
//! `get_unified_queue_depth_by_tenant_status` (Prometheus exporter feed).

use sqlx::SqlitePool;
use workspace_qdrant_core::{
    QueueManager, CREATE_UNIFIED_QUEUE_INDEXES_SQL, CREATE_UNIFIED_QUEUE_SQL,
};
use wqm_common::timestamps;

async fn build_pool() -> SqlitePool {
    let pool = SqlitePool::connect("sqlite::memory:")
        .await
        .expect("connect in-memory sqlite");
    sqlx::query(CREATE_UNIFIED_QUEUE_SQL)
        .execute(&pool)
        .await
        .expect("create unified_queue");
    for stmt in CREATE_UNIFIED_QUEUE_INDEXES_SQL {
        sqlx::query(stmt)
            .execute(&pool)
            .await
            .expect("create index");
    }
    pool
}

/// Insert a synthetic queue row with a given tenant and status. Skips
/// `QueueManager::enqueue*` so the test can also seed `failed`/`done` rows
/// without crafting full retry/finalization payloads.
async fn insert_row(pool: &SqlitePool, tenant: &str, status: &str, file_path: &str) {
    let now = timestamps::now_utc();
    let payload = format!(r#"{{"file_path":"{}"}}"#, file_path);
    sqlx::query(
        r#"
        INSERT INTO unified_queue (
            idempotency_key, item_type, op, tenant_id, collection, status,
            payload_json, created_at, updated_at, file_path
        ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?8, ?9)
        "#,
    )
    .bind(format!("{}|{}|{}", tenant, status, file_path))
    .bind("file")
    .bind("add")
    .bind(tenant)
    .bind("projects")
    .bind(status)
    .bind(payload)
    .bind(&now)
    .bind(file_path)
    .execute(pool)
    .await
    .expect("insert queue row");
}

#[tokio::test]
async fn in_flight_counts_filter_by_tenant_and_status() {
    let pool = build_pool().await;
    let manager = QueueManager::new(pool.clone());

    // Tenant A: 3 pending, 1 in_progress, 1 failed, 2 done.
    insert_row(&pool, "tenant_a", "pending", "a/1.rs").await;
    insert_row(&pool, "tenant_a", "pending", "a/2.rs").await;
    insert_row(&pool, "tenant_a", "pending", "a/3.rs").await;
    insert_row(&pool, "tenant_a", "in_progress", "a/4.rs").await;
    insert_row(&pool, "tenant_a", "failed", "a/5.rs").await;
    insert_row(&pool, "tenant_a", "done", "a/6.rs").await;
    insert_row(&pool, "tenant_a", "done", "a/7.rs").await;

    // Tenant B has unrelated work that must not bleed into A's counts.
    insert_row(&pool, "tenant_b", "pending", "b/1.rs").await;
    insert_row(&pool, "tenant_b", "in_progress", "b/2.rs").await;

    let (pending, in_progress, failed) = manager
        .get_in_flight_counts_by_tenant("tenant_a")
        .await
        .expect("count tenant_a");

    assert_eq!(pending, 3, "pending count for tenant_a");
    assert_eq!(in_progress, 1, "in_progress count for tenant_a");
    assert_eq!(failed, 1, "failed count for tenant_a");

    // Unknown tenants must return zeros, not error — used as the default
    // case in `fetch_indexing_progress` when the daemon hasn't seen the
    // project yet.
    let (p, i, f) = manager
        .get_in_flight_counts_by_tenant("nonexistent")
        .await
        .expect("count nonexistent");
    assert_eq!((p, i, f), (0, 0, 0));
}

#[tokio::test]
async fn depth_by_tenant_status_excludes_done_and_groups_correctly() {
    let pool = build_pool().await;
    let manager = QueueManager::new(pool.clone());

    insert_row(&pool, "tenant_x", "pending", "x/1.rs").await;
    insert_row(&pool, "tenant_x", "pending", "x/2.rs").await;
    insert_row(&pool, "tenant_x", "in_progress", "x/3.rs").await;
    insert_row(&pool, "tenant_x", "done", "x/4.rs").await; // must be excluded
    insert_row(&pool, "tenant_y", "failed", "y/1.rs").await;

    let rows = manager
        .get_unified_queue_depth_by_tenant_status()
        .await
        .expect("depth by tenant/status");

    let lookup: std::collections::HashMap<(String, String), i64> =
        rows.into_iter().map(|(t, s, c)| ((t, s), c)).collect();

    assert_eq!(lookup.get(&("tenant_x".into(), "pending".into())), Some(&2));
    assert_eq!(
        lookup.get(&("tenant_x".into(), "in_progress".into())),
        Some(&1)
    );
    assert_eq!(lookup.get(&("tenant_y".into(), "failed".into())), Some(&1));
    // Done rows must NOT appear (the Grafana gauge and the search-time
    // indexing block both treat `unified_queue` as in-flight only).
    assert!(!lookup.contains_key(&("tenant_x".into(), "done".into())));
}

/// The stall signal: the oldest PENDING or IN-PROGRESS item of the tenant.
/// Failed and done rows are history, not waiting work, and another tenant's
/// older items must not leak in.
#[tokio::test]
async fn in_flight_summary_reports_the_tenants_oldest_waiting_item() {
    let pool = build_pool().await;
    let manager = QueueManager::new(pool.clone());

    insert_row(&pool, "tenant_a", "pending", "a/new.rs").await;
    insert_row(&pool, "tenant_a", "in_progress", "a/held.rs").await;
    insert_row(&pool, "tenant_a", "failed", "a/failed.rs").await;
    insert_row(&pool, "tenant_a", "done", "a/done.rs").await;
    insert_row(&pool, "tenant_b", "pending", "b/older.rs").await;
    let stamp = |path: &str, at: &str| {
        sqlx::query("UPDATE unified_queue SET created_at = ?2 WHERE file_path = ?1")
            .bind(path.to_string())
            .bind(at.to_string())
    };
    stamp("a/new.rs", "2026-11-09T10:00:00.000Z")
        .execute(&pool)
        .await
        .unwrap();
    stamp("a/held.rs", "2026-11-08T10:00:00.000Z")
        .execute(&pool)
        .await
        .unwrap();
    stamp("a/failed.rs", "2026-11-01T10:00:00.000Z")
        .execute(&pool)
        .await
        .unwrap();
    stamp("a/done.rs", "2026-11-01T10:00:00.000Z")
        .execute(&pool)
        .await
        .unwrap();
    stamp("b/older.rs", "2026-10-01T10:00:00.000Z")
        .execute(&pool)
        .await
        .unwrap();

    let a = manager
        .get_in_flight_summary_by_tenant("tenant_a")
        .await
        .expect("summary tenant_a");
    assert_eq!((a.pending, a.in_progress, a.failed), (1, 1, 1));
    assert_eq!(
        a.oldest_in_flight_created_at.as_deref(),
        Some("2026-11-08T10:00:00.000Z"),
        "the held item is the oldest WAITING one; failed/done and tenant_b do not count"
    );

    let none = manager
        .get_in_flight_summary_by_tenant("nonexistent")
        .await
        .expect("summary nonexistent");
    assert_eq!(none.oldest_in_flight_created_at, None);
}
