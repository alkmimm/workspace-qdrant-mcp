//! PriorityManager: session lifecycle and priority management.

use chrono::{Duration as ChronoDuration, Utc};
use sqlx::{SqliteConnection, SqlitePool};
use std::collections::BTreeMap;
use std::time::Instant;
use tokio::sync::Mutex;
use tracing::{debug, info, warn};
use wqm_common::timestamps;

use crate::lifecycle::WatchFolderLifecycle;
use crate::metrics::METRICS;
use wqm_common::constants::COLLECTION_PROJECTS;

use super::{OrphanedSessionCleanup, PriorityError, PriorityResult, SessionInfo};

// The gRPC service and reaper have separate managers but publish to the same
// registry. Hold this through commit and publication to keep snapshots ordered.
static SESSION_TRANSITIONS: Mutex<()> = Mutex::const_new(());

/// Priority Manager for server lifecycle-driven priority adjustments
///
/// Uses only spec-compliant tables:
/// - `watch_folders` for activity tracking
/// - `unified_queue` for priority management
#[derive(Clone)]
pub struct PriorityManager {
    db_pool: SqlitePool,
}

impl PriorityManager {
    /// Create a new PriorityManager with existing database pool
    pub fn new(db_pool: SqlitePool) -> Self {
        Self { db_pool }
    }

    // =========================================================================
    // Session Tracking Methods (using watch_folders.is_active)
    // =========================================================================

    /// Register a live session for a project (migration v42 model).
    ///
    /// Upserts one row in `project_sessions` keyed by `(tenant_id,
    /// COLLECTION_PROJECTS, session_id)`, then projects the live-session count
    /// onto `watch_folders.is_active`. Idempotent: re-registering the same
    /// `session_id` (e.g. the MCP self-repo on every restart) refreshes the
    /// heartbeat instead of incrementing, so `is_active` can never leak.
    ///
    /// Returns the resulting live-session count.
    pub async fn register_session(&self, tenant_id: &str, session_id: &str) -> PriorityResult<i32> {
        if tenant_id.is_empty() {
            return Err(PriorityError::EmptyParameter);
        }
        let session_id: &str = if session_id.trim().is_empty() {
            "legacy"
        } else {
            session_id
        };

        let _transition = SESSION_TRANSITIONS.lock().await;
        let mut tx = crate::db_retry::begin_immediate(&self.db_pool).await?;
        Self::require_project(&mut tx, tenant_id).await?;

        let now = timestamps::format_utc(&Utc::now());
        let inserted = sqlx::query(
            r#"
            INSERT INTO project_sessions
                (tenant_id, collection, session_id, registered_at, last_heartbeat_at)
            VALUES (?1, ?2, ?3, ?4, ?4)
            ON CONFLICT(tenant_id, collection, session_id)
            DO NOTHING
            "#,
        )
        .bind(tenant_id)
        .bind(COLLECTION_PROJECTS)
        .bind(session_id)
        .bind(&now)
        .execute(&mut *tx)
        .await?
        .rows_affected()
            > 0;

        if !inserted {
            sqlx::query(
                "UPDATE project_sessions SET last_heartbeat_at = ?1 \
                 WHERE tenant_id = ?2 AND collection = ?3 AND session_id = ?4",
            )
            .bind(&now)
            .bind(tenant_id)
            .bind(COLLECTION_PROJECTS)
            .bind(session_id)
            .execute(&mut *tx)
            .await?;
        }

        let active = Self::project_session_count(&mut tx, tenant_id).await?;
        tx.commit().await?;

        if inserted {
            METRICS.session_created(tenant_id);
        }
        METRICS.set_active_session_count(tenant_id, i64::from(active));
        info!(
            "Session '{}' registered for project {}: {} live session(s)",
            session_id, tenant_id, active
        );
        Ok(active)
    }

    async fn require_project(conn: &mut SqliteConnection, tenant_id: &str) -> PriorityResult<()> {
        let exists: Option<i32> = sqlx::query_scalar(
            "SELECT 1 FROM watch_folders WHERE tenant_id = ?1 AND collection = ?2 LIMIT 1",
        )
        .bind(tenant_id)
        .bind(COLLECTION_PROJECTS)
        .fetch_optional(conn)
        .await?;
        if exists.is_none() {
            return Err(PriorityError::ProjectNotFound(tenant_id.to_string()));
        }
        Ok(())
    }

