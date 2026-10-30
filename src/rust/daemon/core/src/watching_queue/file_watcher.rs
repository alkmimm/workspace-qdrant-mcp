//! FileWatcherQueue - main file watcher with queue integration.

use std::sync::Arc;
use std::time::SystemTime;

use notify::{Event, RecursiveMode, Watcher as NotifyWatcher};
use tokio::sync::{mpsc, Mutex, RwLock};
use tracing::{error, info, warn};

use crate::allowed_extensions::AllowedExtensions;
use crate::patterns::global_ignore::resolve_global_ignore_path;
use crate::queue_operations::QueueManager;

use super::dir_watches::{can_change_content, dir_changes, DirChange, DirWatches};
use super::error_state::{WatchErrorState, WatchErrorTracker};
use super::error_types::WatchHealthStatus;
use super::throttle::{QueueThrottleState, QueueThrottleSummary};
use super::types::{
    CompiledPatterns, EventDebouncer, FileEvent, WatchConfig, WatchingQueueError,
    WatchingQueueResult, WatchingQueueStats,
};

type WatcherSlot = Option<Box<dyn NotifyWatcher + Send + Sync>>;

/// Apply one batch of directory changes; `false` once the watcher is gone. An
/// ignore-file change re-syncs the whole set, which subsumes the rest.
fn apply_dir_changes(
    mut watcher: tokio::sync::OwnedMutexGuard<WatcherSlot>,
    mut dirs: tokio::sync::OwnedMutexGuard<DirWatches>,
    batch: &[DirChange],
) -> bool {
    let Some(w) = watcher.as_mut() else {
        return false;
    };
    let w: &mut dyn NotifyWatcher = w.as_mut();
    let result = if batch.contains(&DirChange::IgnoreRulesChanged) {
        dirs.sync(w)
    } else {
        batch.iter().try_for_each(|change| dirs.apply(w, change))
    };
    if let Err(e) = result {
        warn!("File watcher could not follow a directory change: {}", e);
    }
    true
}

/// Main file watcher with queue integration
pub struct FileWatcherQueue {
    pub(super) config: Arc<RwLock<WatchConfig>>,
    pub(super) patterns: Arc<RwLock<CompiledPatterns>>,
    pub(super) queue_manager: Arc<QueueManager>,
    pub(super) allowed_extensions: Arc<AllowedExtensions>,
    pub(super) debouncer: Arc<Mutex<EventDebouncer>>,
    pub(super) watcher: Arc<Mutex<Option<Box<dyn NotifyWatcher + Send + Sync>>>>,
    /// The directories `watcher` registered, one non-recursive watch each.
    pub(super) dir_watches: Arc<Mutex<DirWatches>>,
    /// Follows directories created, renamed or re-included after start.
    pub(super) dir_follower: Arc<Mutex<Option<tokio::task::JoinHandle<()>>>>,
    pub(super) event_receiver: Arc<Mutex<Option<mpsc::UnboundedReceiver<FileEvent>>>>,
    pub(super) processor_handle: Arc<Mutex<Option<tokio::task::JoinHandle<()>>>>,

    // Error tracking (Task 461.5)
    pub(super) error_tracker: Arc<WatchErrorTracker>,

    // Queue depth throttling (Task 461.8)
    pub(super) throttle_state: Arc<QueueThrottleState>,

    // Statistics
    pub(super) events_received: Arc<Mutex<u64>>,
    pub(super) events_processed: Arc<Mutex<u64>>,
    pub(super) events_filtered: Arc<Mutex<u64>>,
    pub(super) queue_errors: Arc<Mutex<u64>>,
    pub(super) events_throttled: Arc<Mutex<u64>>,
}

impl FileWatcherQueue {
    /// Create a new file watcher with queue integration
    pub fn new(
        config: WatchConfig,
        queue_manager: Arc<QueueManager>,
        allowed_extensions: Arc<AllowedExtensions>,
    ) -> WatchingQueueResult<Self> {
        let patterns = CompiledPatterns::new(&config)?;
        let debouncer = EventDebouncer::new(config.debounce_ms);
        let error_tracker = WatchErrorTracker::new();
        let throttle_state = QueueThrottleState::new();
        let dir_watches = DirWatches::new(config.path.clone(), resolve_global_ignore_path());

        Ok(Self {
            config: Arc::new(RwLock::new(config)),
            patterns: Arc::new(RwLock::new(patterns)),
            queue_manager,
            allowed_extensions,
            debouncer: Arc::new(Mutex::new(debouncer)),
            watcher: Arc::new(Mutex::new(None)),
            dir_watches: Arc::new(Mutex::new(dir_watches)),
            dir_follower: Arc::new(Mutex::new(None)),
            event_receiver: Arc::new(Mutex::new(None)),
            processor_handle: Arc::new(Mutex::new(None)),
            error_tracker: Arc::new(error_tracker),
            throttle_state: Arc::new(throttle_state),
            events_received: Arc::new(Mutex::new(0)),
            events_processed: Arc::new(Mutex::new(0)),
            events_filtered: Arc::new(Mutex::new(0)),
            queue_errors: Arc::new(Mutex::new(0)),
            events_throttled: Arc::new(Mutex::new(0)),
        })
    }

