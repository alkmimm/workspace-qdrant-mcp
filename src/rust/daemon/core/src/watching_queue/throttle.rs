//! Queue depth monitoring and adaptive throttling (Task 461.8).
//!
//! A throttled event is DEFERRED, never dropped. Until 2026-09-19 it was
//! dropped: `should_throttle` skipped 1 in 2 events above 1 000 pending
//! items and 3 in 4 above 5 000, and the F-045 flag that was meant to
//! trigger a catch-up reconcile had no consumer outside its own tests. The
//! queue sits above 1 000 for the better part of an hour after every daemon
//! restart (the worktree re-enqueue) and for the whole of any tenant
//! re-embed, so half to three quarters of the edits made in ANY watched
//! project during those windows were silently never indexed until the next
//! restart's startup reconciliation found them. Now the held-back events
//! sit in [`QueueThrottleState::defer`], one per path (the last event for a
//! path wins, so a Remove after a Modify is not replayed as an Update), and
//! the watcher tick releases them at a rate the load allows.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use serde::{Deserialize, Serialize};
use tracing::{info, warn};

use super::types::FileEvent;
use crate::queue_operations::QueueManager;

/// Deferred events released per watcher tick (500 ms) while the load is
/// High / Critical: a bounded trickle, so a live edit made during a re-embed
/// still reaches the index within seconds instead of hours. Under Normal
/// load the whole buffer is released at once.
pub const DEFER_RELEASE_HIGH_PER_TICK: usize = 25;
pub const DEFER_RELEASE_CRITICAL_PER_TICK: usize = 5;
/// Above this many distinct deferred paths the buffer stops growing and the
/// F-045 flag is raised (and logged): a tree that large under sustained
/// pressure needs a scan, not a buffer. One entry per path — the bound is
/// the size of the watched trees, ~100 bytes each.
pub const DEFER_CAP: usize = 250_000;

/// Queue load level for adaptive throttling
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum QueueLoadLevel {
    /// Normal load - no throttling needed
    Normal,
    /// High load - moderate throttling recommended
    High,
    /// Critical load - aggressive throttling required
    Critical,
}

impl QueueLoadLevel {
    pub fn as_str(&self) -> &'static str {
        match self {
            QueueLoadLevel::Normal => "normal",
            QueueLoadLevel::High => "high",
            QueueLoadLevel::Critical => "critical",
        }
    }
}

/// Configuration for queue depth throttling
#[derive(Debug, Clone)]
pub struct QueueThrottleConfig {
    /// Queue depth threshold for high load (default: 1000)
    pub high_threshold: i64,
    /// Queue depth threshold for critical load (default: 5000)
    pub critical_threshold: i64,
    /// How often to check queue depth in milliseconds (default: 5000)
    pub check_interval_ms: u64,
    /// Skip ratio when in high load (skip 1 in N events, default: 2)
    pub high_skip_ratio: u64,
    /// Skip ratio when in critical load (skip 1 in N events, default: 4)
    pub critical_skip_ratio: u64,
}

impl Default for QueueThrottleConfig {
    fn default() -> Self {
        Self {
            high_threshold: 1000,
            critical_threshold: 5000,
            check_interval_ms: 5000,
            high_skip_ratio: 2,
            critical_skip_ratio: 4,
        }
    }
}

/// State for queue depth throttling
#[derive(Debug)]
pub struct QueueThrottleState {
    /// Current queue depth (periodically updated)
    current_depth: Arc<tokio::sync::RwLock<i64>>,
    /// Current load level
    load_level: Arc<tokio::sync::RwLock<QueueLoadLevel>>,
    /// Per-collection depths
    collection_depths: Arc<tokio::sync::RwLock<HashMap<String, i64>>>,
    /// Event counter for skip ratio calculation
    event_counter: Arc<std::sync::atomic::AtomicU64>,
    /// Configuration
    config: QueueThrottleConfig,
    /// Last check timestamp
    last_check: Arc<tokio::sync::RwLock<SystemTime>>,
    /// Raised when the deferred buffer overflowed [`DEFER_CAP`] and an event
    /// really was dropped (F-045). Nothing consumes it yet — it is logged at
    /// the point of raising so the loss is at least visible.
    needs_full_reconcile: Arc<AtomicBool>,
    /// Events held back by the throttle, one per path, the last one wins.
    deferred: Arc<tokio::sync::Mutex<HashMap<PathBuf, FileEvent>>>,
}

