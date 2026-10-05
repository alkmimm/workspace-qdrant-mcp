//! Reconciles linked git worktrees as they appear, instead of at the next tenant
//! scan (see `workspace_qdrant_core::branch_switch::WorktreeDiscovery`).

use std::sync::Arc;

use sqlx::SqlitePool;
use tokio::task::JoinHandle;
use tracing::info;

use workspace_qdrant_core::branch_switch::WorktreeDiscovery;
use workspace_qdrant_core::queue_operations::QueueManager;
use workspace_qdrant_core::AllowedExtensions;

/// Default seconds between passes. A new worktree is reconciled after it was
/// seen on two consecutive passes, so within one to two intervals.
const DEFAULT_INTERVAL_SECS: u64 = 60;

/// Spawn the discovery loop. Interval override: `WQM_WORKTREE_DISCOVERY_SECS`
/// (10 s floor — each pass reads every project's `.git/worktrees`).
pub fn start(pool: SqlitePool, allowed_extensions: Arc<AllowedExtensions>) -> JoinHandle<()> {
    let secs = std::env::var("WQM_WORKTREE_DISCOVERY_SECS")
        .ok()
        .and_then(|s| s.parse::<u64>().ok())
        .map(|s| s.max(10))
        .unwrap_or(DEFAULT_INTERVAL_SECS);
    tokio::spawn(async move {
        let queue_manager = QueueManager::new(pool.clone());
        let mut discovery = WorktreeDiscovery::default();
        let mut interval = tokio::time::interval(tokio::time::Duration::from_secs(secs));
        info!(interval_secs = secs, "Worktree discovery started");
        loop {
            interval.tick().await;
            let enqueued = discovery
                .tick(&pool, &queue_manager, &allowed_extensions)
                .await;
            if enqueued > 0 {
                info!(
                    enqueued,
                    "Worktree discovery reconciled new or moved worktrees"
                );
            }
        }
    })
}
