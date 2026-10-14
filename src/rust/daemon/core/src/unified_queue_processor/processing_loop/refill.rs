//! Refill source for one batch: while a batch's longer items run, its free
//! dispatch slots are topped up from the fairness scheduler (consumed by
//! `concurrent_dispatch::run_dispatch_loop`).

use std::sync::Arc;
use std::time::Duration;

use tracing::{debug, warn};

use crate::fairness_scheduler::FairnessScheduler;
use crate::storage::StorageClient;
use crate::unified_queue_schema::UnifiedQueueItem;

/// How long one batch keeps refilling free slots before it drains and hands
/// control back to the outer loop, whose control plane (circuit breaker,
/// adaptive scaling, queue metrics, tenant activity) runs between batches.
const REFILL_WINDOW: Duration = Duration::from_secs(300);

/// The refill for one batch (see `run_dispatch_loop`): items from the
/// fairness scheduler for the free slots, while the refill window is open and
/// Qdrant is available. A dequeue error ends the refilling; the batch drains.
pub(super) fn refill_from(
    fairness_scheduler: &Arc<FairnessScheduler>,
    storage_client: &Arc<StorageClient>,
) -> impl FnMut(usize) -> futures::future::BoxFuture<'static, Vec<UnifiedQueueItem>> {
    let deadline = std::time::Instant::now() + REFILL_WINDOW;
    let scheduler = Arc::clone(fairness_scheduler);
    let storage = Arc::clone(storage_client);
    move |free_slots| {
        let scheduler = Arc::clone(&scheduler);
        let storage = Arc::clone(&storage);
        Box::pin(async move {
            if std::time::Instant::now() >= deadline || !storage.is_qdrant_available() {
                return Vec::new();
            }
            let wanted = i32::try_from(free_slots).unwrap_or(i32::MAX);
            match scheduler.dequeue_next_batch(wanted).await {
                Ok(items) => {
                    if !items.is_empty() {
                        debug!(
                            "Refilled {} free slot(s) while longer items run",
                            items.len()
                        );
                    }
                    items
                }
                Err(e) => {
                    warn!("Refill dequeue failed, the batch drains instead: {e}");
                    Vec::new()
                }
            }
        })
    }
}