    /// Start watching the configured path
    pub async fn start(&self) -> WatchingQueueResult<()> {
        let config = self.config.read().await;

        // Validate path exists
        if !config.path.exists() {
            return Err(WatchingQueueError::Config {
                message: format!("Path does not exist: {}", config.path.display()),
            });
        }

        // Create file system watcher
        let (tx, rx) = mpsc::unbounded_channel();
        let tx_clone = tx.clone();
        let (dir_tx, dir_rx) = mpsc::unbounded_channel();
        let follow_dirs = config.recursive;

        let watcher: Box<dyn NotifyWatcher + Send + Sync> =
            Box::new(notify::RecommendedWatcher::new(
                move |result| {
                    if let Ok(event) = result {
                        if follow_dirs {
                            for change in dir_changes(&event) {
                                let _ = dir_tx.send(change);
                            }
                        }
                        Self::handle_notify_event(event, &tx_clone);
                    }
                },
                notify::Config::default(),
            )?);

        // Register the watches: recursive mode watches each directory the
        // project walk keeps, one non-recursive watch apiece (see `dir_watches`);
        // a single non-recursive watch on the root otherwise.
        let watched_dirs = {
            let mut watcher_lock = self.watcher.lock().await;
            let mut w = watcher;
            let registered = if config.recursive {
                let mut dirs = self.dir_watches.lock().await;
                dirs.sync(w.as_mut()).map(|()| dirs.len())
            } else {
                w.watch(&config.path, RecursiveMode::NonRecursive)
                    .map(|()| 1)
            };
            // On failure `w` is dropped here, which releases every watch it
            // registered before the refusal.
            let count = registered?;
            *watcher_lock = Some(w);
            count
        };
        if config.recursive {
            self.start_dir_follower(dir_rx).await;
        }

        // Store receiver
        {
            let mut receiver_lock = self.event_receiver.lock().await;
            *receiver_lock = Some(rx);
        }

        // Start event processor
        self.start_event_processor().await?;

        info!(
            "Started file watcher: {} (recursive: {}, {} director{} watched)",
            config.path.display(),
            config.recursive,
            watched_dirs,
            if watched_dirs == 1 { "y" } else { "ies" }
        );

        Ok(())
    }

    /// Follow directory changes reported by the notify callback: watch what
    /// appears, forget what goes, re-sync the whole set when an ignore file
    /// changes. Each batch runs off the async workers: registering a large new
    /// subtree is a blocking walk plus one syscall per directory.
    async fn start_dir_follower(&self, mut changes: mpsc::UnboundedReceiver<DirChange>) {
        let watcher = Arc::clone(&self.watcher);
        let dir_watches = Arc::clone(&self.dir_watches);
        let handle = tokio::spawn(async move {
            while let Some(first) = changes.recv().await {
                let mut batch = vec![first];
                while let Ok(more) = changes.try_recv() {
                    batch.push(more);
                }
                let watcher_guard = Arc::clone(&watcher).lock_owned().await;
                let dirs_guard = Arc::clone(&dir_watches).lock_owned().await;
                let applied = tokio::task::spawn_blocking(move || {
                    apply_dir_changes(watcher_guard, dirs_guard, &batch)
                })
                .await;
                if !matches!(applied, Ok(true)) {
                    break; // watcher stopped
                }
            }
        });
        *self.dir_follower.lock().await = Some(handle);
    }

