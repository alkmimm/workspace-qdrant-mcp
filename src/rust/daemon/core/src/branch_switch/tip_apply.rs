//! Applying a branch tip's plan: stage and re-ingest what moved, untag what
//! left (see `branch_tips` for when and why).

use std::collections::HashSet;
use std::path::Path;

use sqlx::SqlitePool;
use tracing::{info, warn};
use wqm_common::constants::COLLECTION_PROJECTS;
use wqm_common::paths::RelativePath;

use crate::allowed_extensions::AllowedExtensions;
use crate::queue_operations::QueueManager;
use crate::startup::reconciliation::branch_prune::{
    build_prune_delete_payload, BRANCH_PRUNE_COVERED_DELETE_METADATA, BRANCH_PRUNE_DELETE_METADATA,
};
use crate::unified_queue_schema::{FilePayload, ItemType, QueueOperation};

use super::branch_tips::TipPlan;
use super::db;
use super::tip_stage::stage_file;
use super::worktree_eligibility::{main_eligibility_gate, worktree_path_eligible};

/// The branch a plan is applied to.
pub(super) struct Target<'a> {
    pub(super) watch_id: &'a str,
    pub(super) tenant_id: &'a str,
    pub(super) root: &'a str,
    pub(super) branch: &'a str,
    pub(super) tip: &'a str,
}

/// What applying a plan did.
pub(super) struct Applied {
    pub(super) enqueued: usize,
    /// Every re-ingest entered the queue. A pending item for the same
    /// `(branch, path)` from an OLDER tip makes the queue ignore the newer one
    /// (`INSERT OR IGNORE` on the per-file unique index) — the older bytes would
    /// land and the branch would rest on an intermediate version. Such a tip is
    /// left unsettled and planned again on the next pass, until the older item
    /// has run and the newer one can enter.
    pub(super) settled: bool,
}

/// Stage and enqueue one branch's plan.
pub(super) async fn apply_plan(
    pool: &SqlitePool,
    queue_manager: &QueueManager,
    allowed_extensions: &AllowedExtensions,
    t: &Target<'_>,
    dir: &Path,
    plan: TipPlan,
    live: &HashSet<String>,
) -> Applied {
    let gate = main_eligibility_gate(t.root);
    let (mut reingested, mut untagged, mut settled) = (0usize, 0usize, true);
    for (rel, bytes) in plan.reingest {
        // Every re-ingest passes the main scan's eligibility — a path the move
        // added must, a tagged one did when it was indexed. A library-format
        // document is followed like any file: the queue routes it to the
        // project's library (it was refused here until 2026-10-05).
        if !worktree_path_eligible(t.root, &gate, allowed_extensions, COLLECTION_PROJECTS, &rel) {
            continue;
        }
        if !stage_file(dir, &rel, &bytes) {
            warn!("branch tips: could not stage {} of '{}'", rel, t.branch);
            settled = false;
            continue;
        }
        match enqueue_reingest(queue_manager, t, &rel, dir).await {
            Ok(true) => reingested += 1,
            Ok(false) => settled = false,
            Err(e) => {
                warn!(
                    "branch tips: enqueue {} on '{}' failed: {}",
                    rel, t.branch, e
                );
                settled = false;
            }
        }
    }
    for rel in plan.untag {
        // An untag already pending is the same untag: nothing to retry.
        match enqueue_untag(pool, queue_manager, t, &rel, live).await {
            Ok(true) => untagged += 1,
            Ok(false) => {}
            Err(e) => {
                warn!("branch tips: untag {} on '{}' failed: {}", rel, t.branch, e);
                settled = false;
            }
        }
    }
    if reingested + untagged > 0 {
        info!(
            "branch tips: '{}' ({}) is at {}, followed from git; {} file(s) re-ingested, {} untagged",
            t.branch, t.tenant_id, t.tip, reingested, untagged
        );
    }
    Applied {
        enqueued: reingested + untagged,
        settled,
    }
}

/// `File/Add` of `rel` under the branch, reading the staged tip bytes. The tip
/// rides in the payload so the idempotency key differs per tip: an item still
/// pending for an older tip never swallows the newer one.
async fn enqueue_reingest(
    queue_manager: &QueueManager,
    t: &Target<'_>,
    rel: &str,
    dir: &Path,
) -> Result<bool, String> {
    let rel_path = RelativePath::from_user_input(rel).map_err(|e| e.to_string())?;
    let mut payload = serde_json::to_value(FilePayload {
        file_path: rel_path,
        file_type: None,
        file_hash: None,
        size_bytes: None,
        old_path: None,
    })
    .map_err(|e| e.to_string())?;
    payload["branch_tip"] = serde_json::Value::String(t.tip.to_string());
    // `worktree_membership` keeps the item's branch (no restamp to the main
    // HEAD) and scopes the idempotency key to the branch; `git_stage` keeps the
    // language server out of bytes it cannot see.
    let metadata = serde_json::json!({
        "worktree_membership": true,
        "read_root": dir.to_string_lossy(),
        "git_stage": true,
    })
    .to_string();
    queue_manager
        .enqueue_unified(
            ItemType::File,
            QueueOperation::Add,
            t.tenant_id,
            COLLECTION_PROJECTS,
            &payload.to_string(),
            Some(t.branch),
            Some(metadata.as_str()),
        )
        .await
        .map(|(_, new)| new)
        .map_err(|e| e.to_string())
}

/// Drop the branch's tag from `rel` — the branch prune's per-branch delete,
/// marked covered when another live generation keeps the path indexed.
async fn enqueue_untag(
    pool: &SqlitePool,
    queue_manager: &QueueManager,
    t: &Target<'_>,
    rel: &str,
    live: &HashSet<String>,
) -> Result<bool, String> {
    let rel_path = RelativePath::from_user_input(rel).map_err(|e| e.to_string())?;
    let payload_json = build_prune_delete_payload(&rel_path, t.branch)?;
    let covered = db::path_served_without_branch(pool, t.watch_id, rel, t.branch, live).await?;
    let metadata = if covered {
        BRANCH_PRUNE_COVERED_DELETE_METADATA
    } else {
        BRANCH_PRUNE_DELETE_METADATA
    };
    queue_manager
        .enqueue_unified(
            ItemType::File,
            QueueOperation::Delete,
            t.tenant_id,
            COLLECTION_PROJECTS,
            &payload_json,
            Some(t.branch),
            Some(metadata),
        )
        .await
        .map(|(_, new)| new)
        .map_err(|e| e.to_string())
}
