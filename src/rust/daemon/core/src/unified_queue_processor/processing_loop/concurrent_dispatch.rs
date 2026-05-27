//! Generic dispatch driver for the unified queue processor.
//!
//! Factored out of `batch_processing.rs` so that the control flow — spawn
//! up to `max_concurrent_items` futures, drain on cancellation, re-lease on
//! memory pressure, apply inter-dispatch delay — can be unit-tested with a
//! stubbed `spawn_item` and a stubbed pressure predicate, without standing
//! up the full Qdrant/embedding pipeline.

use std::collections::VecDeque;
use std::sync::Arc;
use std::time::Duration;

use futures::stream::{FuturesUnordered, StreamExt};
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, info, warn};

use crate::queue_operations::QueueManager;
use crate::unified_queue_schema::UnifiedQueueItem;

/// Drive the per-batch dispatch loop with `FuturesUnordered`, capped by
/// `item_semaphore`.
///
/// Returns `true` if cancellation was observed mid-batch (caller propagates
/// `Err(())` to the outer poll loop), `false` if the batch ran to completion.
///
/// Behavioral guarantees:
/// - With a 1-permit semaphore the dispatch is strictly sequential — exactly
///   one item runs at a time and `apply_delay` fires between completions.
///   Byte-identical to the legacy `for item in items.iter()` loop.
/// - On cancellation, pending items are re-leased to `Pending` immediately
///   and in-flight items run to completion (no `mark_unified_failed`).
/// - On memory pressure, pending items are re-leased and the legacy 10s
///   in-batch back-off is preserved; in-flight items drain normally.
#[allow(clippy::too_many_arguments)]
pub(super) async fn run_dispatch_loop<F, Spawn, PressureFut, P, DelayFut, D>(
    items: Vec<UnifiedQueueItem>,
    item_semaphore: Arc<tokio::sync::Semaphore>,
    queue_manager: &QueueManager,
    cancellation_token: &CancellationToken,
    mut spawn_item: Spawn,
    mut is_memory_pressure: P,
    mut apply_delay: D,
    max_memory_percent: u8,
) -> bool
where
    Spawn: FnMut(UnifiedQueueItem, tokio::sync::OwnedSemaphorePermit) -> tokio::task::JoinHandle<F>,
    F: Send + 'static,
    P: FnMut() -> PressureFut,
    PressureFut: std::future::Future<Output = bool>,
    D: FnMut() -> DelayFut,
    DelayFut: std::future::Future<Output = ()>,
{
    let mut pending: VecDeque<UnifiedQueueItem> = items.into();
    let mut in_flight: FuturesUnordered<tokio::task::JoinHandle<F>> = FuturesUnordered::new();
    let mut cancelled = false;

    while !pending.is_empty() || !in_flight.is_empty() {
        if cancellation_token.is_cancelled() && !cancelled {
            warn!(
                "Shutdown requested during item processing — draining {} in-flight, re-leasing {} pending",
                in_flight.len(),
                pending.len()
            );
            cancelled = true;
            if !pending.is_empty() {
                let pending_slice: Vec<UnifiedQueueItem> = pending.drain(..).collect();
                re_lease_pending(queue_manager, &pending_slice).await;
            }
        }

        if !cancelled && !pending.is_empty() && is_memory_pressure().await {
            warn!(
                "Memory pressure during batch processing (<{}% available), pausing remaining items",
                100u8.saturating_sub(max_memory_percent)
            );
            // Re-lease ALL pending items (F-044) so they return to pending
            // instead of being stuck in_progress until lease expiry. In-flight
            // items drain to completion below.
            let pending_slice: Vec<UnifiedQueueItem> = pending.drain(..).collect();
            re_lease_pending(queue_manager, &pending_slice).await;
            // Match the legacy 10s back-off inside the batch so the next
            // dispatch cycle does not retry immediately. The outer
            // `handle_memory_pressure` gate in run_poll_cycle re-checks RSS
            // before the next dequeue.
            tokio::time::sleep(Duration::from_secs(10)).await;
        }

        // Dispatch as many items as the semaphore allows.
        while !cancelled && !pending.is_empty() {
            let permit = match Arc::clone(&item_semaphore).try_acquire_owned() {
                Ok(p) => p,
                Err(_) => break,
            };
            let item = pending.pop_front().expect("pending non-empty guarded above");
            in_flight.push(spawn_item(item, permit));
        }

        if in_flight.is_empty() {
            break;
        }

        // Await the next completion. We use FuturesUnordered::next; on
        // cancellation we still want to drain in-flight to avoid half-applied
        // SQLite state, so we do NOT bail out on cancel here.
        match in_flight.next().await {
            Some(Ok(_)) => {}
            Some(Err(join_err)) => {
                if join_err.is_panic() {
                    error!("Spawned item future panicked: {}", join_err);
                } else {
                    debug!("Spawned item future cancelled at runtime: {}", join_err);
                }
            }
            None => break,
        }

        if !cancelled {
            apply_delay().await;
        }
    }

    cancelled
}

/// Re-lease the given items so they return to pending (F-044).
///
/// Without this, items leased as part of the batch but not yet processed would
/// remain `in_progress` until their lease expires, blocking other workers.
pub(super) async fn re_lease_pending(queue_manager: &QueueManager, items: &[UnifiedQueueItem]) {
    if items.is_empty() {
        return;
    }
    let count = items.len();
    let mut failures = 0;
    for item in items {
        if let Err(e) = queue_manager.re_lease_item(&item.queue_id, 30).await {
            warn!(queue_id = %item.queue_id, "Failed to re-lease pending item: {}", e);
            failures += 1;
        }
    }
    info!(
        total = count,
        released = count - failures,
        "Re-leased pending batch items"
    );
}
