//! FTS5 batch writer actor.
//!
//! Decouples per-file FTS5 work from the per-item queue handler. Workers
//! prepare a `Fts5WorkItem` (file content read + hash + diff base already
//! loaded) and `send` it through an mpsc channel. A single long-lived
//! background task drains the channel, accumulates work into batches
//! sized by `BATCH_SIZE` or aged by `BATCH_TIMEOUT`, and commits the
//! whole batch in **one** search.db transaction.
//!
//! Why: under concurrent load the per-item code path was opening one
//! transaction per file against search.db, which collided on the SQLite
//! write lock and surfaced as `database is locked (code 5)` — driving
//! `search_status='failed'` on hundreds of items per minute. With one
//! actor serializing the commit, `SQLITE_BUSY` disappears entirely and
//! the throughput is bounded by FTS5 work itself, not lock contention.
//!
//! The actor takes responsibility for the full post-batch handshake:
//! upserting `indexed_content` cache rows, then recording `search_status`
//! and resolving each item (`batch_finalize`) so completed items leave
//! `unified_queue` without waiting for the next dequeue.

use std::sync::Arc;
use std::time::Duration;

use once_cell::sync::OnceCell;
use sqlx::SqlitePool;
use tokio::sync::mpsc;
use tracing::{debug, info, warn};

use crate::fts_batch_processor::{FileChange, FtsBatchConfig, FtsBatchProcessor};
use crate::indexed_content_schema;
use crate::queue_operations::QueueManager;
use crate::unified_queue_schema::DestinationStatus;

use super::batch_finalize::resolve_search;
use super::SearchDbManager;

/// Global sender installed by the daemon's `UnifiedQueueProcessor::with_search_db`.
///
/// Per-item file handlers ([`crate::strategies::processing::file::ingest`])
/// consult `global_sender()` to decide whether to enqueue FTS5 work to the
/// batch actor or fall back to the inline write path. Lookup is cheap
/// (single atomic load) so it's safe to call on every file.
///
/// Why a global instead of plumbing through `ProcessingContext`: the
/// dispatch chain (`UnifiedQueueProcessor` → `dispatch_nonempty_batch` →
/// `process_batch` → `process_item` → ingest) already threads ~12
/// parameters, and the sender is daemon-singleton state, not per-request
/// state. A `OnceCell` matches how `monitoring::METRICS` is structured
/// elsewhere in the daemon.
static FTS5_SENDER: OnceCell<Fts5Sender> = OnceCell::new();

/// Install the FTS5 batch-writer sender as the daemon-wide default.
///
/// Returns `Err(sender)` if a sender was already installed — the caller
/// can decide whether that's a real bug or a benign re-init (e.g.,
/// test setup running `with_search_db` twice). Production daemons call
/// this exactly once.
pub fn install_global_sender(sender: Fts5Sender) -> Result<(), Fts5Sender> {
    FTS5_SENDER.set(sender)
}

/// Look up the daemon-wide FTS5 sender. Returns `None` when no batch
/// writer has been installed (test daemons, library-only mode, or the
/// search_db feature disabled).
pub fn global_sender() -> Option<&'static Fts5Sender> {
    FTS5_SENDER.get()
}

/// Channel capacity. ~20× the batch size — large enough to absorb the
/// burst of an entire batch's worth of items being prepared concurrently
/// by multiple workers, but small enough that backpressure kicks in if
/// the actor genuinely can't keep up.
pub const FTS5_CHANNEL_CAPACITY: usize = 1024;

/// Maximum files per transaction. Each file's diff is bounded so 50
/// files keeps a single batch under ~200 KB of SQL traffic on average.
pub const FTS5_BATCH_SIZE: usize = 50;

/// Flush a partial batch after this long, even if it hasn't reached
/// `FTS5_BATCH_SIZE`. Keeps latency bounded during low-volume periods.
pub const FTS5_BATCH_TIMEOUT: Duration = Duration::from_millis(500);

/// A single file's FTS5 work, prepared by a queue worker and sent to
/// the actor for batched commit.
///
/// The worker performs the disk read + content hash + old-content lookup
/// up-front, so the actor only does database work. This keeps file IO
/// parallel across workers while serializing writes through one actor.
#[derive(Debug)]
pub struct Fts5WorkItem {
    /// Prepared `FileChange` (file_id, old/new content, tenant, branch, path, hash, etc.)
    pub change: FileChange,
    /// Raw bytes of new content — used to update `indexed_content` cache after commit.
    pub new_content_bytes: Vec<u8>,
    /// Hash of new content — paired with `new_content_bytes` for `indexed_content`.
    pub new_hash: String,
    /// Queue item this work belongs to. The actor uses this to flip
    /// `search_status` and finalize the row after the batch commits.
    pub queue_id: String,
}

/// Sender side of the FTS5 work channel. Cloneable handle stored in
/// `ProcessingContext::fts5_sender` when batched mode is enabled.
pub type Fts5Sender = mpsc::Sender<Fts5WorkItem>;

