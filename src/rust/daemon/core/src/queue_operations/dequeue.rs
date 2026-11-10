//! Queue dequeue and stale lease recovery operations.

use chrono::Duration as ChronoDuration;
use chrono::Utc;
use sqlx::{Row, SqliteConnection};
use tracing::{debug, info, warn};
use wqm_common::timestamps;

use crate::monitoring::metrics_core::METRICS;
use crate::unified_queue_schema::{
    DestinationStatus, ItemType, QueueOperation as UnifiedOp, QueueStatus, UnifiedQueueItem,
};

use super::dequeue_candidates::collect_candidates;
use super::dequeue_query::{rank_candidates, DequeueParams};
use super::{QueueError, QueueManager, QueueResult};

/// How many select→claim rounds one dequeue tries when every selected row was
/// claimed by a concurrent worker between the (lock-free) selection and the
/// claim.
const MAX_CLAIM_ATTEMPTS: usize = 3;

impl QueueManager {
    /// Dequeue a batch of items from the unified queue with lease-based locking
    ///
    /// Acquires a lease on the items to prevent concurrent processing.
    /// Items with expired leases are also considered for dequeuing.
    ///
    /// # Arguments
    /// * `batch_size` - Maximum number of items to dequeue
    /// * `worker_id` - Identifier for this worker (for lease tracking)
    /// * `lease_duration_secs` - How long to hold the lease (default: 300 seconds)
    /// * `tenant_id` - Optional filter by tenant
    /// * `item_type` - Optional filter by item type
    /// * `priority_descending` - If true, high priority first (DESC) with FIFO tiebreaker;
    ///   if false, low priority first (ASC) with LIFO tiebreaker.
    ///   Used for anti-starvation alternation. Defaults to true if None.
    /// * `age_promotion_warning_seconds` - Items pending longer than this get +1 priority
    ///   bump in the age-promotion CASE. Defaults to 300 (5 minutes) if None.
    /// * `age_promotion_critical_seconds` - Items pending longer than this get +2 priority
    ///   bump in the age-promotion CASE. Defaults to 900 (15 minutes) if None.
    #[allow(clippy::too_many_arguments)]
    #[tracing::instrument(
        name = "queue.dequeue",
        skip_all,
        fields(
            batch_size = batch_size,
            worker_id = %worker_id,
            tenant_id = tracing::field::Empty,
            item_type = tracing::field::Empty,
        )
    )]
    pub async fn dequeue_unified(
        &self,
        batch_size: i32,
        worker_id: &str,
        lease_duration_secs: Option<i64>,
        tenant_id: Option<&str>,
        item_type: Option<ItemType>,
        priority_descending: Option<bool>,
        age_promotion_warning_seconds: Option<u64>,
        age_promotion_critical_seconds: Option<u64>,
    ) -> QueueResult<Vec<UnifiedQueueItem>> {
        if let Some(tid) = tenant_id {
            tracing::Span::current().record("tenant_id", tid);
        }
        if let Some(it) = item_type {
            tracing::Span::current().record("item_type", tracing::field::debug(it));
        }
        let lease_duration = lease_duration_secs.unwrap_or(300);
        let lease_until = Utc::now() + ChronoDuration::seconds(lease_duration);
        let lease_until_str = timestamps::format_utc(&lease_until);
        let now_str = timestamps::now_utc();

        // Age-based promotion thresholds (Unit 3: prevent tenant starvation under
        // fairness scheduler). Items pending longer than `warning_seconds` get +1
        // in the age-promotion CASE; items pending longer than `critical_seconds`
        // get +2. This is applied AFTER delete/tenant-add precedence but BEFORE
        // the project-active priority, so old low-weight scans eventually outrank
        // fresh high-weight adds from other tenants.
        let params = DequeueParams {
            now_str: &now_str,
            tenant_id,
            item_type,
            // Task 21: priority direction for anti-starvation alternation
            // (Task 9: FIFO on the normal pass, LIFO on the flip).
            descending: priority_descending.unwrap_or(true),
            batch_size,
            age_warning_seconds: age_promotion_warning_seconds.unwrap_or(300) as i64,
            age_critical_seconds: age_promotion_critical_seconds.unwrap_or(900) as i64,
        };

        for attempt in 1..=MAX_CLAIM_ATTEMPTS {
            let queue_ids = self.select_dequeue_ids(&params).await?;
            if queue_ids.is_empty() {
                return Ok(Vec::new());
            }
            let items = self
                .claim_selected(&queue_ids, worker_id, &now_str, &lease_until_str)
                .await?;
            if !items.is_empty() {
                return Ok(items);
            }
            debug!(
                attempt,
                worker_id, "Every selected queue item was claimed concurrently; reselecting"
            );
        }
        Ok(Vec::new())
    }

    /// Choose the queue ids to claim — WITHOUT holding the write lock.
    ///
    /// A deferred (read) transaction gives the candidate walk and the ranking
    /// one WAL snapshot; it never upgrades to a writer, so it neither blocks
    /// the item-processing writers nor hits `SQLITE_BUSY_SNAPSHOT`. The rows it
    /// returns may be claimed by another worker before `claim_selected` runs;
    /// the claim re-checks claimability and skips those.
    pub(super) async fn select_dequeue_ids(
        &self,
        params: &DequeueParams<'_>,
    ) -> QueueResult<Vec<String>> {
        let mut read = self.pool.begin().await.map_err(QueueError::Database)?;
        let candidates = collect_candidates(&mut read, params).await?;
        let queue_ids = rank_candidates(&mut read, params, &candidates).await?;
        let _ = read.rollback().await;
        Ok(queue_ids)
    }

    /// Lease `queue_ids` to `worker_id` and return the claimed items in
    /// `queue_ids` order. Rows no longer claimable are skipped.
    ///
    /// Uses BEGIN IMMEDIATE (not sqlx's default deferred BEGIN): this
    /// transaction READS (`select_in_progress_subset`) before it WRITES. A
    /// deferred BEGIN takes a read snapshot on the first SELECT and must upgrade
    /// to the write lock at the UPDATE; if another writer committed in between,
    /// SQLite fails the upgrade with SQLITE_BUSY (5) / SQLITE_BUSY_SNAPSHOT
    /// (517) *immediately*, WITHOUT invoking the busy handler — so
    /// `busy_timeout` does not cover it. Taking the write lock up front
    /// (covered by `busy_timeout` + jittered retry) turns that into a brief
    /// wait-and-proceed. The lock is held only for these few primary-key
    /// statements — the ranking already happened in `select_dequeue_ids`. See
    /// `db_retry` and PRs #329/#330.
    pub(super) async fn claim_selected(
        &self,
        queue_ids: &[String],
        worker_id: &str,
        now_str: &str,
        lease_until_str: &str,
    ) -> QueueResult<Vec<UnifiedQueueItem>> {
        let mut tx = crate::db_retry::begin_immediate(&self.pool)
            .await
            .map_err(QueueError::Database)?;

        // Capture which selected rows are ALREADY `in_progress` with an expired
        // lease — those are RE-LEASES (an op that exceeded `lease_duration` without
        // completing), not fresh dequeues. `claim_items` below overwrites their
        // lease, so snapshot it now: re-leases are otherwise SILENT (no recovery
        // log) and are the only fingerprint of a lease-loop.
        let re_leased = select_in_progress_subset(&mut tx, queue_ids).await?;
        let claimed = claim_items(&mut tx, queue_ids, worker_id, now_str, lease_until_str).await?;
        if claimed.is_empty() {
            let _ = tx.commit().await;
            return Ok(Vec::new());
        }
        let mut items = fetch_items(&mut tx, &claimed).await?;
        tx.commit().await.map_err(QueueError::Database)?;

        // Preserve the ranking order.
        let id_positions: std::collections::HashMap<&str, usize> = queue_ids
            .iter()
            .enumerate()
            .map(|(i, id)| (id.as_str(), i))
            .collect();
        items.sort_by_key(|item| {
            *id_positions
                .get(item.queue_id.as_str())
                .unwrap_or(&usize::MAX)
        });

        debug!(
            "Dequeued {} unified items for worker {}",
            items.len(),
            worker_id
        );
        record_dequeued(&items, &re_leased, worker_id);
        Ok(items)
    }

    /// Recover stale leases from crashed workers
    ///
    /// Finds items with status 'in_progress' and expired leases,
    /// resets them to 'pending' for reprocessing.
    ///
    /// Should be called at daemon startup and periodically.
    ///
    /// Returns the number of recovered items.
    pub async fn recover_stale_unified_leases(&self) -> QueueResult<u64> {
        let now_str = timestamps::now_utc();

        // Reset the row status AND any orphaned per-destination sink. A lease
        // expiry proves the worker that held it is dead, so any `qdrant_status`
        // / `search_status` it left in `in_progress` is orphaned: the in-memory
        // FTS5 batch / embedding work that would have flipped it to `done` died
        // with that worker. Without resetting the sink, the row is re-leased,
        // hits the unchanged-hash skip, and `finalize_after_success` *preserves*
        // the orphaned `in_progress` (it only auto-resolves `pending`/NULL) —
        // so the item can never finalize and blocks queue quiescence (e.g. the
        // reembed drain-to-quiescence gate). Resetting the sink to `pending`
        // lets the next pass auto-resolve it to `done`. (Recurring poison fix.)
        let query = r#"
            UPDATE unified_queue
            SET status = 'pending',
                lease_until = NULL,
                worker_id = NULL,
                qdrant_status = CASE WHEN qdrant_status = 'in_progress'
                                     THEN 'pending' ELSE qdrant_status END,
                search_status = CASE WHEN search_status = 'in_progress'
                                     THEN 'pending' ELSE search_status END,
                updated_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now')
            WHERE status = 'in_progress' AND lease_until < ?1
        "#;

        let result = sqlx::query(query)
            .bind(&now_str)
            .execute(&self.pool)
            .await?;

        let recovered = result.rows_affected();

        if recovered > 0 {
            info!("Recovered {} stale unified queue leases", recovered);
        } else {
            debug!("No stale unified queue leases to recover");
        }

        Ok(recovered)
    }

    /// Reset orphaned `in_progress` destination sinks at daemon startup.
    ///
    /// Called once during boot, **before** the queue processing loop starts and
    /// before the FTS5 batch writer / embedding workers have any work in flight.
    /// At that instant nothing in the process can legitimately hold a sink in
    /// `in_progress`, so every `qdrant_status` / `search_status = 'in_progress'`
    /// is provably orphaned from the *previous* daemon generation: the work may
    /// well have completed (e.g. the FTS5 rows were committed) but the daemon
    /// died after the commit and before `update_destination_status(... Done)` +
    /// `check_and_finalize`, leaving the sink stuck.
    ///
    /// Unlike [`recover_stale_unified_leases`](Self::recover_stale_unified_leases),
    /// this does NOT gate on lease expiry — a fast restart can leave a not-yet-
    /// expired lease whose owning worker is nonetheless gone. Resetting the sink
    /// to `pending` lets `finalize_after_success` auto-resolve it to `done` on
    /// the next pass (the on-disk work is already there; the unchanged-hash skip
    /// is therefore correct), unblocking queue quiescence.
    ///
    /// Returns the number of rows whose status or a sink was reset.
    pub async fn reset_orphaned_destinations_at_startup(&self) -> QueueResult<u64> {
        let query = r#"
            UPDATE unified_queue
            SET status = CASE WHEN status = 'in_progress'
                              THEN 'pending' ELSE status END,
                lease_until = CASE WHEN status = 'in_progress'
                                   THEN NULL ELSE lease_until END,
                worker_id = CASE WHEN status = 'in_progress'
                                 THEN NULL ELSE worker_id END,
                qdrant_status = CASE WHEN qdrant_status = 'in_progress'
                                     THEN 'pending' ELSE qdrant_status END,
                search_status = CASE WHEN search_status = 'in_progress'
                                     THEN 'pending' ELSE search_status END,
                updated_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now')
            WHERE status = 'in_progress'
               OR qdrant_status = 'in_progress'
               OR search_status = 'in_progress'
        "#;

        let result = sqlx::query(query).execute(&self.pool).await?;
        let reset = result.rows_affected();

        if reset > 0 {
            info!(
                "Startup: reset {} queue item(s) with orphaned in_progress status/sinks to pending",
                reset
            );
        } else {
            debug!("Startup: no orphaned in_progress queue items to reset");
        }

        Ok(reset)
    }
}

