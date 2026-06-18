//! Types for the branch switch protocol.

/// Result of a branch switch operation
#[derive(Debug, Clone, Default)]
pub struct BranchSwitchStats {
    /// Unchanged files enqueued for cross-branch dedup re-key (Add op; the
    /// dedup fast-path copies existing Qdrant points + FTS5 rows, no re-embed)
    pub enqueued_unchanged: u64,
    /// Files enqueued for re-ingestion (content changed)
    pub enqueued_changed: u64,
    /// Files enqueued for addition (new on target branch)
    pub enqueued_added: u64,
    /// Files enqueued for deletion (removed on target branch)
    pub enqueued_deleted: u64,
    /// Errors during processing
    pub errors: u64,
}