impl QueueThrottleState {
    /// Create a new throttle state with default configuration
    pub fn new() -> Self {
        Self::with_config(QueueThrottleConfig::default())
    }

    /// Create a new throttle state with custom configuration
    pub fn with_config(config: QueueThrottleConfig) -> Self {
        Self {
            current_depth: Arc::new(tokio::sync::RwLock::new(0)),
            load_level: Arc::new(tokio::sync::RwLock::new(QueueLoadLevel::Normal)),
            collection_depths: Arc::new(tokio::sync::RwLock::new(HashMap::new())),
            event_counter: Arc::new(std::sync::atomic::AtomicU64::new(0)),
            config,
            last_check: Arc::new(tokio::sync::RwLock::new(SystemTime::UNIX_EPOCH)),
            needs_full_reconcile: Arc::new(AtomicBool::new(false)),
            deferred: Arc::new(tokio::sync::Mutex::new(HashMap::new())),
        }
    }

    /// Update queue depth from queue manager (using unified_queue)
    pub async fn update_from_queue(&self, queue_manager: &QueueManager) {
        match queue_manager.get_unified_queue_depth(None, None).await {
            Ok(depth) => {
                let mut current = self.current_depth.write().await;
                *current = depth;

                // Update load level
                let new_level = if depth >= self.config.critical_threshold {
                    QueueLoadLevel::Critical
                } else if depth >= self.config.high_threshold {
                    QueueLoadLevel::High
                } else {
                    QueueLoadLevel::Normal
                };

                let mut level = self.load_level.write().await;
                if *level != new_level {
                    info!(
                        "Queue load level changed: {:?} -> {:?} (depth: {})",
                        *level, new_level, depth
                    );
                }
                *level = new_level;

                // Update last check time
                let mut last = self.last_check.write().await;
                *last = SystemTime::now();
            }
            Err(e) => {
                warn!("Failed to get queue depth: {}", e);
            }
        }

        // Also update per-collection depths (using unified_queue)
        match queue_manager
            .get_unified_queue_depth_all_collections()
            .await
        {
            Ok(depths) => {
                let mut collection_depths = self.collection_depths.write().await;
                *collection_depths = depths;
            }
            Err(e) => {
                warn!("Failed to get per-collection queue depths: {}", e);
            }
        }
    }

    /// Check if we should throttle (DEFER this event — see the module docs).
    ///
    /// An event is held back unless its ordinal is a multiple of the level's
    /// skip ratio: every other event at High (ratio 2), three in four at
    /// Critical (ratio 4). The rest enqueue right away. A held event goes to
    /// [`Self::defer`], never to the floor.
    pub async fn should_throttle(&self) -> bool {
        let level = *self.load_level.read().await;
        let ratio = match level {
            QueueLoadLevel::Normal => return false,
            QueueLoadLevel::High => self.config.high_skip_ratio,
            QueueLoadLevel::Critical => self.config.critical_skip_ratio,
        };
        let count = self
            .event_counter
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        count % ratio != 0
    }

    /// Hold an event back for a later tick. One entry per path — the LAST
    /// event for a path wins, so a Remove that follows a Modify is replayed
    /// as a Remove and a burst of Modifies collapses into one Update. Above
    /// [`DEFER_CAP`] distinct paths the event is dropped and the F-045 flag
    /// raised: that is the only way an event is lost now, and it is logged.
    pub(super) async fn defer(&self, event: FileEvent) {
        let mut deferred = self.deferred.lock().await;
        if deferred.len() >= DEFER_CAP && !deferred.contains_key(&event.path) {
            if !self
                .needs_full_reconcile
                .swap(true, std::sync::atomic::Ordering::AcqRel)
            {
                warn!(
                    "throttle: deferred-event buffer full ({} paths) — dropping {} and every further new path until the load eases; a startup reconciliation will be needed (F-045)",
                    DEFER_CAP,
                    event.path.display()
                );
            }
            return;
        }
        deferred.insert(event.path.clone(), event);
    }