/// Meter every dequeued item and surface re-leases.
fn record_dequeued(
    items: &[UnifiedQueueItem],
    re_leased: &std::collections::HashMap<String, Option<String>>,
    worker_id: &str,
) {
    for item in items {
        let item_type_s = item.item_type.to_string();
        METRICS.unified_queue_dequeued(&item_type_s);
        // A row that was still `in_progress` when re-selected had its lease
        // expire before the worker finished. Surface it: a repeated re-lease of
        // the same queue_id (visible by grepping this queue_id or via the
        // `memexd_unified_queue_releases_total` counter on the dashboard) is the
        // signature of a lease-loop — an op that never completes within
        // `lease_duration` and silently restarts every expiry.
        if let Some(expired_lease) = re_leased.get(&item.queue_id) {
            let op_s = item.op.to_string();
            METRICS.unified_queue_released(&item_type_s, &op_s);
            warn!(
                queue_id = %item.queue_id,
                item_type = %item_type_s,
                op = %op_s,
                tenant_id = %item.tenant_id,
                retry_count = item.retry_count,
                expired_lease_until = expired_lease.as_deref().unwrap_or("<none>"),
                worker_id = %worker_id,
                "Re-leasing in_progress unified queue item whose lease expired \
                 before completion (op exceeded lease_duration); repeated \
                 re-leases of the same queue_id indicate a lease-loop"
            );
        }
    }
}