    async fn live_session_count(
        conn: &mut SqliteConnection,
        tenant_id: &str,
    ) -> PriorityResult<i32> {
        Ok(sqlx::query_scalar(
            "SELECT COUNT(*) FROM project_sessions WHERE tenant_id = ?1 AND collection = ?2",
        )
        .bind(tenant_id)
        .bind(COLLECTION_PROJECTS)
        .fetch_one(conn)
        .await?)
    }

    async fn project_session_count(
        conn: &mut SqliteConnection,
        tenant_id: &str,
    ) -> PriorityResult<i32> {
        WatchFolderLifecycle::sync_is_active_from_sessions_on(conn, tenant_id, COLLECTION_PROJECTS)
            .await?;
        Self::live_session_count(conn, tenant_id).await
    }

    /// Drop a live session for a project (migration v42 model).
    ///
    /// Deletes the `project_sessions` row for `session_id` (if any), then
    /// re-projects the live-session count onto `is_active`. Returns the
    /// remaining live-session count — the caller fires teardown side effects
    /// (LSP shutdown, watch refresh) only when it reaches 0.
    pub async fn unregister_session(
        &self,
        tenant_id: &str,
        session_id: &str,
    ) -> PriorityResult<i32> {
        if tenant_id.is_empty() {
            return Err(PriorityError::EmptyParameter);
        }
        let session_id: &str = if session_id.trim().is_empty() {
            "legacy"
        } else {
            session_id
        };

        let _transition = SESSION_TRANSITIONS.lock().await;
        let mut tx = crate::db_retry::begin_immediate(&self.db_pool).await?;
        Self::require_project(&mut tx, tenant_id).await?;

        let deleted = sqlx::query(
            "DELETE FROM project_sessions \
             WHERE tenant_id = ?1 AND collection = ?2 AND session_id = ?3",
        )
        .bind(tenant_id)
        .bind(COLLECTION_PROJECTS)
        .bind(session_id)
        .execute(&mut *tx)
        .await?
        .rows_affected()
            > 0;

        let remaining = Self::project_session_count(&mut tx, tenant_id).await?;
        tx.commit().await?;

        if deleted {
            METRICS.session_closed(tenant_id, 0.0);
        }
        METRICS.set_active_session_count(tenant_id, i64::from(remaining));
        info!(
            "Session '{}' unregistered for project {}: {} live session(s) remain",
            session_id, tenant_id, remaining
        );
        Ok(remaining)
    }

    /// Deactivate a single watch folder by `(tenant_id, path)`.
    ///
    /// Decrements `is_active` by 1 (clamped to 0) for only the watch folder
    /// at the specified path, leaving other entries for the same tenant
    /// untouched. Returns the `is_active` value after the decrement. Session
    /// rows and their tenant-wide metric are unaffected by this priority change.
    pub async fn unregister_session_by_path(
        &self,
        tenant_id: &str,
        path: &str,
    ) -> PriorityResult<i32> {
        if tenant_id.is_empty() || path.is_empty() {
            return Err(PriorityError::EmptyParameter);
        }

        let lifecycle = WatchFolderLifecycle::new(self.db_pool.clone());

        // Check existence before mutating
        let current = lifecycle
            .get_is_active_by_tenant_and_path(tenant_id, path)
            .await?;

        if current.is_none() {
            return Err(PriorityError::ProjectNotFound(format!(
                "{tenant_id} at path {path}"
            )));
        }

        lifecycle
            .deactivate_by_tenant_and_path(tenant_id, path)
            .await?;

        // Read back the updated value
        let updated = lifecycle
            .get_is_active_by_tenant_and_path(tenant_id, path)
            .await?
            .unwrap_or(0);

        info!(
            "Session unregistered for project {} at path {}: is_active={}",
            tenant_id, path, updated
        );

        Ok(updated)
    }

