//! Dequeue candidate list + lock-free selection (schema v50).
//!
//! The dequeue ranks only each group's head instead of the whole queue. These
//! tests pin the two properties that change must not break:
//! - the ranked result is IDENTICAL to ranking every row, on both fairness
//!   passes, with and without the tenant / item-type filters;
//! - selecting outside the write lock never steals a row another worker
//!   claimed in between.

use super::*;
use crate::queue_operations::dequeue_candidates::{collect_candidates, pending_groups};
use crate::queue_operations::dequeue_query::{rank_candidates, DequeueParams};

async fn setup() -> (SqlitePool, QueueManager, tempfile::TempDir) {
    let temp_dir = tempdir().unwrap();
    let config = QueueConnectionConfig::with_database_path(temp_dir.path().join("cand.db"));
    let pool = config.create_pool().await.unwrap();
    apply_sql_script(&pool, include_str!("../../schema/watch_folders_schema.sql"))
        .await
        .unwrap();
    let manager = QueueManager::new(pool.clone());
    manager.init_unified_queue().await.unwrap();
    (pool, manager, temp_dir)
}

async fn insert_watch(pool: &SqlitePool, tenant: &str, collection: &str, active: i64) {
    sqlx::query(
        "INSERT INTO watch_folders (watch_id, path, collection, tenant_id, is_active, \
         created_at, updated_at) VALUES (?1, ?2, ?3, ?1, ?4, \
         '2026-01-01T00:00:00.000Z', '2026-01-01T00:00:00.000Z')",
    )
    .bind(tenant)
    .bind(format!("/w/{}", tenant))
    .bind(collection)
    .bind(active)
    .execute(pool)
    .await
    .unwrap();
}

#[allow(clippy::too_many_arguments)]
async fn insert_row(
    pool: &SqlitePool,
    id: &str,
    tenant: &str,
    collection: &str,
    item_type: &str,
    op: &str,
    status: &str,
    created_at: &str,
    lease_until: Option<&str>,
) {
    sqlx::query(
        "INSERT INTO unified_queue (queue_id, item_type, op, tenant_id, collection, status, \
         created_at, lease_until, idempotency_key) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?1)",
    )
    .bind(id)
    .bind(item_type)
    .bind(op)
    .bind(tenant)
    .bind(collection)
    .bind(status)
    .bind(created_at)
    .bind(lease_until)
    .execute(pool)
    .await
    .unwrap();
}

const PAST: &str = "2020-01-01T00:00:00.000Z";
const FUTURE: &str = "2999-01-01T00:00:00.000Z";

/// A queue spanning every ranking term: active/idle/library/rules tenants, all
/// op weights, tenant-adds, ages on both sides of the promotion thresholds,
/// leased-in-the-future pending rows, expired and live in_progress rows, and
/// finished rows.
async fn seed(pool: &SqlitePool) {
    insert_watch(pool, "t-active", "projects", 1).await;
    insert_watch(pool, "t-idle", "projects", 0).await;
    insert_watch(pool, "lib-a", "libraries", 0).await;
    let tenants = [
        ("t-active", "projects"),
        ("t-idle", "projects"),
        ("lib-a", "libraries"),
        ("t-rules", "rules"),
    ];
    let ops = ["add", "delete", "update", "scan", "uplift", "reset"];
    let types = ["file", "folder", "tenant"];
    let statuses = ["pending", "pending", "pending", "in_progress", "done"];
    let now = chrono::Utc::now();
    let mut n: u64 = 7;
    for i in 0..400 {
        // Deterministic LCG so the fixture is stable across runs.
        n = n
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        let (tenant, collection) = tenants[(n >> 33) as usize % tenants.len()];
        let op = ops[(n >> 40) as usize % ops.len()];
        let item_type = types[(n >> 45) as usize % types.len()];
        let status = statuses[(n >> 50) as usize % statuses.len()];
        // Ages from seconds to days old: crosses the 300 s / 900 s thresholds.
        let age = chrono::Duration::seconds(((n >> 20) % 200_000) as i64);
        let created = wqm_common::timestamps::format_utc(&(now - age));
        let lease = match (status, (n >> 55) % 3) {
            ("in_progress", 0) => Some(FUTURE),
            ("in_progress", _) => Some(PAST),
            ("pending", 0) => Some(FUTURE),
            _ => None,
        };
        let id = format!("{:032x}", i);
        insert_row(
            pool, &id, tenant, collection, item_type, op, status, &created, lease,
        )
        .await;
    }
}

fn params<'a>(
    now: &'a str,
    tenant_id: Option<&'a str>,
    item_type: Option<ItemType>,
    descending: bool,
    batch_size: i32,
) -> DequeueParams<'a> {
    DequeueParams {
        now_str: now,
        tenant_id,
        item_type,
        descending,
        batch_size,
        age_warning_seconds: 300,
        age_critical_seconds: 900,
    }
}

