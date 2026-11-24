//! What a branch changed since it left the trunk the index follows.
//!
//! The worktree reconcile used to find a branch's own content only through
//! `modified_paths_head_vs_branch`: the paths git reports as `Modified` between
//! the MAIN FOLDER's HEAD and the branch tip. That HEAD is whatever the user has
//! checked out, not the base agents branch from. Measured 2026-11-24 on tecsul:
//! the main folder sat on a personal branch with no `plataforma/` tree at all,
//! so every agent edit there diffed as `Added`, never `Modified`, and was never
//! re-read. A worktree branch tagged with the trunk's bytes of a file when it
//! was created kept that tag after the agent edited the file, and a search on
//! the branch answered from the trunk's version (`ObjectApi.java`: branch tip
//! and disk 71270b…, index 693dbb…, the release's), with nothing pending.
//!
//! [`paths_changed_since_fork`] answers from the trunk instead, as every
//! fill-in diff does (`git::followed_trunk`): the diff from the fork point
//! (merge-base of the trunk's followed tip and the branch tip) to the branch
//! tip. That is the branch's own work, `Added` included, and nothing the trunk
//! did after the fork. Its size is the branch's, not the repository's.

use std::collections::HashSet;
use std::path::Path;

use super::tips::root_prefix;
use super::trunk_tip::followed_trunk_in;

/// Repo-relative paths (relative to `repo_root`, forward slashes) that
/// `branch` added, modified or renamed to since it forked from the trunk.
///
/// The fork is the merge-base of the trunk's followed tip and the branch tip;
/// when the main folder has the trunk checked out (`followed_trunk` is `None`)
/// its HEAD is the trunk. Deleted paths are left out: they are absent on the
/// branch. Empty, never an error, when git cannot say (not a repository, an
/// unknown branch, no common history) or the branch has no commits of its own.
pub fn paths_changed_since_fork(repo_root: &Path, branch: &str) -> HashSet<String> {
    changed_since_fork(repo_root, branch).unwrap_or_default()
}

fn changed_since_fork(repo_root: &Path, branch: &str) -> Option<HashSet<String>> {
    let repo = git2::Repository::open(repo_root).ok()?;
    let tip = repo.refname_to_id(&format!("refs/heads/{branch}")).ok()?;
    let base = match followed_trunk_in(&repo) {
        Some(trunk) => trunk.tip,
        None => repo.head().ok()?.target()?,
    };
    let fork = repo.merge_base(base, tip).ok()?;
    if fork == tip {
        return Some(HashSet::new());
    }
    let fork_tree = repo.find_commit(fork).ok()?.tree().ok()?;
    let tip_tree = repo.find_commit(tip).ok()?.tree().ok()?;
    let diff = repo
        .diff_tree_to_tree(Some(&fork_tree), Some(&tip_tree), None)
        .ok()?;
    let prefix = root_prefix(&repo, repo_root);
    let mut changed = HashSet::new();
    for delta in diff.deltas() {
        if delta.status() == git2::Delta::Deleted {
            continue;
        }
        let Some(path) = delta
            .new_file()
            .path()
            .map(|p| p.to_string_lossy().replace('\\', "/"))
        else {
            continue;
        };
        let relative = if prefix.is_empty() {
            Some(path.as_str())
        } else {
            path.strip_prefix(&format!("{prefix}/"))
        };
        if let Some(rel) = relative {
            changed.insert(rel.to_string());
        }
    }
    Some(changed)
}

#[cfg(test)]
#[path = "branch_changes_tests.rs"]
mod tests;