    /// Set project priority explicitly
    ///
    /// Maps a priority string ("high"/"normal") to a session count increment/decrement.
    /// "high" increments is_active, "normal" decrements it (floor 0).
    /// Queue ordering is computed at dequeue time based on is_active. Only
    /// session lifecycle operations publish the active-session metric.
    pub async fn set_priority(
        &self,
        tenant_id: &str,
        priority_str: &str,
    ) -> PriorityResult<(String, i32)> {
        if tenant_id.is_empty() {
            return Err(PriorityError::EmptyParameter);
        }

        // Validate priority string early
        if priority_str != "high" && priority_str != "normal" {
            return Err(PriorityError::InvalidPriority(
                priority_str.parse::<i32>().unwrap_or(-1),
            ));
        }

        // Get current state
        let current_active: Option<i32> = sqlx::query_scalar(
            "SELECT is_active FROM watch_folders WHERE tenant_id = ?1 AND collection = ?2 LIMIT 1",
        )
        .bind(tenant_id)
        .bind(COLLECTION_PROJECTS)
        .fetch_optional(&self.db_pool)
        .await?;

        let current_active = match current_active {
            Some(v) => v,
            None => {
                return Err(PriorityError::ProjectNotFound(tenant_id.to_string()));
            }
        };

        let previous_priority = if current_active > 0 { "high" } else { "normal" };

        // Delegate is_active mutation to WatchFolderLifecycle
        let lifecycle = WatchFolderLifecycle::new(self.db_pool.clone());
        if priority_str == "high" {
            lifecycle
                .activate_by_tenant(tenant_id, COLLECTION_PROJECTS)
                .await?;
        } else {
            lifecycle
                .deactivate_by_tenant(tenant_id, COLLECTION_PROJECTS)
                .await?;
        }

        info!(
            "Set priority for project {}: {} -> {}",
            tenant_id, previous_priority, priority_str
        );

        Ok((previous_priority.to_string(), 0))
    }

    /// Refresh a session's heartbeat (migration v42 model).
    ///
    /// Updates `last_heartbeat_at` on the caller's `project_sessions` row so the
    /// orphan reaper keeps it alive, and mirrors the activity timestamp onto
    /// `watch_folders` for status/ETA views. A heartbeat for a session that is
    /// not registered (or a project with no live sessions) updates 0 rows and
    /// returns `false` — it cannot resurrect a session, preserving the
    /// register/unregister lifecycle as the source of truth for `is_active`.
    pub async fn heartbeat(&self, tenant_id: &str, session_id: &str) -> PriorityResult<bool> {
        if tenant_id.is_empty() {
            return Err(PriorityError::EmptyParameter);
        }
        let session_id: &str = if session_id.trim().is_empty() {
            "legacy"
        } else {
            session_id
        };

        let start = Instant::now();
        let now = timestamps::format_utc(&Utc::now());

        let result = sqlx::query(
            r#"
            UPDATE project_sessions
            SET last_heartbeat_at = ?1
            WHERE tenant_id = ?2
              AND collection = ?3
              AND session_id = ?4
            "#,
        )
        .bind(&now)
        .bind(tenant_id)
        .bind(COLLECTION_PROJECTS)
        .bind(session_id)
        .execute(&self.db_pool)
        .await?;

        let updated = result.rows_affected() > 0;

        // Mirror the activity timestamp onto watch_folders for status/ETA views.
        if updated {
            sqlx::query(
                "UPDATE watch_folders SET last_activity_at = ?1, updated_at = ?1 \
                 WHERE tenant_id = ?2 AND collection = ?3",
            )
            .bind(&now)
            .bind(tenant_id)
            .bind(COLLECTION_PROJECTS)
            .execute(&self.db_pool)
            .await?;
        }

        let latency_secs = start.elapsed().as_secs_f64();
        if updated {
            METRICS.heartbeat_processed(tenant_id, latency_secs);
            debug!(
                "Heartbeat for session '{}' project {} (latency: {:.3}s)",
                session_id, tenant_id, latency_secs
            );
        } else {
            debug!(
                "Heartbeat ignored: no live session '{}' for project {}",
                session_id, tenant_id
            );
        }

        Ok(updated)
    }

    /// Get session info for a project
    pub async fn get_session_info(&self, tenant_id: &str) -> PriorityResult<Option<SessionInfo>> {
        let query = r#"
            SELECT watch_id, tenant_id, is_active, last_activity_at
            FROM watch_folders
            WHERE tenant_id = ?1
              AND collection = ?2
            LIMIT 1
        "#;

        let row = sqlx::query(query)
            .bind(tenant_id)
            .bind(COLLECTION_PROJECTS)
            .fetch_optional(&self.db_pool)
            .await?;

        if let Some(row) = row {
            use chrono::DateTime;
            use sqlx::Row;
            let is_active: i32 = row.try_get("is_active").unwrap_or(0);
            let last_activity_str: Option<String> = row.try_get("last_activity_at").ok();
            let last_activity_at = last_activity_str
                .and_then(|s| DateTime::parse_from_rfc3339(&s).ok())
                .map(|dt| dt.with_timezone(&Utc));

            let priority = if is_active != 0 { "high" } else { "normal" };

            Ok(Some(SessionInfo {
                watch_id: row.try_get("watch_id")?,
                tenant_id: row.try_get("tenant_id")?,
                is_active: is_active != 0,
                last_activity_at,
                priority: priority.to_string(),
            }))
        } else {
            Ok(None)
        }
    }