#[tokio::test]
async fn candidate_ranking_equals_whole_queue_ranking() {
    let (pool, _manager, _dir) = setup().await;
    seed(&pool).await;
    let all: Vec<String> = sqlx::query_scalar("SELECT queue_id FROM unified_queue")
        .fetch_all(&pool)
        .await
        .unwrap();
    let now = wqm_common::timestamps::now_utc();
    let mut conn = pool.acquire().await.unwrap();
    let mut compared = 0;
    for descending in [true, false] {
        for tenant in [None, Some("t-active"), Some("t-idle"), Some("absent")] {
            for item_type in [None, Some(ItemType::File), Some(ItemType::Tenant)] {
                for batch in [1, 3, 10, 50] {
                    let p = params(&now, tenant, item_type, descending, batch);
                    let candidates = collect_candidates(&mut conn, &p).await.unwrap();
                    let got = rank_candidates(&mut conn, &p, &candidates).await.unwrap();
                    let want = rank_candidates(&mut conn, &p, &all).await.unwrap();
                    assert_eq!(
                        got, want,
                        "desc={} tenant={:?} type={:?} batch={}",
                        descending, tenant, item_type, batch
                    );
                    compared += usize::from(!want.is_empty());
                }
            }
        }
    }
    // Guard against a vacuous pass: most shapes must actually select rows.
    assert!(compared >= 40, "only {} non-empty comparisons", compared);
}

#[tokio::test]
async fn candidates_are_bounded_by_groups_not_queue_size() {
    let (pool, _manager, _dir) = setup().await;
    seed(&pool).await;
    let now = wqm_common::timestamps::now_utc();
    let mut conn = pool.acquire().await.unwrap();
    let groups = pending_groups(&mut conn, None).await.unwrap();
    let distinct: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM (SELECT DISTINCT tenant_id, collection, op, item_type \
         FROM unified_queue WHERE status = 'pending')",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(groups.len() as i64, distinct);
    let p = params(&now, None, None, true, 3);
    let candidates = collect_candidates(&mut conn, &p).await.unwrap();
    let expired: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM unified_queue WHERE status = 'in_progress' AND lease_until < ?1",
    )
    .bind(&now)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert!(candidates.len() as i64 <= distinct * 3 + expired);

    let only = pending_groups(&mut conn, Some("t-idle")).await.unwrap();
    assert!(!only.is_empty() && only.iter().all(|g| g[0] == "t-idle"));
    assert!(pending_groups(&mut conn, Some("absent"))
        .await
        .unwrap()
        .is_empty());
}

#[tokio::test]
async fn claim_skips_rows_claimed_after_selection() {
    let (pool, manager, _dir) = setup().await;
    insert_watch(&pool, "t-active", "projects", 1).await;
    for i in 0..3 {
        let id = format!("{:032x}", i);
        let created = format!("2026-01-0{}T00:00:00.000Z", i + 1);
        insert_row(
            &pool, &id, "t-active", "projects", "file", "add", "pending", &created, None,
        )
        .await;
    }
    let now = wqm_common::timestamps::now_utc();
    let p = params(&now, None, None, true, 3);
    let selected = manager.select_dequeue_ids(&p).await.unwrap();
    assert_eq!(selected.len(), 3);

    // Another worker claims the top row between selection and claim.
    sqlx::query("UPDATE unified_queue SET status = 'in_progress', worker_id = 'other', lease_until = ?2 WHERE queue_id = ?1")
        .bind(&selected[0])
        .bind(FUTURE)
        .execute(&pool)
        .await
        .unwrap();

    let items = manager
        .claim_selected(&selected, "me", &now, FUTURE)
        .await
        .unwrap();
    let claimed: Vec<&str> = items.iter().map(|i| i.queue_id.as_str()).collect();
    assert_eq!(claimed, vec![selected[1].as_str(), selected[2].as_str()]);
    let owner: String =
        sqlx::query_scalar("SELECT worker_id FROM unified_queue WHERE queue_id = ?1")
            .bind(&selected[0])
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(owner, "other", "a live lease must never be stolen");

    // Everything taken → the claim returns nothing instead of re-leasing.
    let again = manager
        .claim_selected(&selected, "third", &now, FUTURE)
        .await
        .unwrap();
    assert!(again.is_empty());
}

#[tokio::test]
async fn dequeue_unified_claims_in_rank_order() {
    let (pool, manager, _dir) = setup().await;
    seed(&pool).await;
    let items = manager
        .dequeue_unified(5, "w1", Some(300), None, None, Some(true), None, None)
        .await
        .unwrap();
    assert_eq!(items.len(), 5);
    // Delete/reset always rank first and the seed has plenty of them.
    assert!(items
        .iter()
        .all(|i| matches!(i.op, UnifiedOp::Delete | UnifiedOp::Reset)));
    assert!(items
        .iter()
        .all(|i| i.status == QueueStatus::InProgress && i.worker_id.as_deref() == Some("w1")));
}
