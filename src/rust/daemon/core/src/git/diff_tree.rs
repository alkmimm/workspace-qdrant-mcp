use std::collections::HashSet;
use std::path::Path;

use super::watcher_types::GitWatcherError;

/// File change status from git diff-tree output
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FileChangeStatus {
    /// Modified file (M)
    Modified,
    /// Added file (A)
    Added,
    /// Deleted file (D)
    Deleted,
    /// Renamed file (R) with similarity percentage
    Renamed { old_path: String, similarity: u8 },
    /// Copied file (C) with similarity percentage
    Copied { src_path: String, similarity: u8 },
    /// Type changed (T) -- e.g., file became symlink
    TypeChanged,
}

/// A single file change from git diff-tree
#[derive(Debug, Clone)]
pub struct FileChange {
    /// Change status (modified, added, deleted, etc.)
    pub status: FileChangeStatus,
    /// Path relative to repo root
    pub path: String,
}

/// Run git diff-tree between two commits to get the list of changed files.
///
/// Uses the git2 crate for safety (no shell escaping issues).
/// Returns file changes relative to the repository root.
pub fn diff_tree(
    repo_root: &Path,
    old_sha: &str,
    new_sha: &str,
) -> Result<Vec<FileChange>, GitWatcherError> {
    let repo = git2::Repository::open(repo_root)
        .map_err(|e| GitWatcherError::NotGitRepo(format!("{}: {}", repo_root.display(), e)))?;

    let diff = build_diff(&repo, old_sha, new_sha)?;
    collect_changes(diff)
}

/// Repo-relative paths that DIFFER between the repo's current HEAD tree and
/// `branch`'s tip tree — the *committed* divergence of a worktree branch from
/// the main tree.
///
/// Used by the worktree-membership reconcile (B2): a shared file whose content
/// diverges on a worktree branch must be indexed FROM the worktree tree under
/// that branch, not inherited from the main tree (baseline). Only `Modified` /
/// `TypeChanged` deltas qualify — `Added` files are new-on-branch (handled by
/// the walk-minus-tracked path) and `Deleted` files are absent on the branch.
/// Rename/copy deltas are intentionally excluded: their new path is an addition
/// on the branch, covered by the new-on-branch path.
///
/// Returns an EMPTY set (never an error) when the repo, HEAD, or `branch` ref
/// can't be resolved, or HEAD == branch tip — the caller then routes nothing to
/// the worktree read, keeping the safe baseline inheritance. Paths are
/// forward-slash, matching `tracked_files.relative_path`. Uncommitted worktree
/// edits are NOT captured (commit-to-commit diff); the live watcher covers an
/// active worktree session.
pub fn modified_paths_head_vs_branch(repo_root: &Path, branch: &str) -> HashSet<String> {
    let repo = match git2::Repository::open(repo_root) {
        Ok(r) => r,
        Err(_) => return HashSet::new(),
    };
    let head_sha = match repo.head().ok().and_then(|h| h.target()) {
        Some(oid) => oid.to_string(),
        None => return HashSet::new(),
    };
    let branch_sha = match repo.refname_to_id(&format!("refs/heads/{branch}")) {
        Ok(oid) => oid.to_string(),
        Err(_) => return HashSet::new(),
    };
    if head_sha == branch_sha {
        return HashSet::new();
    }
    let diff = match build_diff(&repo, &head_sha, &branch_sha) {
        Ok(d) => d,
        Err(_) => return HashSet::new(),
    };
    match collect_changes(diff) {
        Ok(changes) => changes
            .into_iter()
            .filter(|c| {
                matches!(
                    c.status,
                    FileChangeStatus::Modified | FileChangeStatus::TypeChanged
                )
            })
            .map(|c| c.path)
            .collect(),
        Err(_) => HashSet::new(),
    }
}