/// Lease the still-claimable rows of `queue_ids` and return their ids.
///
/// The claimability guard is the dequeue WHERE clause again: selection ran
/// outside the write lock, so a row may have been claimed (fresh lease) or
/// finished since. Those are left alone — never stolen from a live worker.
async fn claim_items(
    conn: &mut SqliteConnection,
    queue_ids: &[String],
    worker_id: &str,
    now_str: &str,
    lease_until_str: &str,
) -> QueueResult<Vec<String>> {
    let placeholders: Vec<String> = (1..=queue_ids.len())
        .map(|i| format!("?{}", i + 3))
        .collect();
    let update_query = format!(
        r#"
        UPDATE unified_queue
        SET status = 'in_progress',
            worker_id = ?1,
            lease_until = ?2,
            updated_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now')
        WHERE queue_id IN ({})
          AND (
            (status = 'pending' AND (lease_until IS NULL OR lease_until < ?3))
            OR (status = 'in_progress' AND lease_until < ?3)
          )
        RETURNING queue_id
        "#,
        placeholders.join(", ")
    );

    let mut update_builder = sqlx::query_scalar::<_, String>(&update_query)
        .bind(worker_id)
        .bind(lease_until_str)
        .bind(now_str);

    for queue_id in queue_ids {
        update_builder = update_builder.bind(queue_id);
    }

    Ok(update_builder.fetch_all(&mut *conn).await?)
}