    /// Release up to `max` deferred events (any order) for enqueueing. The
    /// caller must NOT run them through [`Self::should_throttle`] again, or
    /// they would bounce back in.
    pub(super) async fn drain_deferred(&self, max: usize) -> Vec<FileEvent> {
        let mut deferred = self.deferred.lock().await;
        if deferred.is_empty() || max == 0 {
            return Vec::new();
        }
        let keys: Vec<PathBuf> = deferred.keys().take(max).cloned().collect();
        keys.into_iter()
            .filter_map(|k| deferred.remove(&k))
            .collect()
    }

    /// How many deferred events wait for a release.
    pub async fn deferred_len(&self) -> usize {
        self.deferred.lock().await.len()
    }

    /// How many deferred events the current load level allows this tick to
    /// release: everything under Normal, a bounded trickle otherwise.
    pub async fn release_budget(&self) -> usize {
        match *self.load_level.read().await {
            QueueLoadLevel::Normal => usize::MAX,
            QueueLoadLevel::High => DEFER_RELEASE_HIGH_PER_TICK,
            QueueLoadLevel::Critical => DEFER_RELEASE_CRITICAL_PER_TICK,
        }
    }

    /// Returns `true` and clears the flag if the deferred buffer overflowed
    /// and events were dropped (F-045).
    pub fn take_needs_full_reconcile(&self) -> bool {
        self.needs_full_reconcile
            .swap(false, std::sync::atomic::Ordering::AcqRel)
    }

    /// Check if we need to refresh queue depth (time-based)
    pub async fn needs_refresh(&self) -> bool {
        let last = *self.last_check.read().await;
        let elapsed = SystemTime::now()
            .duration_since(last)
            .unwrap_or(Duration::ZERO);
        elapsed >= Duration::from_millis(self.config.check_interval_ms)
    }

    /// Get current queue depth
    pub async fn get_depth(&self) -> i64 {
        *self.current_depth.read().await
    }

    /// Get current load level
    pub async fn get_load_level(&self) -> QueueLoadLevel {
        *self.load_level.read().await
    }

    /// Get queue depth for a specific collection
    pub async fn get_collection_depth(&self, collection: &str) -> i64 {
        let depths = self.collection_depths.read().await;
        depths.get(collection).copied().unwrap_or(0)
    }

    /// Get throttle summary for telemetry
    pub async fn get_summary(&self) -> QueueThrottleSummary {
        QueueThrottleSummary {
            total_depth: *self.current_depth.read().await,
            load_level: *self.load_level.read().await,
            events_processed: self.event_counter.load(std::sync::atomic::Ordering::SeqCst),
            high_threshold: self.config.high_threshold,
            critical_threshold: self.config.critical_threshold,
            deferred_events: self.deferred.lock().await.len(),
        }
    }
}

impl Default for QueueThrottleState {
    fn default() -> Self {
        Self::new()
    }
}

/// Summary of throttle state for telemetry (Task 461.8)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QueueThrottleSummary {
    pub total_depth: i64,
    pub load_level: QueueLoadLevel,
    pub events_processed: u64,
    pub high_threshold: i64,
    pub critical_threshold: i64,
    /// Events held back by the throttle and not yet released.
    pub deferred_events: usize,
}

#[cfg(test)]
mod tests {
    use super::*;
    use notify::EventKind;

    /// Force the state to a given load level + depth. Production code only
    /// transitions via `update_from_queue`, which needs a real QueueManager;
    /// these helpers let unit tests exercise the pure throttling math.
    async fn force_state(state: &QueueThrottleState, depth: i64, level: QueueLoadLevel) {
        *state.current_depth.write().await = depth;
        *state.load_level.write().await = level;
    }

    #[test]
    fn load_level_str_round_trip() {
        assert_eq!(QueueLoadLevel::Normal.as_str(), "normal");
        assert_eq!(QueueLoadLevel::High.as_str(), "high");
        assert_eq!(QueueLoadLevel::Critical.as_str(), "critical");
    }

    #[test]
    fn default_config_uses_documented_thresholds() {
        let c = QueueThrottleConfig::default();
        assert_eq!(c.high_threshold, 1000);
        assert_eq!(c.critical_threshold, 5000);
        assert_eq!(c.check_interval_ms, 5000);
        assert_eq!(c.high_skip_ratio, 2);
        assert_eq!(c.critical_skip_ratio, 4);
    }