fn build_diff<'a>(
    repo: &'a git2::Repository,
    old_sha: &str,
    new_sha: &str,
) -> Result<git2::Diff<'a>, GitWatcherError> {
    let io_err = |kind, msg: String| GitWatcherError::Io(std::io::Error::new(kind, msg));

    let old_oid = git2::Oid::from_str(old_sha).map_err(|e| {
        io_err(
            std::io::ErrorKind::InvalidData,
            format!("Invalid old SHA: {}", e),
        )
    })?;
    let new_oid = git2::Oid::from_str(new_sha).map_err(|e| {
        io_err(
            std::io::ErrorKind::InvalidData,
            format!("Invalid new SHA: {}", e),
        )
    })?;

    let is_initial = old_sha == "0000000000000000000000000000000000000000";
    let old_tree = if is_initial {
        None
    } else {
        let old_commit = repo.find_commit(old_oid).map_err(|e| {
            io_err(
                std::io::ErrorKind::NotFound,
                format!("Old commit not found: {}", e),
            )
        })?;
        Some(
            old_commit
                .tree()
                .map_err(|e| io_err(std::io::ErrorKind::Other, format!("Old tree error: {}", e)))?,
        )
    };

    let new_commit = repo.find_commit(new_oid).map_err(|e| {
        io_err(
            std::io::ErrorKind::NotFound,
            format!("New commit not found: {}", e),
        )
    })?;
    let new_tree = new_commit
        .tree()
        .map_err(|e| io_err(std::io::ErrorKind::Other, format!("New tree error: {}", e)))?;

    let mut diff_opts = git2::DiffOptions::new();
    diff_opts.include_untracked(false);

    let diff = repo
        .diff_tree_to_tree(old_tree.as_ref(), Some(&new_tree), Some(&mut diff_opts))
        .map_err(|e| io_err(std::io::ErrorKind::Other, format!("Diff error: {}", e)))?;

    let mut find_opts = git2::DiffFindOptions::new();
    find_opts.renames(true);
    find_opts.copies(true);
    let mut diff = diff;
    diff.find_similar(Some(&mut find_opts)).map_err(|e| {
        io_err(
            std::io::ErrorKind::Other,
            format!("Find similar error: {}", e),
        )
    })?;

    Ok(diff)
}

fn collect_changes(diff: git2::Diff<'_>) -> Result<Vec<FileChange>, GitWatcherError> {
    let mut changes = Vec::new();

    diff.foreach(
        &mut |delta, _| {
            let new_file = delta.new_file();
            let old_file = delta.old_file();
            let path = new_file
                .path()
                .or_else(|| old_file.path())
                .map(|p| p.to_string_lossy().to_string())
                .unwrap_or_default();

            let change_status = match delta.status() {
                git2::Delta::Added => FileChangeStatus::Added,
                git2::Delta::Deleted => {
                    let delete_path = old_file
                        .path()
                        .map(|p| p.to_string_lossy().to_string())
                        .unwrap_or_default();
                    changes.push(FileChange {
                        status: FileChangeStatus::Deleted,
                        path: delete_path,
                    });
                    return true;
                }
                git2::Delta::Modified => FileChangeStatus::Modified,
                git2::Delta::Renamed => FileChangeStatus::Renamed {
                    old_path: old_file
                        .path()
                        .map(|p| p.to_string_lossy().to_string())
                        .unwrap_or_default(),
                    similarity: 0,
                },
                git2::Delta::Copied => FileChangeStatus::Copied {
                    src_path: old_file
                        .path()
                        .map(|p| p.to_string_lossy().to_string())
                        .unwrap_or_default(),
                    similarity: 0,
                },
                git2::Delta::Typechange => FileChangeStatus::TypeChanged,
                _ => return true,
            };

            changes.push(FileChange {
                status: change_status,
                path,
            });
            true
        },
        None,
        None,
        None,
    )
    .map_err(|e| {
        GitWatcherError::Io(std::io::Error::new(
            std::io::ErrorKind::Other,
            format!("Diff foreach error: {}", e),
        ))
    })?;

    Ok(changes)
}