/// Snapshot which of `queue_ids` are currently `in_progress`, with their
/// (soon-to-be-overwritten) `lease_until`.
///
/// A row selected for dequeue while still `in_progress` means its lease expired
/// before the worker finished — the dequeue is RE-LEASING it rather than picking
/// up fresh work. Capturing this set BEFORE `lease_items` overwrites the lease
/// lets the caller log/meter re-leases. Without it, re-leases are silent (the
/// "Recovered N stale unified queue leases" message belongs to the separate
/// `recover_stale_unified_leases` path, not this hot dequeue path), so a
/// never-completing op that re-leases every `lease_duration` loops invisibly.
async fn select_in_progress_subset(
    conn: &mut SqliteConnection,
    queue_ids: &[String],
) -> QueueResult<std::collections::HashMap<String, Option<String>>> {
    let placeholders: Vec<String> = (1..=queue_ids.len()).map(|i| format!("?{}", i)).collect();
    let query = format!(
        "SELECT queue_id, lease_until FROM unified_queue \
         WHERE status = 'in_progress' AND queue_id IN ({})",
        placeholders.join(", ")
    );

    let mut builder = sqlx::query(&query);
    for queue_id in queue_ids {
        builder = builder.bind(queue_id);
    }

    let rows = builder.fetch_all(&mut *conn).await?;
    let mut map = std::collections::HashMap::with_capacity(rows.len());
    for row in rows {
        let qid: String = row.try_get("queue_id")?;
        let lease: Option<String> = row.try_get("lease_until")?;
        map.insert(qid, lease);
    }
    Ok(map)
}