    #[tokio::test]
    async fn initial_state_is_normal_load_zero_depth() {
        let s = QueueThrottleState::new();
        assert_eq!(s.get_depth().await, 0);
        assert_eq!(s.get_load_level().await, QueueLoadLevel::Normal);
        assert!(!s.take_needs_full_reconcile());
    }

    #[tokio::test]
    async fn should_throttle_normal_always_false() {
        let s = QueueThrottleState::new();
        for _ in 0..10 {
            assert!(!s.should_throttle().await);
        }
        // F-045 flag stays clear under normal load.
        assert!(!s.take_needs_full_reconcile());
    }

    #[tokio::test]
    async fn should_throttle_high_skips_one_in_n() {
        // With high_skip_ratio=2 the pattern is throttle/keep/throttle/keep...
        // (count starts at 0 → 0 % 2 == 0 → keep; 1 % 2 != 0 → throttle).
        let s = QueueThrottleState::new();
        force_state(&s, 1500, QueueLoadLevel::High).await;

        let mut kept = 0usize;
        let mut throttled = 0usize;
        for _ in 0..100 {
            if s.should_throttle().await {
                throttled += 1;
            } else {
                kept += 1;
            }
        }
        assert_eq!(kept, 50);
        assert_eq!(throttled, 50);
        // High load alone never sets the reconcile flag — that's
        // reserved for Critical.
        assert!(!s.take_needs_full_reconcile());
    }

    #[tokio::test]
    async fn should_throttle_critical_holds_three_in_four_and_loses_nothing() {
        let s = QueueThrottleState::new();
        force_state(&s, 6000, QueueLoadLevel::Critical).await;

        let mut throttled = 0usize;
        for _ in 0..100 {
            if s.should_throttle().await {
                throttled += 1;
            }
        }
        // skip_ratio = 4 → hold when count % 4 != 0 → 75/100.
        assert_eq!(throttled, 75);
        // Holding is not losing: the F-045 flag is reserved for a buffer
        // overflow, which throttling alone never causes.
        assert!(!s.take_needs_full_reconcile());
    }

    fn ev(path: &str, kind: EventKind) -> FileEvent {
        FileEvent {
            path: PathBuf::from(path),
            event_kind: kind,
            timestamp: SystemTime::now(),
        }
    }

    /// One entry per path, last event wins: a Remove after a Modify must be
    /// replayed as a Remove (a Modify on a path that no longer exists would
    /// resolve to Delete anyway, but the buffer must not resurrect a Create),
    /// and a burst of Modifies collapses into one entry.
    #[tokio::test]
    async fn defer_keeps_the_last_event_per_path() {
        use notify::event::{CreateKind, ModifyKind, RemoveKind};
        let s = QueueThrottleState::new();
        s.defer(ev("/w/a.rs", EventKind::Create(CreateKind::File)))
            .await;
        s.defer(ev("/w/a.rs", EventKind::Modify(ModifyKind::Any)))
            .await;
        s.defer(ev("/w/a.rs", EventKind::Remove(RemoveKind::File)))
            .await;
        for _ in 0..5 {
            s.defer(ev("/w/b.rs", EventKind::Modify(ModifyKind::Any)))
                .await;
        }
        assert_eq!(s.deferred_len().await, 2);
        let mut out = s.drain_deferred(usize::MAX).await;
        out.sort_by(|x, y| x.path.cmp(&y.path));
        assert!(
            matches!(out[0].event_kind, EventKind::Remove(_)),
            "{:?}",
            out[0]
        );
        assert!(
            matches!(out[1].event_kind, EventKind::Modify(_)),
            "{:?}",
            out[1]
        );
        assert_eq!(
            s.deferred_len().await,
            0,
            "drained entries leave the buffer"
        );
        assert!(!s.take_needs_full_reconcile());
    }

