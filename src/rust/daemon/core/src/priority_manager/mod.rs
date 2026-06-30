//! Priority Manager Module
//!
//! Manages project activation state based on MCP server lifecycle events.
//! Priority ordering is computed at dequeue time by JOINing `watch_folders.is_active`
//! and collection type — the stored `priority` column in `unified_queue` is NOT used
//! for dequeue ordering (see `queue_operations::dequeue_unified`).
//!
//! Live sessions are tracked one-row-per-session in `project_sessions`
//! (migration v42), and `watch_folders.is_active` is the *projection* of that
//! table — `COUNT(*)` of live sessions per `(tenant, collection)`. This makes
//! every transition idempotent and impossible to leak (the previous free-running
//! `is_active += 1` counter leaked whenever a session ended without a clean
//! unregister, e.g. the MCP self-repo re-registering on every restart):
//! - `register_session`: upsert the caller's `session_id` row, re-project is_active
//! - `heartbeat`: refresh the caller's `last_heartbeat_at` (keeps the session live)
//! - `unregister_session`: delete the caller's row, re-project is_active
//! - `cleanup_orphaned_sessions`: reap rows with no heartbeat within the timeout,
//!   re-project is_active for affected projects
//! - `set_priority`: maps "high"/"normal" to is_active=1/0 (admin/test-only path)
//!
//! ## Schema Compliance (docs/specs/04-write-path.md)
//!
//! Activity state lives in `watch_folders.is_active` (projected) and
//! `project_sessions` (source of truth). Queue ordering is computed at dequeue
//! time, not stored.

mod manager;
mod session_monitor;

pub use manager::PriorityManager;
pub use session_monitor::SessionMonitor;

use chrono::{DateTime, Utc};
use thiserror::Error;

/// Priority levels for the queue system — re-exported from wqm_common
pub use wqm_common::constants::priority;

/// Session monitoring configuration
#[derive(Debug, Clone)]
pub struct SessionMonitorConfig {
    /// Heartbeat timeout in seconds (default: 60)
    pub heartbeat_timeout_secs: u64,
    /// Check interval in seconds (default: 30)
    pub check_interval_secs: u64,
}

impl Default for SessionMonitorConfig {
    fn default() -> Self {
        Self {
            heartbeat_timeout_secs: 60,
            check_interval_secs: 30,
        }
    }
}

/// Priority management errors
#[derive(Error, Debug)]
pub enum PriorityError {
    #[error("Database error: {0}")]
    Database(#[from] sqlx::Error),

    #[error("Invalid priority value: {0}")]
    InvalidPriority(i32),

    #[error("Empty tenant_id or branch")]
    EmptyParameter,

    #[error("Project not found: {0}")]
    ProjectNotFound(String),

    #[error("Session monitor already running")]
    MonitorAlreadyRunning,

    #[error("Session monitor not running")]
    MonitorNotRunning,
}

/// Result type for priority operations
pub type PriorityResult<T> = Result<T, PriorityError>;

impl From<crate::lifecycle::WatchFolderLifecycleError> for PriorityError {
    fn from(err: crate::lifecycle::WatchFolderLifecycleError) -> Self {
        match err {
            crate::lifecycle::WatchFolderLifecycleError::Database(e) => Self::Database(e),
            crate::lifecycle::WatchFolderLifecycleError::NotFound(msg) => {
                Self::ProjectNotFound(msg)
            }
        }
    }
}

/// Session information for tracking active MCP server connections
///
/// Uses `watch_folders.is_active` for activity state per spec.
#[derive(Debug, Clone)]
pub struct SessionInfo {
    /// Watch ID (tenant identifier)
    pub watch_id: String,
    /// Tenant ID (project_id for projects)
    pub tenant_id: String,
    /// Whether this project is currently active (has active sessions)
    pub is_active: bool,
    /// Last heartbeat timestamp
    pub last_activity_at: Option<DateTime<Utc>>,
    /// Current priority level (derived from is_active)
    pub priority: String,
}

/// Result of orphaned session cleanup
#[derive(Debug, Clone)]
pub struct OrphanedSessionCleanup {
    /// Number of projects with orphaned sessions detected
    pub projects_affected: usize,
    /// Total sessions cleaned up (same as projects_affected with boolean model)
    pub sessions_cleaned: i32,
    /// Tenant IDs that were demoted
    pub demoted_projects: Vec<String>,
}

#[cfg(test)]
mod tests;
