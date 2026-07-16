//! Queue operations for branch switch: enqueueing file changes and tenant scans.

use std::path::Path;

use wqm_common::paths::{CanonicalPath, RelativePath};

use crate::git::{FileChange, FileChangeStatus};
use crate::queue_operations::QueueManager;
use crate::unified_queue_schema::{
    BranchMembershipBulk, FilePayload, ItemType, ProjectPayload, QueueOperation,
};
use crate::watching_queue::get_current_branch;

/// Paths per bulk branch-membership op. Kept SMALL on purpose: the processor
/// runs each op to completion on one concurrency slot, and the op does a
/// SEQUENTIAL Qdrant `set_payload` (`add_branch_to_base_point`) per base_point.
/// Under heavy load (e.g. a concurrent force-reembed hammering Qdrant) each
/// set_payload can take seconds, so a large chunk turns one op into a 30-80 min
/// monolith that monopolizes its slot and starves File ingestion — and a daemon
/// restart re-runs the whole op from scratch (the state.db write lands at the
/// end). 25 keeps each op bounded (completes, releases the slot, interleaves);
/// 1600 files becomes ~64 small ops, still vastly fewer than 1600 per-file Adds.
/// Also stays well under SQLite's bound-parameter limit for the `IN (...)`.
const BRANCH_BULK_CHUNK: usize = 25;

/// Enqueue a single changed file based on its diff-tree status.
/// Returns the operation type that was enqueued.
pub async fn enqueue_changed_file(
    queue_manager: &QueueManager,
    change: &FileChange,
    tenant_id: &str,
    collection: &str,
    project_root: &str,
    branch: &str,
) -> Result<QueueOperation, String> {
    let (op, old_rel_opt) = match &change.status {
        FileChangeStatus::Modified => (QueueOperation::Update, None),
        FileChangeStatus::Added => (QueueOperation::Add, None),
        FileChangeStatus::Deleted => (QueueOperation::Delete, None),
        FileChangeStatus::Renamed { old_path, .. } => {
            // Delete old path, add new path. Both paths come from git diff-tree
            // and are repository-relative — exactly what RelativePath needs.
            let old_rel = RelativePath::from_user_input(old_path)
                .map_err(|e| format!("invalid old_path from diff-tree {:?}: {}", old_path, e))?;
            enqueue_file_op_rel(
                queue_manager,
                tenant_id,
                collection,
                &old_rel,
                QueueOperation::Delete,
                branch,
            )
            .await?;
            (QueueOperation::Add, Some(old_rel))
        }
        FileChangeStatus::Copied { .. } => (QueueOperation::Add, None),
        FileChangeStatus::TypeChanged => (QueueOperation::Update, None),
    };

    // `change.path` is the diff-tree-reported path, repository-relative.
    let new_rel = RelativePath::from_user_input(&change.path).map_err(|e| {
        format!(
            "invalid change.path from diff-tree {:?}: {}",
            change.path, e
        )
    })?;
    enqueue_file_op_rel(
        queue_manager,
        tenant_id,
        collection,
        &new_rel,
        op.clone(),
        branch,
    )
    .await?;

    // For rename, report as Update for stats (it's logically an update, just with path change)
    if old_rel_opt.is_some() {
        return Ok(QueueOperation::Update);
    }

    // Project root is unused on this path now that we don't reconstruct
    // absolute paths; keep the parameter for caller signature stability
    // but acknowledge it's silently unused via `_`.
    let _ = project_root;
    Ok(op)
}

/// Enqueue an unchanged file (byte-identical to another branch) as an `Add` op
/// on the target branch.
///
/// The cross-branch dedup fast-path ([`branch_dedup`](crate::strategies::processing::file))
/// then re-keys the file's existing Qdrant points + FTS5 rows under the new
/// branch without re-embedding. `Add` (not `Update`) is deliberate: the Update
/// pre-flight issues a defensive `delete_points_by_filter(path, tenant)` for
/// paths untracked on the current branch, which is **not** branch-scoped and
/// would wipe the source branch's points before dedup can scroll them. `Add`
/// skips that defensive delete.
pub async fn enqueue_unchanged_file(
    queue_manager: &QueueManager,
    tenant_id: &str,
    collection: &str,
    relative_path: &str,
    branch: &str,
) -> Result<(), String> {
    let rel = RelativePath::from_user_input(relative_path)
        .map_err(|e| format!("invalid relative_path {:?}: {}", relative_path, e))?;
    enqueue_file_op_rel(
        queue_manager,
        tenant_id,
        collection,
        &rel,
        QueueOperation::Add,
        branch,
    )
    .await
}