/// Parse git diff-tree --name-status output (fallback for when git2 is insufficient).
///
/// Format per line: `<status>\t<path>` or `<status>\t<old_path>\t<new_path>` for renames.
pub fn parse_diff_tree_output(output: &str) -> Vec<FileChange> {
    let mut changes = Vec::new();

    for line in output.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }

        let parts: Vec<&str> = line.split('\t').collect();
        if parts.len() < 2 {
            continue;
        }

        let status_str = parts[0];
        let path = parts[1].to_string();

        let status = if status_str == "M" {
            FileChangeStatus::Modified
        } else if status_str == "A" {
            FileChangeStatus::Added
        } else if status_str == "D" {
            FileChangeStatus::Deleted
        } else if status_str.starts_with('R') {
            let similarity = status_str[1..].parse::<u8>().unwrap_or(0);
            let new_path = parts.get(2).map(|s| s.to_string()).unwrap_or_default();
            changes.push(FileChange {
                status: FileChangeStatus::Renamed {
                    old_path: path,
                    similarity,
                },
                path: new_path,
            });
            continue;
        } else if status_str.starts_with('C') {
            let similarity = status_str[1..].parse::<u8>().unwrap_or(0);
            let new_path = parts.get(2).map(|s| s.to_string()).unwrap_or_default();
            changes.push(FileChange {
                status: FileChangeStatus::Copied {
                    src_path: path,
                    similarity,
                },
                path: new_path,
            });
            continue;
        } else if status_str == "T" {
            FileChangeStatus::TypeChanged
        } else {
            continue;
        };

        changes.push(FileChange { status, path });
    }

    changes
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_diff_tree_output_basic() {
        let output = "M\tsrc/main.rs\nA\tsrc/new_file.rs\nD\tsrc/deleted.rs\n";
        let changes = parse_diff_tree_output(output);
        assert_eq!(changes.len(), 3);
        assert_eq!(changes[0].status, FileChangeStatus::Modified);
        assert_eq!(changes[0].path, "src/main.rs");
        assert_eq!(changes[1].status, FileChangeStatus::Added);
        assert_eq!(changes[1].path, "src/new_file.rs");
        assert_eq!(changes[2].status, FileChangeStatus::Deleted);
        assert_eq!(changes[2].path, "src/deleted.rs");
    }

    #[test]
    fn test_parse_diff_tree_output_rename() {
        let output = "R100\told_name.rs\tnew_name.rs\n";
        let changes = parse_diff_tree_output(output);
        assert_eq!(changes.len(), 1);
        match &changes[0].status {
            FileChangeStatus::Renamed {
                old_path,
                similarity,
            } => {
                assert_eq!(old_path, "old_name.rs");
                assert_eq!(*similarity, 100);
                assert_eq!(changes[0].path, "new_name.rs");
            }
            other => panic!("Expected Renamed, got {:?}", other),
        }
    }

    #[test]
    fn test_parse_diff_tree_output_copy() {
        let output = "C095\toriginal.rs\tcopy.rs\n";
        let changes = parse_diff_tree_output(output);
        assert_eq!(changes.len(), 1);
        match &changes[0].status {
            FileChangeStatus::Copied {
                src_path,
                similarity,
            } => {
                assert_eq!(src_path, "original.rs");
                assert_eq!(*similarity, 95);
                assert_eq!(changes[0].path, "copy.rs");
            }
            other => panic!("Expected Copied, got {:?}", other),
        }
    }

    #[test]
    fn test_parse_diff_tree_output_type_change() {
        let output = "T\tsome_link\n";
        let changes = parse_diff_tree_output(output);
        assert_eq!(changes.len(), 1);
        assert_eq!(changes[0].status, FileChangeStatus::TypeChanged);
    }

    #[test]
    fn test_parse_diff_tree_output_empty() {
        let changes = parse_diff_tree_output("");
        assert!(changes.is_empty());

        let changes = parse_diff_tree_output("\n\n");
        assert!(changes.is_empty());
    }

    #[test]
    fn test_parse_diff_tree_output_unknown_status() {
        let output = "X\tunknown.rs\n";
        let changes = parse_diff_tree_output(output);
        assert!(changes.is_empty());
    }

    #[test]
    fn test_diff_tree_with_real_repo() {
        let temp_dir = tempfile::tempdir().unwrap();
        let repo = git2::Repository::init(temp_dir.path()).unwrap();

        let mut config = repo.config().unwrap();
        config.set_str("user.name", "Test").unwrap();
        config.set_str("user.email", "test@example.com").unwrap();

        let file1 = temp_dir.path().join("hello.txt");
        std::fs::write(&file1, "hello world").unwrap();
        let mut index = repo.index().unwrap();
        index.add_path(Path::new("hello.txt")).unwrap();
        index.write().unwrap();
        let tree_id = index.write_tree().unwrap();
        let tree = repo.find_tree(tree_id).unwrap();
        let sig = repo.signature().unwrap();
        let first_commit = repo
            .commit(Some("HEAD"), &sig, &sig, "first commit", &tree, &[])
            .unwrap();

        std::fs::write(&file1, "hello world modified").unwrap();
        let file2 = temp_dir.path().join("new.txt");
        std::fs::write(&file2, "new file content").unwrap();
        let mut index = repo.index().unwrap();
        index.add_path(Path::new("hello.txt")).unwrap();
        index.add_path(Path::new("new.txt")).unwrap();
        index.write().unwrap();
        let tree_id = index.write_tree().unwrap();
        let tree = repo.find_tree(tree_id).unwrap();
        let first = repo.find_commit(first_commit).unwrap();
        let second_commit = repo
            .commit(Some("HEAD"), &sig, &sig, "second commit", &tree, &[&first])
            .unwrap();

        let changes = diff_tree(
            temp_dir.path(),
            &first_commit.to_string(),
            &second_commit.to_string(),
        )
        .unwrap();

        assert_eq!(changes.len(), 2);

        let modified = changes.iter().find(|c| c.path == "hello.txt").unwrap();
        assert_eq!(modified.status, FileChangeStatus::Modified);

        let added = changes.iter().find(|c| c.path == "new.txt").unwrap();
        assert_eq!(added.status, FileChangeStatus::Added);
    }

    #[test]
    fn modified_paths_head_vs_branch_returns_only_modified() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = git2::Repository::init(tmp.path()).unwrap();
        let mut config = repo.config().unwrap();
        config.set_str("user.name", "Test").unwrap();
        config.set_str("user.email", "test@example.com").unwrap();
        let sig = repo.signature().unwrap();

        // c1 on HEAD (main): shared="v1", identical="same".
        std::fs::write(tmp.path().join("shared.txt"), "v1").unwrap();
        std::fs::write(tmp.path().join("identical.txt"), "same").unwrap();
        let mut index = repo.index().unwrap();
        index.add_path(Path::new("shared.txt")).unwrap();
        index.add_path(Path::new("identical.txt")).unwrap();
        index.write().unwrap();
        let tree1 = repo.find_tree(index.write_tree().unwrap()).unwrap();
        let c1 = repo
            .commit(Some("HEAD"), &sig, &sig, "c1", &tree1, &[])
            .unwrap();
        let commit1 = repo.find_commit(c1).unwrap();

        // c2 on refs/heads/feat (parent c1): shared diverges, identical unchanged.
        // HEAD stays on main, so head-vs-feat sees only shared.txt.
        std::fs::write(tmp.path().join("shared.txt"), "v2").unwrap();
        let mut index = repo.index().unwrap();
        index.add_path(Path::new("shared.txt")).unwrap();
        index.add_path(Path::new("identical.txt")).unwrap();
        index.write().unwrap();
        let tree2 = repo.find_tree(index.write_tree().unwrap()).unwrap();
        repo.commit(Some("refs/heads/feat"), &sig, &sig, "c2", &tree2, &[&commit1])
            .unwrap();

        let modified = modified_paths_head_vs_branch(tmp.path(), "feat");
        assert!(
            modified.contains("shared.txt"),
            "divergent file returned: {modified:?}"
        );
        assert!(
            !modified.contains("identical.txt"),
            "unchanged file excluded: {modified:?}"
        );
        assert_eq!(modified.len(), 1);

        // Unknown branch / clean repo → empty set, never an error.
        assert!(modified_paths_head_vs_branch(tmp.path(), "nonexistent").is_empty());
    }

    #[test]
    fn test_diff_tree_with_delete() {
        let temp_dir = tempfile::tempdir().unwrap();
        let repo = git2::Repository::init(temp_dir.path()).unwrap();

        let mut config = repo.config().unwrap();
        config.set_str("user.name", "Test").unwrap();
        config.set_str("user.email", "test@example.com").unwrap();

        std::fs::write(temp_dir.path().join("keep.txt"), "keep").unwrap();
        std::fs::write(temp_dir.path().join("delete_me.txt"), "will be deleted").unwrap();
        let mut index = repo.index().unwrap();
        index.add_path(Path::new("keep.txt")).unwrap();
        index.add_path(Path::new("delete_me.txt")).unwrap();
        index.write().unwrap();
        let tree_id = index.write_tree().unwrap();
        let tree = repo.find_tree(tree_id).unwrap();
        let sig = repo.signature().unwrap();
        let first = repo
            .commit(Some("HEAD"), &sig, &sig, "first", &tree, &[])
            .unwrap();

        let mut index = repo.index().unwrap();
        index.remove(Path::new("delete_me.txt"), 0).unwrap();
        index.write().unwrap();
        let tree_id = index.write_tree().unwrap();
        let tree = repo.find_tree(tree_id).unwrap();
        let first_c = repo.find_commit(first).unwrap();
        let second = repo
            .commit(Some("HEAD"), &sig, &sig, "second", &tree, &[&first_c])
            .unwrap();

        let changes = diff_tree(temp_dir.path(), &first.to_string(), &second.to_string()).unwrap();

        assert_eq!(changes.len(), 1);
        assert_eq!(changes[0].status, FileChangeStatus::Deleted);
        assert_eq!(changes[0].path, "delete_me.txt");
    }

    #[test]
    fn test_diff_tree_not_git_repo() {
        let temp_dir = tempfile::tempdir().unwrap();
        let result = diff_tree(temp_dir.path(), "abc", "def");
        assert!(result.is_err());
    }
}
