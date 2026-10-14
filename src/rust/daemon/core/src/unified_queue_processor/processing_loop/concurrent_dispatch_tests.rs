//! Unit tests for the generic dispatch driver.
//!
//! These tests exercise `run_dispatch_loop` with stub `spawn_item`,
//! `is_memory_pressure`, and `apply_delay` closures. They do NOT touch
//! Qdrant, embeddings, or the full `process_item` pipeline — that
//! coverage lives in the integration tests over `process_batch`.

use super::*;
use crate::queue_config::QueueConnectionConfig;
use crate::queue_operations::QueueManager;
use crate::unified_queue_schema::{ItemType, QueueOperation, QueueStatus, UnifiedQueueItem};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tempfile::tempdir;

fn make_item(queue_id: &str) -> UnifiedQueueItem {
    UnifiedQueueItem {
        queue_id: queue_id.to_string(),
        idempotency_key: format!("idem-{queue_id}"),
        item_type: ItemType::File,
        op: QueueOperation::Add,
        tenant_id: "test-tenant".to_string(),
        collection: "projects".to_string(),
        status: QueueStatus::InProgress,
        branch: "main".to_string(),
        payload_json: "{}".to_string(),
        metadata: None,
        created_at: "2026-05-01T00:00:00Z".to_string(),
        updated_at: "2026-05-01T00:00:00Z".to_string(),
        lease_until: None,
        worker_id: None,
        retry_count: 0,
        error_message: None,
        last_error_at: None,
        file_path: None,
        qdrant_status: None,
        search_status: None,
        decision_json: None,
    }
}

async fn setup_queue_manager() -> (QueueManager, tempfile::TempDir) {
    let temp = tempdir().unwrap();
    let db_path = temp.path().join("dispatch_test.db");
    let config = QueueConnectionConfig::with_database_path(&db_path);
    let pool = config.create_pool().await.unwrap();
    // watch_folders is referenced by dequeue_unified's JOIN; the queue
    // operations tests apply this schema before init_unified_queue, so we
    // mirror the order here. Without this, enqueue/dequeue panics.
    apply_watch_folders_schema(&pool).await;
    let manager = QueueManager::new(pool);
    manager.init_unified_queue().await.unwrap();
    (manager, temp)
}

/// Apply the watch_folders DDL by parsing the schema file statement-by-
/// statement (sqlx's `execute` doesn't run multi-statement scripts).
async fn apply_watch_folders_schema(pool: &sqlx::SqlitePool) {
    let script = include_str!("../../schema/watch_folders_schema.sql");
    let mut conn = pool.acquire().await.unwrap();
    let mut statement = String::new();
    let mut in_trigger = false;
    for line in script.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with("--") {
            continue;
        }
        if trimmed.to_uppercase().starts_with("CREATE TRIGGER") {
            in_trigger = true;
        }
        statement.push_str(line);
        statement.push('\n');
        if in_trigger {
            if trimmed.eq_ignore_ascii_case("END;") || trimmed.eq_ignore_ascii_case("END") {
                in_trigger = false;
                let stmt = statement.trim();
                if !stmt.is_empty() {
                    sqlx::query(stmt).execute(&mut *conn).await.unwrap();
                }
                statement.clear();
            }
            continue;
        }
        if trimmed.ends_with(';') {
            let stmt = statement.trim();
            if !stmt.is_empty() {
                sqlx::query(stmt).execute(&mut *conn).await.unwrap();
            }
            statement.clear();
        }
    }
    let remainder = statement.trim();
    if !remainder.is_empty() {
        sqlx::query(remainder).execute(&mut *conn).await.unwrap();
    }
}