    /// The release budget follows the load: everything under Normal, a
    /// bounded trickle under High / Critical, and `drain_deferred` honours
    /// the number it is given.
    #[tokio::test]
    async fn release_budget_follows_load_and_drain_respects_max() {
        use notify::event::ModifyKind;
        let s = QueueThrottleState::new();
        for i in 0..40 {
            s.defer(ev(
                &format!("/w/{i}.rs"),
                EventKind::Modify(ModifyKind::Any),
            ))
            .await;
        }
        force_state(&s, 6000, QueueLoadLevel::Critical).await;
        assert_eq!(s.release_budget().await, DEFER_RELEASE_CRITICAL_PER_TICK);
        let batch = s.drain_deferred(s.release_budget().await).await;
        assert_eq!(batch.len(), DEFER_RELEASE_CRITICAL_PER_TICK);
        assert_eq!(s.deferred_len().await, 40 - DEFER_RELEASE_CRITICAL_PER_TICK);

        force_state(&s, 1500, QueueLoadLevel::High).await;
        assert_eq!(s.release_budget().await, DEFER_RELEASE_HIGH_PER_TICK);
        let batch = s.drain_deferred(s.release_budget().await).await;
        assert_eq!(batch.len(), DEFER_RELEASE_HIGH_PER_TICK);

        force_state(&s, 10, QueueLoadLevel::Normal).await;
        assert_eq!(s.release_budget().await, usize::MAX);
        let rest = s.drain_deferred(s.release_budget().await).await;
        assert_eq!(
            rest.len(),
            40 - DEFER_RELEASE_CRITICAL_PER_TICK - DEFER_RELEASE_HIGH_PER_TICK
        );
        assert_eq!(s.deferred_len().await, 0);
        assert!(s.drain_deferred(usize::MAX).await.is_empty());
    }

    /// Only an overflow of the deferred buffer raises the F-045 flag, and the
    /// flag is consumed on read. A path already in the buffer is still
    /// updated past the cap (it costs no new entry).
    #[tokio::test]
    async fn only_buffer_overflow_raises_the_reconcile_flag() {
        use notify::event::{ModifyKind, RemoveKind};
        let s = QueueThrottleState::new();
        {
            let mut d = s.deferred.lock().await;
            for i in 0..DEFER_CAP {
                let e = ev(&format!("/w/{i}"), EventKind::Modify(ModifyKind::Any));
                d.insert(e.path.clone(), e);
            }
        }
        assert!(!s.take_needs_full_reconcile());
        s.defer(ev("/w/0", EventKind::Remove(RemoveKind::File)))
            .await;
        assert!(
            !s.take_needs_full_reconcile(),
            "an existing path is updated, not dropped"
        );
        s.defer(ev("/w/new", EventKind::Modify(ModifyKind::Any)))
            .await;
        assert_eq!(
            s.deferred_len().await,
            DEFER_CAP,
            "the new path was dropped"
        );
        assert!(s.take_needs_full_reconcile());
        // Second read after consumption returns false (flag was cleared).
        assert!(!s.take_needs_full_reconcile());
    }

    #[tokio::test]
    async fn needs_refresh_initially_true() {
        // last_check starts at UNIX_EPOCH, so any positive interval has
        // already elapsed and a refresh is owed.
        let s = QueueThrottleState::new();
        assert!(s.needs_refresh().await);
    }

    #[tokio::test]
    async fn get_collection_depth_returns_zero_for_unknown() {
        let s = QueueThrottleState::new();
        assert_eq!(s.get_collection_depth("never-seen").await, 0);
    }

    #[tokio::test]
    async fn get_summary_reflects_state_and_config() {
        let cfg = QueueThrottleConfig {
            high_threshold: 100,
            critical_threshold: 500,
            check_interval_ms: 1000,
            high_skip_ratio: 3,
            critical_skip_ratio: 5,
        };
        let s = QueueThrottleState::with_config(cfg);
        force_state(&s, 250, QueueLoadLevel::High).await;
        // Drive the event counter so the summary's events_processed is
        // distinguishable from a fresh state.
        for _ in 0..6 {
            let _ = s.should_throttle().await;
        }

        let summary = s.get_summary().await;
        assert_eq!(summary.total_depth, 250);
        assert_eq!(summary.load_level, QueueLoadLevel::High);
        assert_eq!(summary.events_processed, 6);
        assert_eq!(summary.high_threshold, 100);
        assert_eq!(summary.critical_threshold, 500);
    }
}
