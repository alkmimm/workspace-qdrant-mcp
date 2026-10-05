//! Follows local branches nobody has checked out (see
//! `workspace_qdrant_core::branch_switch::BranchTipFollower`).

use std::sync::Arc;

use sqlx::SqlitePool;
use tokio::task::JoinHandle;
use tracing::info;

use workspace_qdrant_core::branch_switch::BranchTipFollower;
use workspace_qdrant_core::queue_operations::QueueManager;
use workspace_qdrant_core::AllowedExtensions;

/// Default seconds between passes. A pass only reads git refs unless a
/// branch's tip moved since it was last checked.
const DEFAULT_INTERVAL_SECS: u64 = 120;

/// Spawn the follower. Interval: `WQM_BRANCH_TIP_FOLLOW_SECS` (floor 30; `0`
/// turns it off).
pub fn start(
    pool: SqlitePool,
    allowed_extensions: Arc<AllowedExtensions>,
) -> Option<JoinHandle<()>> {
    let secs = match std::env::var("WQM_BRANCH_TIP_FOLLOW_SECS")
        .ok()
        .and_then(|s| s.trim().parse::<u64>().ok())
    {
        Some(0) => return None,
        Some(n) => n.max(30),
        None => DEFAULT_INTERVAL_SECS,
    };
    // Tip versions are staged beside the databases: pending queue items read
    // them, so they must survive a restart.
    let stage_root = wqm_common::paths::get_database_path()
        .ok()
        .and_then(|db| db.parent().map(|dir| dir.join("branch-tips")))
        .unwrap_or_else(|| std::env::temp_dir().join("wqm-branch-tips"));
    Some(tokio::spawn(async move {
        let queue_manager = QueueManager::new(pool.clone());
        let mut follower = BranchTipFollower::new(stage_root.clone());
        let mut interval = tokio::time::interval(tokio::time::Duration::from_secs(secs));
        info!(
            interval_secs = secs,
            stage = %stage_root.display(),
            "Branch tip follower started"
        );
        loop {
            interval.tick().await;
            let enqueued = follower
                .tick(&pool, &queue_manager, &allowed_extensions)
                .await;
            if enqueued > 0 {
                info!(
                    enqueued,
                    "Branch tip follower moved branches nobody has checked out"
                );
            }
        }
    }))
}