/// Enqueue a file operation to the unified queue (absolute path entry point).
///
/// `abs_file_path` MUST live under a watch_folder root whose path matches
/// the on-disk prefix; the function derives the relative form for the
/// payload by stripping that root.
#[allow(dead_code)]
pub async fn enqueue_file_op(
    queue_manager: &QueueManager,
    tenant_id: &str,
    collection: &str,
    abs_file_path: &str,
    op: QueueOperation,
    branch: &str,
) -> Result<(), String> {
    // Look up the watch_folder root to anchor the path.
    let root = lookup_watch_folder_root(queue_manager, tenant_id, collection)
        .await?
        .ok_or_else(|| {
            format!(
                "No watch_folder found for tenant_id={}, collection={} -- cannot anchor file path",
                tenant_id, collection
            )
        })?;
    let abs = CanonicalPath::from_user_input(abs_file_path)
        .map_err(|e| format!("invalid absolute path {:?}: {}", abs_file_path, e))?;
    let rel = RelativePath::from_absolute_and_root(&abs, &root).map_err(|e| {
        format!(
            "file {} is not under watch_folder root {}: {}",
            abs_file_path,
            root.as_str(),
            e
        )
    })?;
    enqueue_file_op_rel(queue_manager, tenant_id, collection, &rel, op, branch).await
}

/// Internal: build the FilePayload from a pre-validated [`RelativePath`].
async fn enqueue_file_op_rel(
    queue_manager: &QueueManager,
    tenant_id: &str,
    collection: &str,
    rel: &RelativePath,
    op: QueueOperation,
    branch: &str,
) -> Result<(), String> {
    let file_payload = FilePayload {
        file_path: rel.clone(),
        file_type: None,
        file_hash: None,
        size_bytes: None,
        old_path: None,
    };

    let payload_json = serde_json::to_string(&file_payload)
        .map_err(|e| format!("Failed to serialize FilePayload: {}", e))?;

    queue_manager
        .enqueue_unified(
            ItemType::File,
            op,
            tenant_id,
            collection,
            &payload_json,
            Some(branch),
            None,
        )
        .await
        .map(|_| ())
        .map_err(|e| format!("Failed to enqueue: {}", e))
}

/// Lookup the watch_folder canonical root for (tenant_id, collection).
///
/// Returns `Ok(None)` if no watch_folder row exists. Returns `Err` if the
/// stored path fails canonical validation.
#[allow(dead_code)]
async fn lookup_watch_folder_root(
    queue_manager: &QueueManager,
    tenant_id: &str,
    collection: &str,
) -> Result<Option<CanonicalPath>, String> {
    let row: Option<String> = sqlx::query_scalar(
        "SELECT path FROM watch_folders WHERE tenant_id = ?1 AND collection = ?2 LIMIT 1",
    )
    .bind(tenant_id)
    .bind(collection)
    .fetch_optional(queue_manager.pool())
    .await
    .map_err(|e| format!("Failed to lookup watch_folder: {}", e))?;
    row.map(|p| {
        CanonicalPath::from_user_input(&p)
            .map_err(|e| format!("watch_folder.path is not canonical: {}", e))
    })
    .transpose()
}

/// Enqueue the unchanged-file branch re-key as a FEW bulk `(Tenant, Scan)` ops
/// (each carrying a verified path chunk) instead of one `Add` per file.
///
/// Each op carries a [`BranchMembershipBulk`] marker; the tenant processor
/// appends `branch` to all three stores for the listed paths WITHOUT
/// re-embedding (see `strategies::processing::tenant::project`). The caller must
/// pass only git-identical (no-diff) paths — that is what makes a per-path append
/// safe without re-hashing each file. Returns the total number of paths enqueued.
pub async fn enqueue_branch_membership_bulk(
    queue_manager: &QueueManager,
    tenant_id: &str,
    collection: &str,
    watch_folder_id: &str,
    project_root: &str,
    branch: &str,
    // The switch's OLD branch: scopes the processor's candidate set to the
    // generation the no-diff verification actually compared (see
    // `BranchMembershipBulk::from_branch`).
    from_branch: &str,
    paths: Vec<String>,
) -> Result<usize, String> {
    if paths.is_empty() {
        return Ok(0);
    }
    let total = paths.len();
    for chunk in paths.chunks(BRANCH_BULK_CHUNK) {
        let payload = ProjectPayload {
            project_root: project_root.to_string(),
            git_remote: None,
            project_type: None,
            old_tenant_id: None,
            is_active: None,
            branch_membership: Some(BranchMembershipBulk {
                watch_folder_id: watch_folder_id.to_string(),
                branch: branch.to_string(),
                paths: chunk.to_vec(),
                from_branch: Some(from_branch.to_string()),
            }),
        };
        let payload_json = serde_json::to_string(&payload)
            .map_err(|e| format!("serialize branch bulk payload: {}", e))?;
        queue_manager
            .enqueue_unified(
                ItemType::Tenant,
                QueueOperation::Scan,
                tenant_id,
                collection,
                &payload_json,
                Some(branch),
                None,
            )
            .await
            .map_err(|e| format!("enqueue branch bulk op: {}", e))?;
    }
    Ok(total)
}

/// Enqueue a full tenant scan (used for reset events).
pub async fn enqueue_tenant_scan(
    queue_manager: &QueueManager,
    tenant_id: &str,
    collection: &str,
    project_root: &str,
) -> Result<(), String> {
    let payload = serde_json::json!({
        "project_root": project_root,
        "recovery": false,
    })
    .to_string();

    let branch = get_current_branch(Path::new(project_root));

    queue_manager
        .enqueue_unified(
            ItemType::Tenant,
            QueueOperation::Scan,
            tenant_id,
            collection,
            &payload,
            Some(&branch),
            None,
        )
        .await
        .map(|_| ())
        .map_err(|e| format!("Failed to enqueue tenant scan: {}", e))
}
