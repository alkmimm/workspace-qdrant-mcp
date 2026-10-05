//! Branch tips: the repository's default branch, and the paths two branch tips
//! disagree on. Both feed the trunk fill-in that composes a feature branch's
//! view of the index (the daemon tags only the files a feature branch CHANGED;
//! everything else is read from the trunk's generation when unchanged).

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use once_cell::sync::Lazy;

/// The repository's default branch: `origin/HEAD`'s target when the remote
/// names one, else `main`, else `master` (the same order the MCP server's
/// `getDefaultBranch` uses, so both sides agree on the trunk).
pub fn default_branch(repo_root: &Path) -> Option<String> {
    let repo = git2::Repository::open(repo_root).ok()?;
    if let Ok(head) = repo.find_reference("refs/remotes/origin/HEAD") {
        if let Some(target) = head.symbolic_target() {
            if let Some(name) = target.strip_prefix("refs/remotes/origin/") {
                if !name.is_empty() {
                    return Some(name.to_string());
                }
            }
        }
    }
    ["main", "master"]
        .into_iter()
        .find(|b| repo.find_reference(&format!("refs/heads/{b}")).is_ok())
        .map(str::to_string)
}

type TipKey = (PathBuf, git2::Oid, git2::Oid);

/// Changed-path sets per (repository, tip, tip): a pair of commits never
/// changes, so the cache needs no expiry — only a bound.
static CHANGED_CACHE: Lazy<Mutex<HashMap<TipKey, Arc<HashSet<String>>>>> =
    Lazy::new(|| Mutex::new(HashMap::new()));
const CHANGED_CACHE_MAX: usize = 256;