    /// Stop watching
    pub async fn stop(&self) -> WatchingQueueResult<()> {
        // Stop processor
        {
            let mut handle_lock = self.processor_handle.lock().await;
            if let Some(handle) = handle_lock.take() {
                handle.abort();
            }
        }

        // Stop following directories, then drop the watcher (and its watches)
        {
            let mut follower_lock = self.dir_follower.lock().await;
            if let Some(handle) = follower_lock.take() {
                handle.abort();
            }
        }
        {
            let mut watcher_lock = self.watcher.lock().await;
            *watcher_lock = None;
        }
        self.dir_watches.lock().await.clear();

        // Clear receiver
        {
            let mut receiver_lock = self.event_receiver.lock().await;
            *receiver_lock = None;
        }

        info!("Stopped file watcher");
        Ok(())
    }

    /// Handle notify event
    fn handle_notify_event(event: Event, tx: &mpsc::UnboundedSender<FileEvent>) {
        // Content indexing only cares about events that can change file CONTENT.
        // Drop access (reads) and metadata-only (atime/ctime/chmod/chown) events:
        // across the Docker bind-mount of the WSL ext4 tree these fire spuriously
        // (and the daemon's own reads bump atime), previously churning the queue
        // with an endless stream of hash-skip `File/Update` no-ops that kept the
        // index from ever settling. Real edits arrive as Create / Remove /
        // Modify(Data) / Modify(Name) / Modify(Any), which all pass through.
        if !can_change_content(&event.kind) {
            tracing::debug!(
                "Ignoring non-content notify event kind={:?} paths={:?}",
                event.kind,
                event.paths
            );
            return;
        }
        tracing::debug!("notify event kind={:?} paths={:?}", event.kind, event.paths);

        let timestamp = SystemTime::now();

        for path in event.paths {
            let file_event = FileEvent {
                path,
                event_kind: event.kind,
                timestamp,
            };

            if let Err(e) = tx.send(file_event) {
                error!("Failed to send file event: {}", e);
            }
        }
    }

    /// Start event processing task
    async fn start_event_processor(&self) -> WatchingQueueResult<()> {
        {
            let handle_lock = self.processor_handle.lock().await;
            if handle_lock.is_some() {
                return Ok(());
            }
        }

        let event_receiver = self.event_receiver.clone();
        let debouncer = self.debouncer.clone();
        let patterns = self.patterns.clone();
        let config = self.config.clone();
        let queue_manager = self.queue_manager.clone();
        let allowed_extensions = self.allowed_extensions.clone();
        let error_tracker = self.error_tracker.clone();
        let throttle_state = self.throttle_state.clone();
        let events_received = self.events_received.clone();
        let events_processed = self.events_processed.clone();
        let events_filtered = self.events_filtered.clone();
        let queue_errors = self.queue_errors.clone();
        let events_throttled = self.events_throttled.clone();

        let handle = tokio::spawn(async move {
            Self::event_processing_loop(
                event_receiver,
                debouncer,
                patterns,
                config,
                queue_manager,
                allowed_extensions,
                error_tracker,
                throttle_state,
                events_received,
                events_processed,
                events_filtered,
                queue_errors,
                events_throttled,
            )
            .await;
        });

        {
            let mut handle_lock = self.processor_handle.lock().await;
            *handle_lock = Some(handle);
        }

        Ok(())
    }

    /// Get current statistics
    pub async fn get_stats(&self) -> WatchingQueueStats {
        WatchingQueueStats {
            events_received: *self.events_received.lock().await,
            events_processed: *self.events_processed.lock().await,
            events_filtered: *self.events_filtered.lock().await,
            queue_errors: *self.queue_errors.lock().await,
            events_throttled: *self.events_throttled.lock().await,
        }
    }

    /// Get the throttle state reference (Task 461.8)
    pub fn throttle_state(&self) -> &Arc<QueueThrottleState> {
        &self.throttle_state
    }

    /// Get throttle summary for this watcher (Task 461.8)
    pub async fn get_throttle_summary(&self) -> QueueThrottleSummary {
        self.throttle_state.get_summary().await
    }

    /// Get error state for this watcher (Task 461.5)
    pub async fn get_error_state(&self) -> Option<WatchErrorState> {
        let config = self.config.read().await;
        self.error_tracker.get_state(&config.id)
    }

    /// Get the error tracker reference (Task 461.5)
    pub fn error_tracker(&self) -> &Arc<WatchErrorTracker> {
        &self.error_tracker
    }

    /// Get the watch_id for this watcher
    pub async fn watch_id(&self) -> String {
        let config = self.config.read().await;
        config.id.clone()
    }

    /// Get health status for this watcher (Task 461.5)
    pub async fn get_health_status(&self) -> WatchHealthStatus {
        let config = self.config.read().await;
        self.error_tracker.get_health_status(&config.id).await
    }
}