/// Spawn the batch writer actor and return its sender.
///
/// The actor runs until the channel is dropped (i.e., until every
/// `Fts5Sender` clone is dropped, which only happens at daemon shutdown
/// because the sender lives in `ProcessingContext`).
pub fn spawn(
    search_db: Arc<SearchDbManager>,
    state_pool: SqlitePool,
    queue_manager: Arc<QueueManager>,
) -> Fts5Sender {
    let (tx, rx) = mpsc::channel::<Fts5WorkItem>(FTS5_CHANNEL_CAPACITY);
    let writer = Fts5BatchWriter {
        rx,
        search_db,
        state_pool,
        queue_manager,
    };
    tokio::spawn(writer.run());
    tx
}

struct Fts5BatchWriter {
    rx: mpsc::Receiver<Fts5WorkItem>,
    search_db: Arc<SearchDbManager>,
    state_pool: SqlitePool,
    queue_manager: Arc<QueueManager>,
}

impl Fts5BatchWriter {
    async fn run(mut self) {
        info!(
            "FTS5 batch writer started (batch_size={}, batch_timeout={:?}, channel_capacity={})",
            FTS5_BATCH_SIZE, FTS5_BATCH_TIMEOUT, FTS5_CHANNEL_CAPACITY
        );

        let mut buf: Vec<Fts5WorkItem> = Vec::with_capacity(FTS5_BATCH_SIZE);
        let mut ticker = tokio::time::interval(FTS5_BATCH_TIMEOUT);
        // Skip the immediate first tick — we want timeouts to elapse from
        // the moment buf first becomes non-empty, not from actor start.
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        let _ = ticker.tick().await;

        loop {
            tokio::select! {
                maybe_item = self.rx.recv() => {
                    match maybe_item {
                        Some(item) => {
                            buf.push(item);
                            if buf.len() >= FTS5_BATCH_SIZE {
                                self.flush(&mut buf).await;
                            }
                        }
                        None => {
                            // All senders dropped — drain remaining buffer and exit.
                            if !buf.is_empty() {
                                self.flush(&mut buf).await;
                            }
                            info!("FTS5 batch writer stopping (channel closed)");
                            return;
                        }
                    }
                }
                _ = ticker.tick() => {
                    if !buf.is_empty() {
                        self.flush(&mut buf).await;
                    }
                }
            }
        }
    }

    /// Commit one batch in a single search.db transaction, then update
    /// `indexed_content` cache and finalize each queue item.
    ///
    /// On batch error every item in the batch is marked `search_status=failed`
    /// and `mark_unified_failed` is called for it. This matches the previous
    /// per-item failure semantics — the unified queue's retry path (backoff
    /// via `lease_until`, retry_count) takes over from there.
    async fn flush(&self, buf: &mut Vec<Fts5WorkItem>) {
        let items = std::mem::take(buf);
        let n = items.len();
        let start = std::time::Instant::now();

        // This writer already accumulated a deliberate batch, so force the
        // single-transaction path (the size guard can still fall back to
        // single-file for an oversized change). `flush_forced_batch` replaces
        // the old `flush(usize::MAX)` sentinel, which leaked into logs as the
        // raw `18446744073709551615` queue_depth.
        let mut processor = FtsBatchProcessor::new(&self.search_db, FtsBatchConfig::default());
        for item in &items {
            processor.add_change(item.change.clone());
        }

        match processor.flush_forced_batch().await {
            Ok(stats) => {
                debug!(
                    "FTS5 batch committed: {} files, {} inserted/{} updated/{} deleted lines in {}ms",
                    stats.files_processed,
                    stats.lines_inserted,
                    stats.lines_updated,
                    stats.lines_deleted,
                    stats.processing_time_ms
                );
                self.finalize_success(&items).await;
            }
            Err(e) => {
                warn!(
                    "FTS5 batch failed ({} items): {} — marking search_status=failed and letting unified_queue retry",
                    n, e
                );
                self.finalize_failure(&items).await;
            }
        }

        debug!(
            "FTS5 batch flush of {} items in {}ms",
            n,
            start.elapsed().as_millis()
        );
    }

    /// Post-commit work: update `indexed_content` cache, then record
    /// search=done and resolve each queue item.
    async fn finalize_success(&self, items: &[Fts5WorkItem]) {
        for item in items {
            // Best-effort indexed_content cache update. Failures here are
            // logged but don't change the destination status — the FTS5
            // commit already succeeded.
            if let Err(e) = indexed_content_schema::upsert_indexed_content(
                &self.state_pool,
                item.change.file_id,
                &item.new_content_bytes,
                &item.new_hash,
            )
            .await
            {
                warn!(
                    "indexed_content upsert failed for file_id={} ({}): {}",
                    item.change.file_id, item.change.file_path, e
                );
            }

            resolve_search(&self.queue_manager, &item.queue_id, DestinationStatus::Done).await;
        }
    }

    /// The batch error itself is logged by `flush`; each item's retry path
    /// records the transient message (`batch_finalize::fts5_failure_message`).
    async fn finalize_failure(&self, items: &[Fts5WorkItem]) {
        for item in items {
            resolve_search(
                &self.queue_manager,
                &item.queue_id,
                DestinationStatus::Failed,
            )
            .await;
        }
    }
}
