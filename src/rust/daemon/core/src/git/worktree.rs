use std::path::{Path, PathBuf};
use tracing::warn;

/// Find the main working tree path for a git worktree.
///
/// Given a worktree's git directory (e.g., `/main/.git/worktrees/feature`),
/// reads the `commondir` file to find the main `.git` directory, then
/// returns its parent as the main working tree root.
///
/// Returns `None` if:
/// - The `commondir` file doesn't exist (not a worktree)
/// - The resolved path doesn't exist on disk
///
/// # Examples
///
/// ```no_run
/// use std::path::Path;
/// use workspace_qdrant_core::git::find_main_worktree_path;
///
/// // Given a worktree git dir like /repos/main/.git/worktrees/feature
/// let main_root = find_main_worktree_path(Path::new("/repos/main/.git/worktrees/feature"));
/// // Returns Some("/repos/main")
/// ```
pub fn find_main_worktree_path(worktree_git_dir: &Path) -> Option<PathBuf> {
    let commondir_file = worktree_git_dir.join("commondir");
    let content = match std::fs::read_to_string(&commondir_file) {
        Ok(c) => c,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return None,
        Err(e) => {
            warn!(
                "Failed to read commondir at {}: {}",
                commondir_file.display(),
                e
            );
            return None;
        }
    };
    let common_path = content.trim();

    // Resolve absolute or relative path
    let resolved = if Path::new(common_path).is_absolute() {
        PathBuf::from(common_path)
    } else {
        worktree_git_dir.join(common_path)
    };

    // CATEGORY-B: git-internal commondir resolution. Resolves "../.."  written by
    // git into .git/worktrees/<n>/commondir. Result is consumed locally to derive
    // the main worktree root; never persisted to SQLite or sent over gRPC.
    // See spec §16 §3.2.2.
    // CATEGORY-B: see above; result is process-local PathBuf only.
    let canonical = match resolved.canonicalize() {
        Ok(c) => c,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return None,
        Err(e) => {
            warn!(
                "Failed to canonicalize worktree common dir {}: {}",
                resolved.display(),
                e
            );
            return None;
        }
    };

    // The common dir points to the main .git directory;
    // its parent is the main working tree root
    canonical.parent().map(Path::to_path_buf)
}

/// A linked git worktree of a main repository.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LinkedWorktree {
    /// The worktree's working-tree root, exactly as git recorded it in
    /// `.git/worktrees/<name>/gitdir`. This may be a host/UNC form (e.g. a
    /// worktree created from a Windows host over `\\wsl.localhost\...`); the
    /// caller folds it to the daemon's native view before touching disk.
    pub root: PathBuf,
    /// The branch the worktree has checked out, or `None` for a detached HEAD.
    pub branch: Option<String>,
}

