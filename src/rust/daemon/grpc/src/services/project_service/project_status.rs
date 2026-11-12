//! `GetProjectStatus`: the project's row, its indexing progress, and how far
//! its main checkout of the trunk trails `origin`.

use std::path::PathBuf;

use chrono::Utc;
use tonic::Status;
use tracing::{debug, error};

use crate::proto::{GetProjectStatusRequest, GetProjectStatusResponse};

use workspace_qdrant_core::git::main_checkout_lag;
use wqm_common::constants::COLLECTION_PROJECTS;

use super::indexing_progress::fetch_indexing_progress;
use super::queries::to_timestamp;
use super::ProjectServiceImpl;

impl ProjectServiceImpl {
    /// Execute the get_project_status business logic
    pub(crate) async fn handle_get_project_status(
        &self,
        req: GetProjectStatusRequest,
    ) -> Result<GetProjectStatusResponse, Status> {
        if req.project_id.is_empty() {
            return Err(Status::invalid_argument("project_id cannot be empty"));
        }

        debug!("Getting project status: {}", req.project_id);

        let query = r#"
            SELECT wf.tenant_id, wf.path, wf.is_active, wf.last_activity_at,
                   wf.created_at, wf.git_remote_url, wf.is_worktree,
                   wf.main_worktree_watch_id,
                   mw.path AS main_worktree_path
            FROM watch_folders wf
            LEFT JOIN watch_folders mw ON wf.main_worktree_watch_id = mw.watch_id
            WHERE wf.tenant_id = ?1 AND wf.collection = ?2
            LIMIT 1
        "#;

        let row = sqlx::query(query)
            .bind(&req.project_id)
            .bind(COLLECTION_PROJECTS)
            .fetch_optional(&self.db_pool)
            .await
            .map_err(|e| {
                error!("Database error fetching project: {e}");
                Status::internal(format!("Database error: {e}"))
            })?;

        let Some(row) = row else {
            return Ok(not_found(req.project_id));
        };
        let mut response = build_project_status_response(row)?;
        let progress = fetch_indexing_progress(&self.db_pool, &req.project_id).await;
        response.pending_count = progress.pending;
        response.in_progress_count = progress.in_progress;
        response.failed_count = progress.failed;
        response.done_count = progress.done;
        response.total_count = progress.total();
        response.percent_complete = progress.percent_complete();
        response.eta_seconds = progress.eta_seconds;
        response.oldest_pending_age_seconds = progress.oldest_pending_age_seconds;
        attach_checkout_lag(&mut response).await;
        Ok(response)
    }
}

/// Fill the checkout-lag fields from the main folder's git (the main working
/// tree's, when the row is a worktree's). Absent when it does not apply —
/// including a worktree row whose main tree is unknown: a linked worktree on
/// the trunk is followed from origin (`git::followed_trunk`), so its own
/// checkout lagging is no stale index and must not be reported as one.
async fn attach_checkout_lag(response: &mut GetProjectStatusResponse) {
    let main_root = if response.is_worktree {
        match response.main_worktree_path.as_deref() {
            Some(main) => PathBuf::from(main),
            None => return,
        }
    } else {
        PathBuf::from(&response.project_root)
    };
    let lag = tokio::task::spawn_blocking(move || main_checkout_lag(&main_root))
        .await
        .ok()
        .flatten();
    if let Some(lag) = lag {
        response.checkout_trunk = Some(lag.trunk);
        response.checkout_behind_commits = Some(lag.behind as i64);
        response.checkout_ahead_commits = Some(lag.ahead as i64);
    }
}

fn not_found(project_id: String) -> GetProjectStatusResponse {
    GetProjectStatusResponse {
        found: false,
        project_id,
        project_name: String::new(),
        project_root: String::new(),
        priority: String::new(),
        is_active: false,
        last_active: None,
        registered_at: None,
        git_remote: None,
        is_worktree: false,
        main_worktree_path: None,
        pending_count: 0,
        in_progress_count: 0,
        failed_count: 0,
        done_count: 0,
        total_count: 0,
        percent_complete: 0.0,
        eta_seconds: None,
        oldest_pending_age_seconds: None,
        checkout_trunk: None,
        checkout_behind_commits: None,
        checkout_ahead_commits: None,
    }
}

/// Build a status response from a database row. Indexing-progress and
/// checkout-lag fields are left empty here; the caller fills them.
fn build_project_status_response(
    row: sqlx::sqlite::SqliteRow,
) -> Result<GetProjectStatusResponse, Status> {
    use sqlx::Row;

    let last_active_str: Option<String> = row
        .try_get("last_activity_at")
        .map_err(|e| Status::internal(format!("Failed to get last_activity_at: {e}")))?;
    let last_active = last_active_str
        .and_then(|s| chrono::DateTime::parse_from_rfc3339(&s).ok())
        .map(|dt| to_timestamp(dt.with_timezone(&Utc)));

    let registered_at_str: Option<String> = row
        .try_get("created_at")
        .map_err(|e| Status::internal(format!("Failed to get created_at: {e}")))?;
    let registered_at = registered_at_str
        .and_then(|s| chrono::DateTime::parse_from_rfc3339(&s).ok())
        .map(|dt| to_timestamp(dt.with_timezone(&Utc)));

    let is_active_int: i32 = row
        .try_get("is_active")
        .map_err(|e| Status::internal(format!("Failed to get is_active: {e}")))?;

    let priority = if is_active_int > 0 { "high" } else { "normal" };

    let path: String = row
        .try_get("path")
        .map_err(|e| Status::internal(format!("Failed to get path: {e}")))?;
    let project_name = std::path::Path::new(&path)
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("")
        .to_string();

    let is_worktree_int: i32 = row.try_get("is_worktree").unwrap_or(0);
    let main_worktree_path: Option<String> = row.try_get("main_worktree_path").unwrap_or(None);

    let mut response = not_found(
        row.try_get("tenant_id")
            .map_err(|e| Status::internal(format!("Failed to get tenant_id: {e}")))?,
    );
    response.found = true;
    response.project_name = project_name;
    response.project_root = path;
    response.priority = priority.to_string();
    response.is_active = is_active_int > 0;
    response.last_active = last_active;
    response.registered_at = registered_at;
    response.git_remote = row
        .try_get("git_remote_url")
        .map_err(|e| Status::internal(format!("Failed to get git_remote_url: {e}")))?;
    response.is_worktree = is_worktree_int != 0;
    response.main_worktree_path = main_worktree_path;
    Ok(response)
}