/// Test 1: With `max_concurrent_items=4`, the dispatch loop never holds
/// more than 4 spawned futures in flight at once. The closure MUST move
/// the permit into the spawned future so its lifetime extends past the
/// closure return — otherwise the semaphore frees immediately and the
/// cap collapses.
#[tokio::test]
async fn test_concurrent_dispatch_respects_semaphore() {
    let (manager, _tmp) = setup_queue_manager().await;
    let semaphore = Arc::new(tokio::sync::Semaphore::new(4));
    let cancel = CancellationToken::new();

    let in_flight = Arc::new(AtomicUsize::new(0));
    let max_observed = Arc::new(AtomicUsize::new(0));

    let items: Vec<UnifiedQueueItem> = (0..20).map(|i| make_item(&format!("q{i}"))).collect();

    let in_flight_clone = Arc::clone(&in_flight);
    let max_clone = Arc::clone(&max_observed);

    let cancelled = run_dispatch_loop(
        items,
        Arc::clone(&semaphore),
        &manager,
        &cancel,
        move |_item, permit| {
            let in_flight = Arc::clone(&in_flight_clone);
            let max_obs = Arc::clone(&max_clone);
            tokio::spawn(async move {
                // Hold permit for the full spawned future lifetime —
                // mirrors how `process_one_item_owned` keeps the
                // dispatch slot reserved until completion.
                let _p = permit;
                let current = in_flight.fetch_add(1, Ordering::SeqCst) + 1;
                max_obs.fetch_max(current, Ordering::SeqCst);
                // Yield enough times that other spawned futures get a
                // chance to race in before we exit.
                for _ in 0..4 {
                    tokio::task::yield_now().await;
                }
                tokio::time::sleep(Duration::from_millis(2)).await;
                in_flight.fetch_sub(1, Ordering::SeqCst);
            })
        },
        || async { false },
        || async {},
        |_| async { Vec::new() },
        70,
    )
    .await;

    assert!(!cancelled);
    assert_eq!(in_flight.load(Ordering::SeqCst), 0, "all spawns settled");
    assert!(
        max_observed.load(Ordering::SeqCst) <= 4,
        "max in-flight = {} should never exceed semaphore=4",
        max_observed.load(Ordering::SeqCst)
    );
    assert!(
        max_observed.load(Ordering::SeqCst) >= 2,
        "with 20 items + 4 permits we expect at least 2 concurrent at some point, observed {}",
        max_observed.load(Ordering::SeqCst)
    );
}

/// Test 2: Cancellation drains in-flight items (no panics, no half-state)
/// and re-leases pending items back to `Pending`.
#[tokio::test]
async fn test_cancellation_drains_in_flight() {
    let (manager, _tmp) = setup_queue_manager().await;

    // Enqueue 8 items so we have real rows the dispatcher can re-lease.
    let mut queue_ids = Vec::new();
    for i in 0..8 {
        let (qid, _) = manager
            .enqueue_unified(
                ItemType::File,
                QueueOperation::Add,
                "test-tenant",
                "projects",
                &format!(r#"{{"file_path":"/test/{i}.rs"}}"#),
                Some("main"),
                None,
            )
            .await
            .unwrap();
        queue_ids.push(qid);
    }
    // Dequeue them so they're in_progress (matching the pre-dispatch state).
    let items = manager
        .dequeue_unified(8, "test-worker", Some(300), None, None, None, None, None)
        .await
        .unwrap();
    assert_eq!(items.len(), 8);

    let semaphore = Arc::new(tokio::sync::Semaphore::new(4));
    let cancel = CancellationToken::new();
    let cancel_for_trigger = cancel.clone();

    let started_count = Arc::new(AtomicUsize::new(0));
    let completed_count = Arc::new(AtomicUsize::new(0));
    let started_clone = Arc::clone(&started_count);
    let completed_clone = Arc::clone(&completed_count);

    // Fire cancellation after a short delay.
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(10)).await;
        cancel_for_trigger.cancel();
    });

    let cancelled = run_dispatch_loop(
        items,
        Arc::clone(&semaphore),
        &manager,
        &cancel,
        move |_item, _permit| {
            let started = Arc::clone(&started_clone);
            let completed = Arc::clone(&completed_clone);
            tokio::spawn(async move {
                started.fetch_add(1, Ordering::SeqCst);
                // Slow enough that cancellation lands while some are still
                // running.
                tokio::time::sleep(Duration::from_millis(50)).await;
                completed.fetch_add(1, Ordering::SeqCst);
            })
        },
        || async { false },
        || async {},
        |_| async { Vec::new() },
        70,
    )
    .await;

    assert!(cancelled, "loop must report cancellation");
    // Every started item must have completed (no half-applied state).
    let started_total = started_count.load(Ordering::SeqCst);
    let completed_total = completed_count.load(Ordering::SeqCst);
    assert_eq!(
        started_total, completed_total,
        "in-flight items must drain to completion ({started_total} started, {completed_total} completed)"
    );

    // The 8 items split into: some started (which completed) and some
    // re-leased. Query the queue to verify the re-leased ones are Pending.
    let pending_rows = sqlx::query_scalar::<_, i64>(
        "SELECT COUNT(*) FROM unified_queue WHERE status = 'pending' AND queue_id IN (\
         ?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
    )
    .bind(&queue_ids[0])
    .bind(&queue_ids[1])
    .bind(&queue_ids[2])
    .bind(&queue_ids[3])
    .bind(&queue_ids[4])
    .bind(&queue_ids[5])
    .bind(&queue_ids[6])
    .bind(&queue_ids[7])
    .fetch_one(manager.pool())
    .await
    .unwrap();
    // We expect (8 - started_total) items to be re-leased to pending.
    let expected_pending = (8u64).saturating_sub(started_total as u64) as i64;
    assert_eq!(
        pending_rows, expected_pending,
        "expected {expected_pending} re-leased items in pending, found {pending_rows} \
         (started={started_total})"
    );
}

