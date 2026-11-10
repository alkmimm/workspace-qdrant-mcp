//! Per-tenant indexing progress for `GetProjectStatusResponse`.

use chrono::{DateTime, Utc};
use sqlx::SqlitePool;
use tracing::warn;
use workspace_qdrant_core::indexing_progress::{
    estimate_eta_seconds, global_rate_files_per_sec, rate_files_per_sec,
};
use workspace_qdrant_core::queue_operations::{InFlightSummary, QueueManager};

/// Per-tenant indexing-progress counts attached to `GetProjectStatusResponse`.
///
/// `pending`, `in_progress`, `failed` come from `unified_queue` (in-flight),
/// while `done` is the durable count from `tracked_files`. The queue rows
/// for completed work are deleted after retention, so we cannot derive
/// `done` from the queue alone.
///
/// `eta_seconds` is `Some` only when we have at least
/// [`MIN_RATE_WINDOW_SECONDS`] of recent indexing activity AND a positive
/// rate — callers must render "warming up" / "unknown" when it's `None`.
///
/// `oldest_pending_age_seconds` is how long the tenant's oldest in-flight
/// item has waited, `None` when nothing is in flight. It is the signal a
/// stalled queue gives: counts and ETA can look plausible while nothing of
/// this project is being processed.
#[derive(Debug, Default, Clone, Copy)]
pub(crate) struct IndexingProgress {
    pub pending: i64,
    pub in_progress: i64,
    pub failed: i64,
    pub done: i64,
    pub eta_seconds: Option<i64>,
    pub oldest_pending_age_seconds: Option<i64>,
}

impl IndexingProgress {
    pub(crate) fn total(&self) -> i64 {
        self.pending + self.in_progress + self.failed + self.done
    }

    pub(crate) fn percent_complete(&self) -> f64 {
        let total = self.total();
        if total == 0 {
            100.0
        } else {
            (self.done as f64) / (total as f64) * 100.0
        }
    }
}

/// Fetch per-tenant indexing progress. Returns zeros on any DB error so the
/// caller can still return the rest of the project status; the gRPC contract
/// has the indexing block as best-effort enrichment.
pub(crate) async fn fetch_indexing_progress(
    pool: &SqlitePool,
    tenant_id: &str,
) -> IndexingProgress {
    let manager = QueueManager::new(pool.clone());
    let in_flight = match manager.get_in_flight_summary_by_tenant(tenant_id).await {
        Ok(summary) => summary,
        Err(e) => {
            warn!(
                tenant_id = %tenant_id,
                error = %e,
                "Failed to fetch in-flight queue counts for indexing progress"
            );
            InFlightSummary::default()
        }
    };

    let done = count_tracked_files(pool, tenant_id).await;

    // Prefer this tenant's own recent rate; fall back to the daemon-wide rate
    // when the tenant is idle (waiting its turn in the unified queue, which
    // processes ~one tenant at a time). Without the fallback an idle tenant's
    // window goes cold and the ETA is a perpetual "warming up".
    let rate = match rate_files_per_sec(pool, tenant_id).await {
        Some(r) => Some(r),
        None => global_rate_files_per_sec(pool).await,
    };
    let eta_seconds = estimate_eta_seconds(in_flight.pending, in_flight.in_progress, rate);

    IndexingProgress {
        pending: in_flight.pending,
        in_progress: in_flight.in_progress,
        failed: in_flight.failed,
        done,
        eta_seconds,
        oldest_pending_age_seconds: age_seconds(
            in_flight.oldest_in_flight_created_at.as_deref(),
            Utc::now(),
        ),
    }
}

/// `tracked_files` rows of this tenant (0 on error, logged).
async fn count_tracked_files(pool: &SqlitePool, tenant_id: &str) -> i64 {
    match sqlx::query_scalar::<_, i64>(
        r#"
        SELECT COUNT(*)
        FROM tracked_files tf
        JOIN watch_folders wf ON tf.watch_folder_id = wf.watch_id
        WHERE wf.tenant_id = ?1
        "#,
    )
    .bind(tenant_id)
    .fetch_one(pool)
    .await
    {
        Ok(count) => count,
        Err(e) => {
            warn!(
                tenant_id = %tenant_id,
                error = %e,
                "Failed to count tracked_files for indexing progress"
            );
            0
        }
    }
}

/// Whole seconds from `created_at` (the queue's RFC 3339 stamp) to `now`.
/// `None` for no timestamp or an unparsable one; never negative (a clock
/// step backwards reads as 0, not as an item from the future).
pub(crate) fn age_seconds(created_at: Option<&str>, now: DateTime<Utc>) -> Option<i64> {
    let created = DateTime::parse_from_rfc3339(created_at?).ok()?;
    Some((now - created.with_timezone(&Utc)).num_seconds().max(0))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn age_is_measured_from_the_queue_stamp() {
        let now = DateTime::parse_from_rfc3339("2026-11-10T03:18:07.000Z")
            .unwrap()
            .with_timezone(&Utc);
        // The tecsul stall: oldest item enqueued 4 days, 3:37:03 earlier.
        assert_eq!(
            age_seconds(Some("2026-11-05T23:41:04.000Z"), now),
            Some(4 * 86_400 + 3 * 3_600 + 37 * 60 + 3)
        );
        assert_eq!(age_seconds(None, now), None, "nothing in flight");
        assert_eq!(age_seconds(Some("not a date"), now), None);
        assert_eq!(
            age_seconds(Some("2026-11-10T03:20:00.000Z"), now),
            Some(0),
            "a stamp ahead of the clock is clamped, never negative"
        );
    }
}