/// Fetch queue items by their IDs.
async fn fetch_items(
    conn: &mut SqliteConnection,
    queue_ids: &[String],
) -> QueueResult<Vec<UnifiedQueueItem>> {
    let fetch_placeholders: Vec<String> =
        (1..=queue_ids.len()).map(|i| format!("?{}", i)).collect();
    let fetch_query = format!(
        "SELECT * FROM unified_queue WHERE queue_id IN ({})",
        fetch_placeholders.join(", ")
    );

    let mut fetch_builder = sqlx::query(&fetch_query);
    for queue_id in queue_ids {
        fetch_builder = fetch_builder.bind(queue_id);
    }

    let rows = fetch_builder.fetch_all(&mut *conn).await?;

    let mut items = Vec::with_capacity(rows.len());
    for row in rows {
        let item_type_str: String = row.try_get("item_type")?;
        let op_str: String = row.try_get("op")?;
        let status_str: String = row.try_get("status")?;

        items.push(UnifiedQueueItem {
            queue_id: row.try_get("queue_id")?,
            idempotency_key: row.try_get("idempotency_key")?,
            item_type: ItemType::parse_str(&item_type_str)
                .ok_or_else(|| QueueError::InvalidOperation(item_type_str.clone()))?,
            op: UnifiedOp::parse_str(&op_str)
                .ok_or_else(|| QueueError::InvalidOperation(op_str.clone()))?,
            tenant_id: row.try_get("tenant_id")?,
            collection: row.try_get("collection")?,
            status: QueueStatus::parse_str(&status_str)
                .ok_or_else(|| QueueError::InvalidOperation(status_str.clone()))?,
            branch: row.try_get("branch")?,
            payload_json: row.try_get("payload_json")?,
            metadata: row.try_get("metadata")?,
            created_at: row.try_get("created_at")?,
            updated_at: row.try_get("updated_at")?,
            lease_until: row.try_get("lease_until")?,
            worker_id: row.try_get("worker_id")?,
            retry_count: row.try_get("retry_count")?,
            error_message: row.try_get("error_message")?,
            last_error_at: row.try_get("last_error_at")?,
            file_path: row.try_get("file_path")?, // Task 22
            qdrant_status: {
                let s: Option<String> = row.try_get("qdrant_status")?;
                s.and_then(|v| DestinationStatus::parse_str(&v))
            },
            search_status: {
                let s: Option<String> = row.try_get("search_status")?;
                s.and_then(|v| DestinationStatus::parse_str(&v))
            },
            decision_json: row.try_get("decision_json")?,
        });
    }

    Ok(items)
}