/// Test 3: Memory pressure halts new dispatches and re-leases pending.
///
/// The pressure predicate is wired to return `true` on its first call and
/// `false` afterwards. Combined with a no-op delay, this exercises the
/// re-lease path without burning the legacy 10s in-batch back-off in the
/// test (the back-off is gated on `is_memory_pressure() == true`, so a
/// single-shot pressure observation triggers it once and we just sleep
/// 10s of test time before the loop continues). To keep the test fast,
/// we instead set up pressure so the FIRST iteration drains pending and
/// then the loop ends because in_flight is empty.
#[tokio::test]
async fn test_memory_pressure_gate() {
    let (manager, _tmp) = setup_queue_manager().await;

    // Enqueue 6 items.
    let mut queue_ids = Vec::new();
    for i in 0..6 {
        let (qid, _) = manager
            .enqueue_unified(
                ItemType::File,
                QueueOperation::Add,
                "test-tenant",
                "projects",
                &format!(r#"{{"file_path":"/test/p{i}.rs"}}"#),
                Some("main"),
                None,
            )
            .await
            .unwrap();
        queue_ids.push(qid);
    }
    let items = manager
        .dequeue_unified(6, "test-worker", Some(300), None, None, None, None, None)
        .await
        .unwrap();
    assert_eq!(items.len(), 6);

    let semaphore = Arc::new(tokio::sync::Semaphore::new(2));
    let cancel = CancellationToken::new();

    // Pre-set pressure to true. The very first loop iteration will see
    // pressure with all 6 items still pending, drain them, sleep 10s,
    // then exit because nothing is in flight. The wall-clock cost is
    // bounded by the timeout below; we keep it tight so a regression
    // (pressure not honored) is visible in <2s.
    let pressure_flag = Arc::new(AtomicBool::new(true));
    let dispatched_count = Arc::new(AtomicUsize::new(0));
    let hit_count = Arc::new(AtomicUsize::new(0));

    let pressure_for_check = Arc::clone(&pressure_flag);
    let hit_count_clone = Arc::clone(&hit_count);
    let dispatched_clone = Arc::clone(&dispatched_count);

    // After the first pressure observation we flip it off, but the
    // dispatch loop hits the 10s sleep before re-checking. We override
    // the in-batch sleep behavior by NOT applying the back-off here: the
    // dispatch implementation sleeps unconditionally after re-leasing.
    // To keep the test fast, replace the test pool's tokio time with
    // tokio::time::pause / advance — but that interacts with the real
    // time used by tokio::spawn. Simpler: cap with a 12s timeout and
    // tolerate the sleep.
    let dispatch_fut = run_dispatch_loop(
        items,
        Arc::clone(&semaphore),
        &manager,
        &cancel,
        move |_item, permit| {
            let dispatched = Arc::clone(&dispatched_clone);
            tokio::spawn(async move {
                let _p = permit;
                dispatched.fetch_add(1, Ordering::SeqCst);
                tokio::time::sleep(Duration::from_millis(1)).await;
            })
        },
        move || {
            let flag = Arc::clone(&pressure_for_check);
            let hits = Arc::clone(&hit_count_clone);
            async move {
                let v = flag.load(Ordering::SeqCst);
                if v {
                    hits.fetch_add(1, Ordering::SeqCst);
                    // Flip off so the next loop iteration won't pause again.
                    flag.store(false, Ordering::SeqCst);
                }
                v
            }
        },
        || async {},
        |_| async { Vec::new() },
        70,
    );

    // Tight ceiling: the legacy 10s back-off runs once on the pressure
    // hit, plus dispatch and re-lease overhead. 12s is a safety margin.
    let result = tokio::time::timeout(Duration::from_secs(12), dispatch_fut).await;
    assert!(result.is_ok(), "dispatch loop hung past 12s");
    let cancelled = result.unwrap();
    assert!(!cancelled, "memory pressure must not be reported as cancel");

    assert!(
        hit_count.load(Ordering::SeqCst) >= 1,
        "pressure predicate should have fired at least once"
    );
    // With pressure on at start no items dispatched; all 6 re-leased.
    let dispatched_total = dispatched_count.load(Ordering::SeqCst);
    let pending_after =
        sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM unified_queue WHERE status = 'pending'")
            .fetch_one(manager.pool())
            .await
            .unwrap();
    assert!(
        pending_after >= 1,
        "memory pressure should re-lease at least one item to pending, found {pending_after}"
    );
    assert_eq!(
        dispatched_total as i64 + pending_after,
        6,
        "all 6 items accounted for (dispatched={dispatched_total}, pending={pending_after})"
    );
}

/// Test 4: An empty input batch is a no-op (no spawn, no panic).
#[tokio::test]
async fn test_empty_batch_is_noop() {
    let (manager, _tmp) = setup_queue_manager().await;
    let semaphore = Arc::new(tokio::sync::Semaphore::new(4));
    let cancel = CancellationToken::new();
    let spawn_count = Arc::new(AtomicUsize::new(0));
    let spawn_clone = Arc::clone(&spawn_count);

    let cancelled = run_dispatch_loop(
        Vec::new(),
        Arc::clone(&semaphore),
        &manager,
        &cancel,
        move |_item, _permit| {
            let c = Arc::clone(&spawn_clone);
            tokio::spawn(async move {
                c.fetch_add(1, Ordering::SeqCst);
            })
        },
        || async { false },
        || async {},
        |_| async { Vec::new() },
        70,
    )
    .await;

    assert!(!cancelled);
    assert_eq!(spawn_count.load(Ordering::SeqCst), 0);
}

/// A refill source for the tests: hands out up to `free` queued items per
/// call and counts the calls.
fn refill_source(
    items: Vec<UnifiedQueueItem>,
    calls: Arc<AtomicUsize>,
) -> impl FnMut(usize) -> std::future::Ready<Vec<UnifiedQueueItem>> {
    let queue = Arc::new(std::sync::Mutex::new(
        items.into_iter().collect::<VecDeque<_>>(),
    ));
    move |free| {
        calls.fetch_add(1, Ordering::SeqCst);
        let mut queue = queue.lock().unwrap();
        let batch = (0..free).filter_map(|_| queue.pop_front()).collect();
        std::future::ready(batch)
    }
}

/// tecsul 2026-10-07: one 5-minute PDF held every other slot idle — and
/// the next dequeue of every tenant — until the whole batch finished. With
/// a refill, the free slots keep taking work while the long item runs: every
/// refilled item finishes before it.
#[tokio::test]
async fn free_slots_are_refilled_while_a_long_item_runs() {
    let (manager, _tmp) = setup_queue_manager().await;
    let semaphore = Arc::new(tokio::sync::Semaphore::new(4));
    let cancel = CancellationToken::new();
    let finished = Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
    let calls = Arc::new(AtomicUsize::new(0));
    let batch = Vec::from(["long", "s0", "s1", "s2"].map(make_item));
    let refills = (0..8).map(|i| make_item(&format!("r{i}"))).collect();

    let done = Arc::clone(&finished);
    let cancelled = run_dispatch_loop(
        batch,
        Arc::clone(&semaphore),
        &manager,
        &cancel,
        move |item, permit| {
            let done = Arc::clone(&done);
            tokio::spawn(async move {
                let _p = permit;
                let ms = if item.queue_id == "long" { 1000 } else { 5 };
                tokio::time::sleep(Duration::from_millis(ms)).await;
                done.lock().unwrap().push(item.queue_id);
            })
        },
        || async { false },
        || async {},
        refill_source(refills, Arc::clone(&calls)),
        70,
    )
    .await;

    assert!(!cancelled);
    let finished = finished.lock().unwrap().clone();
    assert_eq!(finished.len(), 12, "{finished:?}");
    assert_eq!(
        finished.last().map(String::as_str),
        Some("long"),
        "{finished:?}"
    );
    assert!(calls.load(Ordering::SeqCst) >= 1);
}

/// Never pull more work under memory pressure: the pressure check guards the
/// refill exactly as it guards the batch's own items.
#[tokio::test]
async fn no_refill_under_memory_pressure() {
    let (manager, _tmp) = setup_queue_manager().await;
    let semaphore = Arc::new(tokio::sync::Semaphore::new(2));
    let cancel = CancellationToken::new();
    let calls = Arc::new(AtomicUsize::new(0));
    let checks = Arc::new(AtomicUsize::new(0));

    let pressure_checks = Arc::clone(&checks);
    let cancelled = run_dispatch_loop(
        vec![make_item("only")],
        Arc::clone(&semaphore),
        &manager,
        &cancel,
        |_item, permit| {
            tokio::spawn(async move {
                let _p = permit;
                tokio::time::sleep(Duration::from_millis(20)).await;
            })
        },
        // The batch's own item dispatches (first check), then pressure rises.
        move || {
            let n = pressure_checks.fetch_add(1, Ordering::SeqCst);
            async move { n >= 1 }
        },
        || async {},
        refill_source(vec![make_item("extra")], Arc::clone(&calls)),
        70,
    )
    .await;

    assert!(!cancelled);
    assert_eq!(calls.load(Ordering::SeqCst), 0, "refill under pressure");
}

/// A refill with nothing to give ends the wait as before: the batch drains
/// and the loop returns.
#[tokio::test]
async fn a_dry_refill_lets_the_batch_drain() {
    let (manager, _tmp) = setup_queue_manager().await;
    let semaphore = Arc::new(tokio::sync::Semaphore::new(4));
    let cancel = CancellationToken::new();
    let calls = Arc::new(AtomicUsize::new(0));
    let ran = Arc::new(AtomicUsize::new(0));

    let counter = Arc::clone(&ran);
    let outcome = tokio::time::timeout(
        Duration::from_secs(5),
        run_dispatch_loop(
            vec![make_item("a"), make_item("b")],
            Arc::clone(&semaphore),
            &manager,
            &cancel,
            move |_item, permit| {
                let counter = Arc::clone(&counter);
                tokio::spawn(async move {
                    let _p = permit;
                    counter.fetch_add(1, Ordering::SeqCst);
                })
            },
            || async { false },
            || async {},
            refill_source(Vec::new(), Arc::clone(&calls)),
            70,
        ),
    )
    .await;

    assert_eq!(
        outcome,
        Ok(false),
        "the loop must return, not wait for refills"
    );
    assert_eq!(ran.load(Ordering::SeqCst), 2);
}
