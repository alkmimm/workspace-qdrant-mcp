//! What a File item's `metadata` asks of the processor: whose branch the tag
//! is, and where the bytes are.

use crate::unified_queue_schema::UnifiedQueueItem;

fn metadata(item: &UnifiedQueueItem) -> Option<serde_json::Value> {
    item.metadata
        .as_deref()
        .and_then(|m| serde_json::from_str::<serde_json::Value>(m).ok())
}

/// Whether a File item is a worktree-membership tag enqueued by
/// `branch_switch::reconcile_worktree_branches`. Such an item carries an
/// AUTHORITATIVE worktree branch: its bytes are read from the shared main tree
/// (the worktree's baseline), but the tag belongs to the worktree's branch, so
/// the process-time branch-restamp must NOT rewrite it to the main HEAD.
pub(super) fn is_worktree_membership(item: &UnifiedQueueItem) -> bool {
    metadata(item)
        .and_then(|v| v.get("worktree_membership").and_then(|b| b.as_bool()))
        .unwrap_or(false)
}

/// The worktree tree root a worktree-membership item must read its bytes from,
/// or `None` to read from the resolved (main) watch_folder.
///
/// Set only for "new-on-branch" items — files that exist solely on the worktree
/// branch, whose content has no copy under the main root. The processor anchors
/// its reads at this root while storage stays keyed to the main watch_folder;
/// the dequeue ignore-gate is still applied, but against the main-anchored path
/// so `global.wqmignore` filters the rel path without self-excluding the
/// worktree. See `branch_switch::worktree_membership`.
pub(super) fn worktree_read_root(item: &UnifiedQueueItem) -> Option<String> {
    metadata(item).and_then(|v| {
        v.get("read_root")
            .and_then(|r| r.as_str())
            .map(str::to_string)
    })
}

/// Whether the item's bytes are a version staged from git — a branch tip no
/// checkout holds (`branch_switch::branch_tips`). They are in no tree the
/// project's language server sees, so the graph pass resolves their calls
/// with tree-sitter only.
pub(super) fn reads_git_stage(item: &UnifiedQueueItem) -> bool {
    metadata(item)
        .and_then(|v| v.get("git_stage").and_then(|b| b.as_bool()))
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::unified_queue_schema::{ItemType, QueueOperation};
    use wqm_common::constants::COLLECTION_PROJECTS;

    fn item_with_metadata(metadata: Option<&str>) -> UnifiedQueueItem {
        UnifiedQueueItem {
            queue_id: "q".to_string(),
            idempotency_key: "k".to_string(),
            item_type: ItemType::File,
            op: QueueOperation::Add,
            tenant_id: "t".to_string(),
            collection: COLLECTION_PROJECTS.to_string(),
            status: crate::unified_queue_schema::QueueStatus::Pending,
            branch: "feat".to_string(),
            payload_json: r#"{"file_path":"src/only_feat.rs"}"#.to_string(),
            metadata: metadata.map(str::to_string),
            created_at: "2026-01-01T00:00:00Z".to_string(),
            updated_at: "2026-01-01T00:00:00Z".to_string(),
            lease_until: None,
            worker_id: None,
            retry_count: 0,
            error_message: None,
            last_error_at: None,
            file_path: None,
            qdrant_status: None,
            search_status: None,
            decision_json: None,
        }
    }

    #[test]
    fn worktree_read_root_and_membership_parse_metadata() {
        // Shared baseline: flagged, but no read_root → read from the main tree.
        let baseline = item_with_metadata(Some(r#"{"worktree_membership":true}"#));
        assert!(is_worktree_membership(&baseline));
        assert_eq!(worktree_read_root(&baseline), None);

        // New-on-branch: read_root present → read from the worktree tree.
        let new_on_branch = item_with_metadata(Some(
            r#"{"worktree_membership":true,"read_root":"/repo/.claude/worktrees/wt-feat"}"#,
        ));
        assert!(is_worktree_membership(&new_on_branch));
        assert_eq!(
            worktree_read_root(&new_on_branch).as_deref(),
            Some("/repo/.claude/worktrees/wt-feat")
        );

        // Ordinary items: neither flag.
        let plain = item_with_metadata(None);
        assert!(!is_worktree_membership(&plain));
        assert_eq!(worktree_read_root(&plain), None);
        let other = item_with_metadata(Some(r#"{"source_project_id":"abc"}"#));
        assert!(!is_worktree_membership(&other));
        assert_eq!(worktree_read_root(&other), None);
    }

    #[test]
    fn a_git_stage_item_says_so_and_nothing_else_does() {
        let staged = item_with_metadata(Some(
            r#"{"worktree_membership":true,"read_root":"/data/branch-tips/t/b/tip","git_stage":true}"#,
        ));
        assert!(reads_git_stage(&staged));
        assert!(is_worktree_membership(&staged), "keeps its branch");
        let worktree =
            item_with_metadata(Some(r#"{"worktree_membership":true,"read_root":"/wt"}"#));
        assert!(!reads_git_stage(&worktree));
        assert!(!reads_git_stage(&item_with_metadata(None)));
    }
}
