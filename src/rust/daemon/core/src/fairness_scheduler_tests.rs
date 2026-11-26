//! Scheduling must serve other tenants during a branch-prune delete flood.

use super::*;
use crate::queue_config::QueueConnectionConfig;
use sqlx::SqlitePool;

async fn setup() -> (SqlitePool, FairnessScheduler, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let pool = QueueConnectionConfig::with_database_path(dir.path().join("fairness.db"))
        .create_pool()
        .await
        .unwrap();
    sqlx::raw_sql(include_str!("schema/watch_folders_schema.sql"))
        .execute(&pool)
        .await
        .unwrap();
    let manager = QueueManager::new(pool.clone());
    manager.init_unified_queue().await.unwrap();
    let scheduler = FairnessScheduler::new(
        manager,
        FairnessSchedulerConfig {
            high_priority_batch: 2,
            low_priority_batch: 1,
            ..Default::default()
        },
    );
    (pool, scheduler, dir)
}

async fn enqueue(pool: &SqlitePool, id: &str, tenant: &str, collection: &str, op: &str) {
    sqlx::query(
        "INSERT INTO unified_queue (queue_id, item_type, op, tenant_id, collection, \
         idempotency_key, created_at) VALUES (?1, 'text', ?2, ?3, ?4, ?1, \
         '2020-01-01T00:00:00.000Z')",
    )
    .bind(id)
    .bind(op)
    .bind(tenant)
    .bind(collection)
    .execute(pool)
    .await
    .unwrap();
}

#[tokio::test]
async fn delete_flood_does_not_starve_scratchpad_rules_or_project_scans() {
    let (pool, scheduler, _dir) = setup().await;
    for i in 0..64 {
        enqueue(
            &pool,
            &format!("delete-{i:03}"),
            "a-busy",
            "projects",
            "delete",
        )
        .await;
    }
    enqueue(&pool, "note", "b-doc", "scratchpad", "update").await;
    enqueue(&pool, "rule", "c-rules", "rules", "add").await;
    enqueue(&pool, "scan", "d-idle", "projects", "scan").await;

    let mut served = std::collections::HashSet::new();
    for _ in 0..12 {
        let items = scheduler.dequeue_next_batch(1).await.unwrap();
        assert_eq!(items.len(), 1);
        served.insert(items[0].tenant_id.clone());
    }
    assert!(
        served.contains("b-doc"),
        "scratchpad starved behind unrelated deletes"
    );
    assert!(
        served.contains("c-rules"),
        "rules starved behind unrelated deletes"
    );
    assert!(
        served.contains("d-idle"),
        "idle project starved behind unrelated deletes"
    );
    let remaining: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM unified_queue WHERE status = 'pending' AND op = 'delete'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert!(remaining > 0, "fixture must still have a delete backlog");
}

#[tokio::test]
async fn tenant_turn_preserves_delete_before_add_and_wraps_cursor() {
    let (pool, scheduler, _dir) = setup().await;
    enqueue(&pool, "add", "b-doc", "projects", "add").await;
    enqueue(&pool, "delete", "b-doc", "projects", "delete").await;
    scheduler.set_priority_direction(false).await;
    assert_eq!(
        scheduler.dequeue_next_batch(1).await.unwrap()[0].queue_id,
        "delete"
    );
    enqueue(&pool, "late-note", "a-note", "scratchpad", "update").await;
    scheduler.set_priority_direction(false).await;
    assert_eq!(
        scheduler.dequeue_next_batch(1).await.unwrap()[0].queue_id,
        "late-note"
    );
    scheduler.set_priority_direction(false).await;
    assert_eq!(
        scheduler.dequeue_next_batch(1).await.unwrap()[0].queue_id,
        "add"
    );
}

#[tokio::test]
async fn tenant_turn_skips_live_leases_and_recovers_expired_work() {
    let (pool, scheduler, _dir) = setup().await;
    enqueue(&pool, "leased", "a-leased", "scratchpad", "add").await;
    sqlx::query("UPDATE unified_queue SET lease_until = '2999-01-01T00:00:00.000Z' WHERE queue_id = 'leased'")
        .execute(&pool).await.unwrap();
    enqueue(&pool, "ready", "b-ready", "scratchpad", "add").await;
    scheduler.set_priority_direction(false).await;
    assert_eq!(
        scheduler.dequeue_next_batch(1).await.unwrap()[0].queue_id,
        "ready"
    );
    sqlx::query("UPDATE unified_queue SET lease_until = '2020-01-01T00:00:00.000Z' WHERE queue_id = 'ready'")
        .execute(&pool).await.unwrap();
    scheduler.set_priority_direction(false).await;
    assert_eq!(
        scheduler.dequeue_next_batch(1).await.unwrap()[0].queue_id,
        "ready"
    );
    assert!(scheduler.dequeue_next_batch(1).await.unwrap().is_empty());
}