/// Enumerate the linked worktrees of a **main** repository.
///
/// Filesystem-only (no `git` binary), mirroring [`find_main_worktree_path`]:
/// reads `<git_dir>/worktrees/<name>/{gitdir,HEAD}` for each registered linked
/// worktree. `gitdir` points at the worktree's `.git` file, whose parent is the
/// worktree root; `HEAD` names the checked-out branch (`ref: refs/heads/<b>`),
/// or is a raw SHA for a detached HEAD (reported as `branch: None`).
///
/// Expects `main_repo_root` to be a real main working tree (its `.git` is a
/// directory). Returns an empty vec when the repo has no linked worktrees (no
/// `worktrees/` dir) or its git dir can't be resolved — callers treat that as
/// "nothing to do", never an error.
pub fn list_linked_worktrees(main_repo_root: &Path) -> Vec<LinkedWorktree> {
    // The `worktrees/` admin dir lives under the common git dir. For a main
    // repo that is simply `<root>/.git`; only when `.git` is a gitlink file
    // (the root is itself a linked worktree) do we resolve indirectly.
    let dot_git = main_repo_root.join(".git");
    let git_dir = if dot_git.is_dir() {
        dot_git
    } else {
        match super::resolve_git_dir(main_repo_root) {
            Some(d) => d,
            None => return Vec::new(),
        }
    };

    let entries = match std::fs::read_dir(git_dir.join("worktrees")) {
        Ok(e) => e,
        Err(_) => return Vec::new(),
    };

    let mut out = Vec::new();
    for entry in entries.flatten() {
        let admin = entry.path();
        if !admin.is_dir() {
            continue;
        }
        // `gitdir` holds an absolute path to the worktree's `.git` file; the
        // worktree root is that file's parent directory.
        let gitdir_raw = match std::fs::read_to_string(admin.join("gitdir")) {
            Ok(s) => s,
            Err(_) => continue,
        };
        let root = match Path::new(gitdir_raw.trim()).parent() {
            Some(p) => p.to_path_buf(),
            None => continue,
        };
        let branch = std::fs::read_to_string(admin.join("HEAD"))
            .ok()
            .and_then(|h| {
                h.trim()
                    .strip_prefix("ref: refs/heads/")
                    .map(str::to_string)
            });
        out.push(LinkedWorktree { root, branch });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    /// T-01: Verify correct path resolution from a simulated worktree structure
    /// with a relative commondir path.
    #[test]
    fn test_find_main_worktree_path_relative_commondir() {
        let temp = TempDir::new().unwrap();
        let main_repo = temp.path().join("main-repo");

        // Create main repo .git directory
        let main_git = main_repo.join(".git");
        fs::create_dir_all(&main_git).unwrap();

        // Create worktree git dir: main-repo/.git/worktrees/feature
        let worktree_git_dir = main_git.join("worktrees").join("feature");
        fs::create_dir_all(&worktree_git_dir).unwrap();

        // commondir typically contains a relative path like "../../"
        // which resolves from worktrees/feature back to .git
        fs::write(worktree_git_dir.join("commondir"), "../..\n").unwrap();

        let result = find_main_worktree_path(&worktree_git_dir);
        assert!(result.is_some(), "should resolve main worktree path");

        // CATEGORY-B: test mirrors production find_main_worktree_path which
        // retains canonicalize() as Category B (git-internal). See spec §16 §3.2.2.
        let expected = main_repo.canonicalize().unwrap();
        assert_eq!(result.unwrap(), expected);
    }

    /// Test with an absolute commondir path.
    #[test]
    fn test_find_main_worktree_path_absolute_commondir() {
        let temp = TempDir::new().unwrap();
        let main_repo = temp.path().join("main-repo");

        // Create main repo .git directory
        let main_git = main_repo.join(".git");
        fs::create_dir_all(&main_git).unwrap();

        // Create worktree git dir somewhere else
        let worktree_git_dir = temp.path().join("worktree-gitdir");
        fs::create_dir_all(&worktree_git_dir).unwrap();

        // Write absolute path to commondir
        // CATEGORY-B: test fixture; absolute path for commondir file used in test only.
        let abs_git_path = main_git.canonicalize().unwrap();
        fs::write(
            worktree_git_dir.join("commondir"),
            abs_git_path.to_str().unwrap(),
        )
        .unwrap();

        let result = find_main_worktree_path(&worktree_git_dir);
        assert!(result.is_some(), "should resolve from absolute commondir");

        // CATEGORY-B: test mirrors production find_main_worktree_path which
        // retains canonicalize() as Category B (git-internal). See spec §16 §3.2.2.
        let expected = main_repo.canonicalize().unwrap();
        assert_eq!(result.unwrap(), expected);
    }

    /// T-02: Non-worktree directory (no commondir file) returns None.
    #[test]
    fn test_find_main_worktree_path_no_commondir() {
        let temp = TempDir::new().unwrap();
        let result = find_main_worktree_path(temp.path());
        assert!(
            result.is_none(),
            "should return None when commondir is missing"
        );
    }

    /// Missing commondir file returns None.
    #[test]
    fn test_find_main_worktree_path_missing_commondir_file() {
        let temp = TempDir::new().unwrap();
        let fake_git_dir = temp.path().join("fake-git");
        fs::create_dir_all(&fake_git_dir).unwrap();

        let result = find_main_worktree_path(&fake_git_dir);
        assert!(result.is_none());
    }

    /// commondir points to a non-existent path returns None.
    #[test]
    fn test_find_main_worktree_path_nonexistent_resolved_path() {
        let temp = TempDir::new().unwrap();
        let worktree_dir = temp.path().join("wt");
        fs::create_dir_all(&worktree_dir).unwrap();

        fs::write(worktree_dir.join("commondir"), "/nonexistent/path/.git\n").unwrap();

        let result = find_main_worktree_path(&worktree_dir);
        assert!(result.is_none(), "should return None for non-existent path");
    }

    /// commondir with extra whitespace is handled correctly.
    #[test]
    fn test_find_main_worktree_path_whitespace_in_commondir() {
        let temp = TempDir::new().unwrap();
        let main_repo = temp.path().join("main-repo");

        let main_git = main_repo.join(".git");
        fs::create_dir_all(&main_git).unwrap();

        let worktree_git_dir = main_git.join("worktrees").join("feature");
        fs::create_dir_all(&worktree_git_dir).unwrap();

        // Extra whitespace and newlines
        fs::write(worktree_git_dir.join("commondir"), "  ../..\n  ").unwrap();

        let result = find_main_worktree_path(&worktree_git_dir);
        assert!(result.is_some(), "should handle whitespace in commondir");

        // CATEGORY-B: test mirrors production find_main_worktree_path which
        // retains canonicalize() as Category B (git-internal). See spec §16 §3.2.2.
        let expected = main_repo.canonicalize().unwrap();
        assert_eq!(result.unwrap(), expected);
    }

    /// Parse both a branch worktree and a detached-HEAD worktree from the
    /// `.git/worktrees/*` admin dirs.
    #[test]
    fn test_list_linked_worktrees_parses_branch_and_detached() {
        let temp = TempDir::new().unwrap();
        let main_repo = temp.path().join("main");
        let main_git = main_repo.join(".git");
        fs::create_dir_all(&main_git).unwrap();

        // A linked worktree checked out on a branch.
        let wt_feature = temp.path().join("wt-feature");
        fs::create_dir_all(&wt_feature).unwrap();
        let admin_a = main_git.join("worktrees").join("wt-feature");
        fs::create_dir_all(&admin_a).unwrap();
        fs::write(
            admin_a.join("gitdir"),
            format!("{}/.git\n", wt_feature.display()),
        )
        .unwrap();
        fs::write(admin_a.join("HEAD"), "ref: refs/heads/feature/x\n").unwrap();

        // A linked worktree in detached HEAD (HEAD is a raw SHA).
        let wt_detached = temp.path().join("wt-detached");
        fs::create_dir_all(&wt_detached).unwrap();
        let admin_b = main_git.join("worktrees").join("wt-detached");
        fs::create_dir_all(&admin_b).unwrap();
        fs::write(
            admin_b.join("gitdir"),
            format!("{}/.git", wt_detached.display()),
        )
        .unwrap();
        fs::write(
            admin_b.join("HEAD"),
            "0123456789abcdef0123456789abcdef01234567\n",
        )
        .unwrap();

        let got = list_linked_worktrees(&main_repo);
        assert_eq!(got.len(), 2, "both worktrees should be listed");

        let feature = got
            .iter()
            .find(|w| w.root == wt_feature)
            .expect("feature worktree present");
        assert_eq!(feature.branch.as_deref(), Some("feature/x"));

        let detached = got
            .iter()
            .find(|w| w.root == wt_detached)
            .expect("detached worktree present");
        assert_eq!(detached.branch, None, "detached HEAD has no branch");
    }

    /// A repo with no `worktrees/` admin dir yields an empty list, not an error.
    #[test]
    fn test_list_linked_worktrees_empty_without_worktrees_dir() {
        let temp = TempDir::new().unwrap();
        let main_repo = temp.path().join("main");
        fs::create_dir_all(main_repo.join(".git")).unwrap();
        assert!(list_linked_worktrees(&main_repo).is_empty());
    }
}