/// The paths whose content differs between the tips of `base` and `head`,
/// relative to `repo_root` (which may be a sub-directory of the repository).
/// Renames count as a delete plus an add, so both paths are reported — a path
/// renamed away on `head` is gone there. Each branch resolves to its local ref,
/// else `origin/<name>`. `None` when git cannot say (not a repository, a branch
/// that does not resolve, an unreadable tree): the caller decides what an
/// unknown means.
pub fn paths_changed_between(
    repo_root: &Path,
    base: &str,
    head: &str,
) -> Option<Arc<HashSet<String>>> {
    let repo = git2::Repository::open(repo_root).ok()?;
    let base_oid = branch_tip(&repo, base)?;
    let head_oid = branch_tip(&repo, head)?;
    let key = (repo_root.to_path_buf(), base_oid, head_oid);
    if let Some(hit) = CHANGED_CACHE.lock().unwrap().get(&key) {
        return Some(Arc::clone(hit));
    }
    let prefix = root_prefix(&repo, repo_root);
    let base_tree = repo.find_commit(base_oid).ok()?.tree().ok()?;
    let head_tree = repo.find_commit(head_oid).ok()?.tree().ok()?;
    let diff = repo
        .diff_tree_to_tree(Some(&base_tree), Some(&head_tree), None)
        .ok()?;
    let mut changed = HashSet::new();
    for delta in diff.deltas() {
        for file in [delta.old_file(), delta.new_file()] {
            let Some(path) = file.path().map(|p| p.to_string_lossy().replace('\\', "/")) else {
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
    }
    let changed = Arc::new(changed);
    let mut cache = CHANGED_CACHE.lock().unwrap();
    if cache.len() >= CHANGED_CACHE_MAX {
        cache.clear();
    }
    cache.insert(key, Arc::clone(&changed));
    Some(changed)
}

/// The bytes of `relative_path` (relative to `repo_root`) at the tip of the
/// LOCAL branch `branch` — the version that branch holds, whether or not any
/// checkout has it. Local refs only, matching the branch prune's notion of a
/// live branch: a branch it treats as deleted is not read. `None` when git
/// cannot say: not a repository, no such local branch, no such path at that
/// tip, or a path that is not a file.
pub fn blob_at_branch_tip(repo_root: &Path, branch: &str, relative_path: &str) -> Option<Vec<u8>> {
    let repo = git2::Repository::open(repo_root).ok()?;
    let tip = repo.refname_to_id(&format!("refs/heads/{branch}")).ok()?;
    let tree = repo.find_commit(tip).ok()?.tree().ok()?;
    let prefix = root_prefix(&repo, repo_root);
    let path = if prefix.is_empty() {
        relative_path.to_string()
    } else {
        format!("{prefix}/{relative_path}")
    };
    let entry = tree.get_path(Path::new(&path)).ok()?;
    let blob = entry.to_object(&repo).ok()?.into_blob().ok()?;
    Some(blob.content().to_vec())
}

/// Where `repo_root` sits inside its repository's working tree, `/`-separated
/// ("" at the top): tree paths are repository-relative, the index's are
/// root-relative.
pub(crate) fn root_prefix(repo: &git2::Repository, repo_root: &Path) -> String {
    let prefix = repo
        .workdir()
        .and_then(|w| {
            // CATEGORY-B: process-local only — both sides resolved the same way
            // to compute the root's prefix inside the repository; never stored.
            let w = std::fs::canonicalize(w).ok()?;
            let r = std::fs::canonicalize(repo_root).ok()?;
            r.strip_prefix(&w).ok().map(Path::to_path_buf)
        })
        .unwrap_or_default();
    prefix.to_string_lossy().replace('\\', "/")
}

fn branch_tip(repo: &git2::Repository, branch: &str) -> Option<git2::Oid> {
    [
        format!("refs/heads/{branch}"),
        format!("refs/remotes/origin/{branch}"),
    ]
    .iter()
    .find_map(|r| repo.refname_to_id(r).ok())
    .and_then(|oid| repo.find_commit(oid).ok().map(|c| c.id()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn commit(
        repo: &git2::Repository,
        files: &[(&str, &str)],
        parent: Option<git2::Oid>,
    ) -> git2::Oid {
        let mut builder = repo.treebuilder(None).unwrap();
        let mut dirs: HashMap<&str, Vec<(&str, &str)>> = HashMap::new();
        for (path, content) in files {
            match path.split_once('/') {
                Some((dir, name)) => dirs.entry(dir).or_default().push((name, content)),
                None => {
                    let blob = repo.blob(content.as_bytes()).unwrap();
                    builder.insert(path, blob, 0o100644).unwrap();
                }
            }
        }
        for (dir, entries) in dirs {
            let mut sub = repo.treebuilder(None).unwrap();
            for (name, content) in entries {
                let blob = repo.blob(content.as_bytes()).unwrap();
                sub.insert(name, blob, 0o100644).unwrap();
            }
            builder.insert(dir, sub.write().unwrap(), 0o040000).unwrap();
        }
        let tree = repo.find_tree(builder.write().unwrap()).unwrap();
        let sig = git2::Signature::now("t", "t@t").unwrap();
        let parents: Vec<git2::Commit> = parent
            .map(|p| repo.find_commit(p).unwrap())
            .into_iter()
            .collect();
        let parent_refs: Vec<&git2::Commit> = parents.iter().collect();
        repo.commit(None, &sig, &sig, "c", &tree, &parent_refs)
            .unwrap()
    }

    /// develop: a.ts, b.ts, old.ts. feature: a.ts changed, old.ts deleted,
    /// new.ts added, b.ts untouched.
    fn two_branch_repo() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        let repo = git2::Repository::init(dir.path()).unwrap();
        let dev = commit(
            &repo,
            &[("a.ts", "1"), ("b.ts", "1"), ("src/old.ts", "1")],
            None,
        );
        let feat = commit(
            &repo,
            &[("a.ts", "2"), ("b.ts", "1"), ("src/new.ts", "1")],
            Some(dev),
        );
        repo.reference("refs/heads/develop", dev, true, "").unwrap();
        repo.reference("refs/heads/feature", feat, true, "")
            .unwrap();
        dir
    }

    #[test]
    fn changed_paths_cover_edits_deletes_and_adds_but_not_untouched_files() {
        let dir = two_branch_repo();
        let changed = paths_changed_between(dir.path(), "develop", "feature").unwrap();
        let expected: HashSet<String> = ["a.ts", "src/old.ts", "src/new.ts"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        assert_eq!(*changed, expected);
        assert!(paths_changed_between(dir.path(), "develop", "develop")
            .unwrap()
            .is_empty());
    }

    #[test]
    fn an_unknown_branch_or_a_non_repository_is_unknown_not_empty() {
        let dir = two_branch_repo();
        assert!(paths_changed_between(dir.path(), "develop", "no-such").is_none());
        let plain = tempfile::tempdir().unwrap();
        assert!(paths_changed_between(plain.path(), "a", "b").is_none());
    }

    #[test]
    fn a_branch_tip_yields_its_own_version_of_a_file() {
        let dir = two_branch_repo();
        let at = |branch, path| blob_at_branch_tip(dir.path(), branch, path);
        assert_eq!(at("develop", "a.ts").as_deref(), Some(&b"1"[..]));
        assert_eq!(at("feature", "a.ts").as_deref(), Some(&b"2"[..]));
        assert_eq!(at("feature", "src/new.ts").as_deref(), Some(&b"1"[..]));
        assert!(at("develop", "src/new.ts").is_none(), "not at that tip");
        assert!(at("feature", "src").is_none(), "a directory is not a file");
        assert!(at("no-such", "a.ts").is_none());
    }

    #[test]
    fn default_branch_prefers_origin_head_then_main_then_master() {
        let dir = two_branch_repo();
        let repo = git2::Repository::open(dir.path()).unwrap();
        assert_eq!(default_branch(dir.path()), None, "neither main nor master");
        let dev = repo.refname_to_id("refs/heads/develop").unwrap();
        repo.reference("refs/heads/master", dev, true, "").unwrap();
        assert_eq!(default_branch(dir.path()).as_deref(), Some("master"));
        repo.reference("refs/heads/main", dev, true, "").unwrap();
        assert_eq!(default_branch(dir.path()).as_deref(), Some("main"));
        repo.reference("refs/remotes/origin/develop", dev, true, "")
            .unwrap();
        repo.reference_symbolic(
            "refs/remotes/origin/HEAD",
            "refs/remotes/origin/develop",
            true,
            "",
        )
        .unwrap();
        assert_eq!(default_branch(dir.path()).as_deref(), Some("develop"));
    }
}