    /// Reap sessions whose heartbeat is older than `timeout_secs` (migration v42
    /// model) and re-project `is_active` for every affected project.
    ///
    /// These are sessions whose MCP server died without a clean unregister.
    /// Deleting the stale `project_sessions` rows and recomputing the count
    /// naturally demotes a project to idle once its last live session expires —
    /// while leaving projects that still have a live (heartbeating) session
    /// untouched.
    pub async fn cleanup_orphaned_sessions(
        &self,
        timeout_secs: u64,
    ) -> PriorityResult<OrphanedSessionCleanup> {
        let _transition = SESSION_TRANSITIONS.lock().await;
        let mut tx = crate::db_retry::begin_immediate(&self.db_pool).await?;
        let cutoff = Utc::now() - ChronoDuration::seconds(timeout_secs as i64);
        let cutoff_str = timestamps::format_utc(&cutoff);

        let deleted_tenants: Vec<String> = sqlx::query_scalar(
            "DELETE FROM project_sessions WHERE collection = ?1 AND last_heartbeat_at < ?2 \
             RETURNING tenant_id",
        )
        .bind(COLLECTION_PROJECTS)
        .bind(&cutoff_str)
        .fetch_all(&mut *tx)
        .await?;
        let sessions_cleaned = deleted_tenants.len() as i32;
        let mut deleted_by_tenant = BTreeMap::new();
        for tenant_id in deleted_tenants {
            *deleted_by_tenant.entry(tenant_id).or_insert(0) += 1;
        }

        let mut affected = Vec::with_capacity(deleted_by_tenant.len());
        for (tenant_id, removed) in deleted_by_tenant {
            let remaining = Self::project_session_count(&mut tx, &tenant_id).await?;
            affected.push((tenant_id, removed, remaining));
        }
        tx.commit().await?;

        let mut stale_tenants = Vec::with_capacity(affected.len());
        for (tenant_id, removed, remaining) in affected {
            for _ in 0..removed {
                METRICS.session_closed(&tenant_id, 0.0);
            }
            METRICS.set_active_session_count(&tenant_id, i64::from(remaining));
            stale_tenants.push(tenant_id);
        }

        let cleanup = OrphanedSessionCleanup {
            projects_affected: stale_tenants.len(),
            sessions_cleaned,
            demoted_projects: stale_tenants,
        };

        if cleanup.projects_affected > 0 {
            warn!(
                "Reaped {} orphaned session(s) across {} project(s): {:?}",
                cleanup.sessions_cleaned, cleanup.projects_affected, cleanup.demoted_projects
            );
        } else {
            debug!("No orphaned sessions found (timeout: {}s)", timeout_secs);
        }

        Ok(cleanup)
    }

    /// Get all projects with high priority (active sessions)
    pub async fn get_high_priority_projects(&self) -> PriorityResult<Vec<SessionInfo>> {
        let query = r#"
            SELECT watch_id, tenant_id, is_active, last_activity_at
            FROM watch_folders
            WHERE is_active > 0
              AND collection = ?1
            ORDER BY last_activity_at DESC
        "#;

        let rows = sqlx::query(query)
            .bind(COLLECTION_PROJECTS)
            .fetch_all(&self.db_pool)
            .await?;

        let mut projects = Vec::new();
        for row in rows {
            use chrono::DateTime;
            use sqlx::Row;
            let last_activity_str: Option<String> = row.try_get("last_activity_at").ok();
            let last_activity_at = last_activity_str
                .and_then(|s| DateTime::parse_from_rfc3339(&s).ok())
                .map(|dt| dt.with_timezone(&Utc));

            projects.push(SessionInfo {
                watch_id: row.try_get("watch_id")?,
                tenant_id: row.try_get("tenant_id")?,
                is_active: true,
                last_activity_at,
                priority: "high".to_string(),
            });
        }

        Ok(projects)
    }
}
