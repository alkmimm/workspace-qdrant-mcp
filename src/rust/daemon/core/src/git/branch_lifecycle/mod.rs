//! Branch lifecycle management for monitoring branch changes in repositories.
//!
//! ## Why there is no default-branch tracking here
//!
//! This module used to report the repository's "default branch" and emit a
//! `DefaultChanged` event. It was removed (2026-10-01) because it was wrong and
//! unused at the same time:
//!
//! * the detector read `.git/HEAD`, which names the CHECKED-OUT branch, so every
//!   `git checkout` of a feature branch was reported as a default-branch change;
//! * nothing consumed the answer — the `watch_folders.default_branch` column it
//!   was meant to feed was declared as a migration string that never executed
//!   (no deployed database had the column), and no `BranchEventHandler`
//!   implementation existed anywhere;
//! * the tests could not tell the difference, because their fixtures renamed the
//!   current branch, where "current" and "default" coincide.
//!
//! The trunk that actually matters — the one read surfaces widen to for files a
//! feature branch does not carry — is resolved where it is consumed: the MCP
//! server's `getBaseBranch` (`tracked-files-queries/tracked-files.ts`), which
//! takes git's default (`origin/HEAD`, then a local `main`/`master`) only
//! when the index holds files under that name. Do not reintroduce a second
//! resolver here without a consumer; two answers to one question drift apart.

mod detector;
#[cfg(test)]
mod tests;

pub use detector::BranchLifecycleDetector;

use serde::{Deserialize, Serialize};

use super::types::GitResult;

/// Branch lifecycle event types
///
/// These events are emitted when branches are created, deleted, or renamed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum BranchEvent {
    /// A new branch was created
    Created {
        /// Name of the created branch
        branch: String,
        /// Commit hash the branch points to
        commit_hash: Option<String>,
    },
    /// A branch was deleted
    Deleted {
        /// Name of the deleted branch
        branch: String,
    },
    /// A branch was renamed
    Renamed {
        /// Old branch name
        old_name: String,
        /// New branch name
        new_name: String,
    },
    /// Branch was switched to (HEAD changed)
    Switched {
        /// Previous branch
        from_branch: Option<String>,
        /// New branch
        to_branch: String,
    },
}

impl BranchEvent {
    /// Get the primary branch name involved in this event
    pub fn branch_name(&self) -> &str {
        match self {
            BranchEvent::Created { branch, .. } => branch,
            BranchEvent::Deleted { branch } => branch,
            BranchEvent::Renamed { new_name, .. } => new_name,
            BranchEvent::Switched { to_branch, .. } => to_branch,
        }
    }

    /// Check if this event affects a specific branch
    pub fn affects_branch(&self, branch: &str) -> bool {
        match self {
            BranchEvent::Created { branch: b, .. } => b == branch,
            BranchEvent::Deleted { branch: b } => b == branch,
            BranchEvent::Renamed { old_name, new_name } => old_name == branch || new_name == branch,
            BranchEvent::Switched {
                from_branch,
                to_branch,
            } => from_branch.as_deref() == Some(branch) || to_branch == branch,
        }
    }
}

/// Configuration for branch lifecycle detection
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BranchLifecycleConfig {
    /// Enable branch lifecycle tracking
    pub enabled: bool,
    /// Auto-delete branch documents when branch is deleted
    pub auto_delete_on_branch_delete: bool,
    /// Scan interval for detecting branch changes (seconds)
    pub scan_interval_seconds: u64,
    /// Rename correlation timeout (milliseconds)
    pub rename_correlation_timeout_ms: u64,
}

impl Default for BranchLifecycleConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            auto_delete_on_branch_delete: true,
            scan_interval_seconds: 5,
            rename_correlation_timeout_ms: 500,
        }
    }
}

/// Statistics about branch lifecycle tracking
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BranchLifecycleStats {
    /// Number of tracked branches
    pub tracked_branches: usize,
    /// Number of pending delete events (waiting for rename correlation)
    pub pending_deletes: usize,
}

/// Handler for branch lifecycle events that integrates with Qdrant
#[async_trait::async_trait]
pub trait BranchEventHandler: Send + Sync {
    /// Handle a branch creation event
    async fn handle_branch_created(
        &self,
        project_id: &str,
        branch: &str,
        commit_hash: Option<&str>,
    ) -> GitResult<()>;

    /// Handle a branch deletion event
    async fn handle_branch_deleted(&self, project_id: &str, branch: &str) -> GitResult<()>;

    /// Handle a branch rename event
    async fn handle_branch_renamed(
        &self,
        project_id: &str,
        old_branch: &str,
        new_branch: &str,
    ) -> GitResult<()>;
}
