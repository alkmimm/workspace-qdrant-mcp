//! The background tasks that follow git rather than the file watcher.

use std::sync::Arc;

use sqlx::SqlitePool;
use tokio::task::JoinHandle;

use workspace_qdrant_core::AllowedExtensions;

use crate::{branch_tip_task, checkout_lag_task, worktree_discovery_task};

/// Start them all; the handles are kept for the daemon's lifetime.
pub fn start(
    pool: &SqlitePool,
    allowed_extensions: &Arc<AllowedExtensions>,
) -> Vec<JoinHandle<()>> {
    let mut tasks = vec![
        // New linked worktrees are reconciled as they appear, not at the next scan.
        worktree_discovery_task::start(pool.clone(), Arc::clone(allowed_extensions)),
        // A main checkout of the trunk that trails origin is measured.
        checkout_lag_task::start(pool.clone()),
    ];
    // Branches nobody has checked out are followed as their tips move.
    tasks.extend(branch_tip_task::start(
        pool.clone(),
        Arc::clone(allowed_extensions),
    ));
    tasks
}
