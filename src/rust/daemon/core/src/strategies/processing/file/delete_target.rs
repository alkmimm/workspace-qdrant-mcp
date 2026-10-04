//! Which file on disk decides whether a `(path, branch)` delete is stale.
//!
//! The delete handler skips a delete whose file is back on disk (a stale
//! watcher event). The disk that answers must be the item BRANCH's checkout:
//! a linked worktree's content is stored main-anchored, so the main folder
//! neither proves a worktree file present nor absent. Probing the main folder
//! let every delete of a worktree-only file through (2026-10-03: the idle
//! reconcile's false deletes all landed) and would skip a real delete of a
//! file the worktree branch removed while the main folder still has it.

use std::path::{Path, PathBuf};

use crate::git::BranchCheckouts;

/// The watch root of a main-anchored pair (absolute == root + relative), or
/// `None` when the pair does not compose.
pub(super) fn main_root_of<'a>(abs_file_path: &'a str, relative_path: &str) -> Option<&'a str> {
    abs_file_path
        .strip_suffix(relative_path)
        .map(|r| r.trim_end_matches(['/', '\\']))
        .filter(|r| !r.is_empty())
}

/// The file a delete of `relative_path` on `branch` must find on disk to be
/// stale: the copy in the branch's own checkout when a checkout has the
/// branch, else the main-anchored path (a branch nobody has checked out keeps
/// the previous, conservative check).
pub(super) fn stale_probe_path(abs_file_path: &str, relative_path: &str, branch: &str) -> PathBuf {
    main_root_of(abs_file_path, relative_path)
        .and_then(|root| {
            BranchCheckouts::discover_cached(Path::new(root))
                .root_for(branch)
                .map(|checkout| checkout.join(relative_path))
        })
        .unwrap_or_else(|| PathBuf::from(abs_file_path))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    /// A main repo on `develop` with a linked worktree on `feat/x`.
    fn repo_with_worktree(temp: &TempDir) -> (PathBuf, PathBuf) {
        let main = temp.path().join("repo");
        let git = main.join(".git");
        fs::create_dir_all(&git).unwrap();
        fs::write(git.join("HEAD"), "ref: refs/heads/develop\n").unwrap();
        let wt = main.join(".claude/worktrees/wt-x");
        fs::create_dir_all(&wt).unwrap();
        fs::write(wt.join(".git"), "gitdir: repo/.git/worktrees/wt-x\n").unwrap();
        let admin = git.join("worktrees/wt-x");
        fs::create_dir_all(&admin).unwrap();
        fs::write(admin.join("gitdir"), format!("{}/.git\n", wt.display())).unwrap();
        fs::write(admin.join("HEAD"), "ref: refs/heads/feat/x\n").unwrap();
        (main, wt)
    }

    fn abs(root: &Path, rel: &str) -> String {
        root.join(rel).to_string_lossy().into_owned()
    }

    /// A delete for the worktree's branch is judged in the worktree: the
    /// worktree-only file is there, so the delete is stale (emnify `sync.ts`).
    #[test]
    fn worktree_branch_delete_probes_the_worktree() {
        let temp = TempDir::new().unwrap();
        let (main, wt) = repo_with_worktree(&temp);
        let rel = "app/lib/actions/sync.ts";
        assert_eq!(
            stale_probe_path(&abs(&main, rel), rel, "feat/x"),
            wt.join(rel)
        );
    }

    /// A delete for the main HEAD branch is judged in the main folder.
    #[test]
    fn main_branch_delete_probes_the_main_folder() {
        let temp = TempDir::new().unwrap();
        let (main, _wt) = repo_with_worktree(&temp);
        let rel = "src/a.ts";
        assert_eq!(
            stale_probe_path(&abs(&main, rel), rel, "develop"),
            main.join(rel)
        );
    }

    /// A branch nobody has checked out keeps the main-anchored probe.
    #[test]
    fn branch_without_checkout_keeps_the_main_anchored_probe() {
        let temp = TempDir::new().unwrap();
        let (main, _wt) = repo_with_worktree(&temp);
        let rel = "src/a.ts";
        let a = abs(&main, rel);
        assert_eq!(stale_probe_path(&a, rel, "old/merged"), PathBuf::from(&a));
    }

    /// A pair that does not compose (absolute != root + relative) is probed
    /// as given, never guessed at.
    #[test]
    fn non_composing_pair_is_probed_as_given() {
        assert_eq!(
            stale_probe_path("/x/other.ts", "src/a.ts", "develop"),
            PathBuf::from("/x/other.ts")
        );
        assert_eq!(main_root_of("/repo/src/a.ts", "src/a.ts"), Some("/repo"));
        assert_eq!(main_root_of("src/a.ts", "src/a.ts"), None);
    }
}
