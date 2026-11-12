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

/// The folder whose checkout the lag is measured in: the main folder itself,
/// or a worktree row's main working tree. `None` for a worktree row whose main
/// tree is unknown: a linked worktree on the trunk is followed from origin
/// (`git::followed_trunk`), so its own checkout lagging is no stale index and
/// must not be reported as one.
fn lag_root(response: &GetProjectStatusResponse) -> Option<PathBuf> {
    if response.is_worktree {
        response.main_worktree_path.as_deref().map(PathBuf::from)
    } else {
        Some(PathBuf::from(&response.project_root))
    }
}

/// Fill the checkout-lag fields from the main folder's git. Absent when it
/// does not apply (see [`lag_root`] and `git::main_checkout_lag`).
async fn attach_checkout_lag(response: &mut GetProjectStatusResponse) {
    let Some(main_root) = lag_root(response) else {
        return;
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

#[cfg(test)]
mod tests {
    use super::*;

    fn row(root: &str, is_worktree: bool, main: Option<&str>) -> GetProjectStatusResponse {
        let mut r = not_found("t".into());
        r.found = true;
        r.project_root = root.into();
        r.is_worktree = is_worktree;
        r.main_worktree_path = main.map(str::to_string);
        r
    }

    #[test]
    fn the_lag_is_measured_in_the_main_folder() {
        assert_eq!(
            lag_root(&row("/repo", false, None)),
            Some(PathBuf::from("/repo"))
        );
        assert_eq!(
            lag_root(&row("/repo/.claude/worktrees/x", true, Some("/repo"))),
            Some(PathBuf::from("/repo")),
            "a worktree row measures its main working tree"
        );
        assert_eq!(
            lag_root(&row("/wt/x", true, None)),
            None,
            "never the worktree's own checkout"
        );
    }

    /// A main folder on `main` at c1 with `origin/main` `behind` commits ahead.
    fn repo_behind(behind: usize) -> (tempfile::TempDir, git2::Repository, git2::Oid) {
        let dir = tempfile::tempdir().unwrap();
        let repo = git2::Repository::init(dir.path()).unwrap();
        let sig = git2::Signature::now("t", "t@t").unwrap();
        let tree = repo
            .find_tree(repo.treebuilder(None).unwrap().write().unwrap())
            .unwrap();
        let c1 = repo.commit(None, &sig, &sig, "c1", &tree, &[]).unwrap();
        let mut tip = c1;
        for _ in 0..behind {
            let parent = repo.find_commit(tip).unwrap();
            tip = repo
                .commit(None, &sig, &sig, "c", &tree, &[&parent])
                .unwrap();
        }
        repo.reference("refs/heads/main", c1, true, "").unwrap();
        repo.reference("refs/remotes/origin/main", tip, true, "")
            .unwrap();
        repo.set_head("refs/heads/main").unwrap();
        (dir, repo, c1)
    }

    #[tokio::test]
    async fn a_lagging_main_checkout_fills_the_status_fields() {
        let (dir, _repo, _) = repo_behind(412);
        let mut status = row(&dir.path().to_string_lossy(), false, None);
        attach_checkout_lag(&mut status).await;
        assert_eq!(status.checkout_trunk.as_deref(), Some("main"));
        assert_eq!(status.checkout_behind_commits, Some(412));
        assert_eq!(status.checkout_ahead_commits, Some(0));

        // A worktree row of that repo reports the same main checkout.
        let mut wt = row("/elsewhere/wt", true, Some(&dir.path().to_string_lossy()));
        attach_checkout_lag(&mut wt).await;
        assert_eq!(wt.checkout_behind_commits, Some(412));
    }

    #[tokio::test]
    async fn no_lag_leaves_the_status_fields_unset() {
        let (dir, repo, c1) = repo_behind(3);
        // Main folder on another branch: the user's own work, not a stale trunk.
        repo.reference("refs/heads/feat", c1, true, "").unwrap();
        repo.set_head("refs/heads/feat").unwrap();
        let mut status = row(&dir.path().to_string_lossy(), false, None);
        attach_checkout_lag(&mut status).await;
        assert_eq!(
            (status.checkout_trunk, status.checkout_behind_commits),
            (None, None)
        );

        // A worktree row without its main tree, and a root that is no repo.
        for mut status in [
            row(&dir.path().to_string_lossy(), true, None),
            row("/no/such/repo", false, None),
        ] {
            attach_checkout_lag(&mut status).await;
            assert_eq!(status.checkout_behind_commits, None);
        }
    }
}
