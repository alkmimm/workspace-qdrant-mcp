//! Where each branch of a repository is checked out right now.
//!
//! The daemon stores a linked worktree's content MAIN-anchored: its
//! `tracked_files` rows live under the main watch folder, tagged with the
//! worktree's branch (see `branch_switch::worktree_membership`). A file on
//! branch `X` is therefore only "missing from disk" when it is missing from
//! `X`'s OWN checkout — the main folder for the main HEAD, the linked worktree
//! for a worktree branch. Joining the main root for every branch reads a
//! worktree-only file as deleted: the idle reconcile did exactly that and
//! wiped ~92% of what it touched (2026-10-03).
//!
//! A branch with no checkout cannot be judged from disk at all; its tags are
//! branch pruning's business. Every on-disk staleness check that acts per
//! `(path, branch)` resolves the checkout here (CLAUDE.md reconciliation
//! invariant: "the branch comes from the walked tree").

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use once_cell::sync::Lazy;

use super::{list_linked_worktrees, read_current_branch, resolve_git_dir};
use crate::watching_queue::UNRESOLVED_BRANCH_LABEL;

/// How long [`BranchCheckouts::discover_cached`] reuses an answer.
const DISCOVER_TTL: Duration = Duration::from_secs(2);

type CheckoutCache = HashMap<PathBuf, (Instant, Arc<BranchCheckouts>)>;

static DISCOVER_CACHE: Lazy<Mutex<CheckoutCache>> = Lazy::new(|| Mutex::new(HashMap::new()));

/// Whether a file is on disk in the checkout of one branch.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CheckoutPresence {
    /// The branch is checked out and the file is there.
    Present,
    /// The branch is checked out and the file is not there.
    Missing,
    /// No checkout has this branch, so the disk says nothing about it.
    NoCheckout,
}

/// The checkout root of every branch currently checked out in a repository.
#[derive(Debug, Clone, Default)]
pub struct BranchCheckouts {
    roots: HashMap<String, PathBuf>,
    /// Set for a folder that is not a git repository: it has exactly one tree,
    /// which is the checkout of whatever branch label its rows carry.
    single_tree: Option<PathBuf>,
}

impl BranchCheckouts {
    /// Read the main HEAD and the linked worktrees of `main_root`.
    ///
    /// Filesystem-only (no `git` binary): `.git/HEAD` plus the
    /// `.git/worktrees/*` admin dirs. Worktree roots are folded to the daemon's
    /// native path and must be real leaf checkouts (a `.git` gitlink file), the
    /// same guards the worktree membership reconcile applies before it reads a
    /// worktree.
    ///
    /// A main HEAD that names no branch (detached, unborn) maps the label the
    /// writer stamps in that state — [`UNRESOLVED_BRANCH_LABEL`] — to the main
    /// folder, unless a worktree has a branch of that name checked out: that is
    /// where content written while detached lives, so it stays judgeable.
    pub fn discover(main_root: &Path) -> Self {
        let Some(git_dir) = resolve_git_dir(main_root) else {
            return Self {
                roots: HashMap::new(),
                single_tree: Some(main_root.to_path_buf()),
            };
        };
        let mut roots = HashMap::new();
        let head = read_current_branch(&git_dir);
        if let Some(branch) = &head {
            roots.insert(branch.clone(), main_root.to_path_buf());
        }
        for wt in list_linked_worktrees(main_root) {
            let Some(branch) = wt.branch else {
                continue;
            };
            let root = PathBuf::from(canonicalize_host_path(&wt.root.to_string_lossy()));
            if root == main_root || !root.is_dir() || !is_leaf_worktree_root(&root) {
                continue;
            }
            // git refuses to check one branch out twice; if a forced checkout
            // did it anyway, the main folder (inserted first) wins.
            roots.entry(branch).or_insert(root);
        }
        if head.is_none() {
            roots
                .entry(UNRESOLVED_BRANCH_LABEL.to_string())
                .or_insert_with(|| main_root.to_path_buf());
        }
        Self {
            roots,
            single_tree: None,
        }
    }

    /// [`Self::discover`], reused for [`DISCOVER_TTL`] per root. The delete
    /// path asks once per queue item — a branch-prune burst is thousands of
    /// items against the same repository — and a worktree added or switched in
    /// the last two seconds is not worth re-reading the admin dirs for.
    pub fn discover_cached(main_root: &Path) -> Arc<Self> {
        let now = Instant::now();
        if let Some((at, cached)) = DISCOVER_CACHE.lock().unwrap().get(main_root) {
            if now.duration_since(*at) < DISCOVER_TTL {
                return Arc::clone(cached);
            }
        }
        let fresh = Arc::new(Self::discover(main_root));
        DISCOVER_CACHE
            .lock()
            .unwrap()
            .insert(main_root.to_path_buf(), (now, Arc::clone(&fresh)));
        fresh
    }

    /// The checkout root of `branch`, if any checkout has it.
    pub fn root_for(&self, branch: &str) -> Option<&Path> {
        self.roots
            .get(branch)
            .map(PathBuf::as_path)
            .or(self.single_tree.as_deref())
    }

    /// Every checkout root (the main folder and each leaf worktree), deduplicated.
    pub fn all_roots(&self) -> Vec<&Path> {
        let mut roots: Vec<&Path> = self.roots.values().map(PathBuf::as_path).collect();
        roots.extend(self.single_tree.as_deref());
        roots.sort();
        roots.dedup();
        roots
    }

    /// Whether `relative_path` is on disk in `branch`'s checkout.
    pub fn presence(&self, branch: &str, relative_path: &str) -> CheckoutPresence {
        match self.root_for(branch) {
            None => CheckoutPresence::NoCheckout,
            Some(root) if root.join(relative_path).is_file() => CheckoutPresence::Present,
            Some(_) => CheckoutPresence::Missing,
        }
    }
}

/// The branch `main_root` has checked out: `Some` for a git repository (the
/// writer's [`UNRESOLVED_BRANCH_LABEL`] for a detached or unborn HEAD, the
/// same label its rows carry in that state), `None` for a folder that is not
/// a git repository.
pub fn head_branch(main_root: &Path) -> Option<String> {
    let git_dir = resolve_git_dir(main_root)?;
    Some(read_current_branch(&git_dir).unwrap_or_else(|| UNRESOLVED_BRANCH_LABEL.to_string()))
}

/// Whether `wt_root` is a genuine linked-worktree *checkout* rather than a
/// stale/malformed admin entry that resolved to a non-worktree directory.
///
/// A linked worktree checkout always carries a `.git` gitlink **file** whose
/// content is `gitdir: <main>/.git/worktrees/<name>`. Two non-worktree cases
/// this rejects:
/// - the `.claude/worktrees` *container* (an ancestor of every sub-worktree):
///   no `.git` entry at all — the 2026-08-06 phantom-walk root cause;
/// - a main repository root: `.git` is a **directory**, not a gitlink file.
///
/// Uses `is_file()` (not `exists()`) so a `.git` directory (a main repo) is
/// rejected too. This is the source-level guard against a bogus worktree root
/// being walked whole.
pub(crate) fn is_leaf_worktree_root(wt_root: &Path) -> bool {
    wt_root.join(".git").is_file()
}

/// Fold a host-reported path to the daemon's native POSIX view.
///
/// Mirrors the WSL-UNC arm of the TypeScript `canonicalizeHostPath`
/// (`src/typescript/mcp-server/src/clients/project-queries.ts`): a worktree
/// created from a Windows host records a `\\wsl.localhost\<distro>\home\…`
/// (or the legacy `\\wsl$\<distro>\…`) gitdir, but the daemon runs inside the
/// distro and reads the native `/home/…` path. Backslashes fold to `/`, the
/// `wsl.localhost`/`wsl$` share + distro segments are dropped, and duplicate /
/// trailing slashes are collapsed. Native POSIX paths pass through unchanged.
pub(crate) fn canonicalize_host_path(raw: &str) -> String {
    let slashed = raw.replace('\\', "/");
    let trimmed = slashed.trim_start_matches('/');
    let lower = trimmed.to_ascii_lowercase();

    let after_share = if lower.starts_with("wsl.localhost/") {
        Some(&trimmed["wsl.localhost/".len()..])
    } else if lower.starts_with("wsl$/") {
        Some(&trimmed["wsl$/".len()..])
    } else {
        None
    };

    let folded = match after_share {
        // Drop the `<distro>` segment right after the share host.
        Some(after) => match after.split_once('/') {
            Some((_distro, tail)) => format!("/{tail}"),
            None => "/".to_string(),
        },
        None => slashed.clone(),
    };

    collapse_slashes(&folded)
}

/// Collapse runs of `/` to a single separator and drop a trailing slash,
/// preserving the root `/`.
fn collapse_slashes(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut prev_slash = false;
    for c in s.chars() {
        if c == '/' {
            if !prev_slash {
                out.push('/');
            }
            prev_slash = true;
        } else {
            out.push(c);
            prev_slash = false;
        }
    }
    if out.len() > 1 && out.ends_with('/') {
        out.pop();
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    /// A main repo on `develop` with one linked worktree on `feat/x`.
    fn repo_with_worktree(temp: &TempDir) -> (PathBuf, PathBuf) {
        let main = temp.path().join("main");
        let git = main.join(".git");
        fs::create_dir_all(&git).unwrap();
        fs::write(git.join("HEAD"), "ref: refs/heads/develop\n").unwrap();

        let wt = main.join(".claude/worktrees/wt-x");
        fs::create_dir_all(&wt).unwrap();
        fs::write(wt.join(".git"), "gitdir: main/.git/worktrees/wt-x\n").unwrap();
        let admin = git.join("worktrees/wt-x");
        fs::create_dir_all(&admin).unwrap();
        fs::write(admin.join("gitdir"), format!("{}/.git\n", wt.display())).unwrap();
        fs::write(admin.join("HEAD"), "ref: refs/heads/feat/x\n").unwrap();
        (main, wt)
    }

    /// Regression (emnify, 2026-10-03): a file that exists only in the
    /// worktree is PRESENT for the worktree's branch even though the main
    /// folder does not have it. Judging it at the main root deleted it.
    #[test]
    fn worktree_only_file_is_present_for_its_branch_and_missing_on_main() {
        let temp = TempDir::new().unwrap();
        let (main, wt) = repo_with_worktree(&temp);
        fs::create_dir_all(wt.join("app/lib/actions")).unwrap();
        fs::write(wt.join("app/lib/actions/sync.ts"), "export {}\n").unwrap();

        let co = BranchCheckouts::discover(&main);
        assert_eq!(co.root_for("develop"), Some(main.as_path()));
        assert_eq!(co.root_for("feat/x"), Some(wt.as_path()));
        assert_eq!(
            co.presence("feat/x", "app/lib/actions/sync.ts"),
            CheckoutPresence::Present
        );
        assert_eq!(
            co.presence("develop", "app/lib/actions/sync.ts"),
            CheckoutPresence::Missing
        );
    }

    /// A file the worktree branch deleted is MISSING for that branch even
    /// though the main folder still has it — the opposite mistake.
    #[test]
    fn file_deleted_on_worktree_branch_is_missing_even_if_main_has_it() {
        let temp = TempDir::new().unwrap();
        let (main, _wt) = repo_with_worktree(&temp);
        fs::create_dir_all(main.join("app/lib/actions")).unwrap();
        fs::write(main.join("app/lib/actions/endpoint.ts"), "export {}\n").unwrap();

        let co = BranchCheckouts::discover(&main);
        assert_eq!(
            co.presence("develop", "app/lib/actions/endpoint.ts"),
            CheckoutPresence::Present
        );
        assert_eq!(
            co.presence("feat/x", "app/lib/actions/endpoint.ts"),
            CheckoutPresence::Missing
        );
    }

    /// A branch nobody has checked out cannot be judged from disk.
    #[test]
    fn branch_without_checkout_is_not_judged() {
        let temp = TempDir::new().unwrap();
        let (main, _wt) = repo_with_worktree(&temp);
        let co = BranchCheckouts::discover(&main);
        assert_eq!(co.root_for("old/branch"), None);
        assert_eq!(
            co.presence("old/branch", "anything.ts"),
            CheckoutPresence::NoCheckout
        );
    }

    /// A detached main HEAD names no branch, but the writer tags what it
    /// indexes then with UNRESOLVED_BRANCH_LABEL — that label maps to the main
    /// folder so those rows stay judgeable; worktrees still resolve.
    #[test]
    fn detached_main_head_maps_the_writers_fallback_label_to_main() {
        let temp = TempDir::new().unwrap();
        let (main, wt) = repo_with_worktree(&temp);
        fs::write(
            main.join(".git/HEAD"),
            "0123456789abcdef0123456789abcdef01234567\n",
        )
        .unwrap();
        let co = BranchCheckouts::discover(&main);
        assert_eq!(co.root_for("develop"), None);
        assert_eq!(co.root_for(UNRESOLVED_BRANCH_LABEL), Some(main.as_path()));
        assert_eq!(co.root_for("feat/x"), Some(wt.as_path()));
    }

    /// The cached discovery serves the same answer within its TTL.
    #[test]
    fn discover_cached_reuses_the_answer() {
        let temp = TempDir::new().unwrap();
        let (main, wt) = repo_with_worktree(&temp);
        let first = BranchCheckouts::discover_cached(&main);
        let second = BranchCheckouts::discover_cached(&main);
        assert!(Arc::ptr_eq(&first, &second));
        assert_eq!(second.root_for("feat/x"), Some(wt.as_path()));
    }

    /// A non-git folder (a library) has one tree, the checkout of every label.
    #[test]
    fn non_git_folder_is_the_checkout_of_every_branch_label() {
        let temp = TempDir::new().unwrap();
        let lib = temp.path().join("lib");
        fs::create_dir_all(&lib).unwrap();
        fs::write(lib.join("doc.md"), "x\n").unwrap();
        let co = BranchCheckouts::discover(&lib);
        assert_eq!(co.presence("main", "doc.md"), CheckoutPresence::Present);
        assert_eq!(co.presence("main", "gone.md"), CheckoutPresence::Missing);
    }

    /// A worktree admin entry pointing at something that is not a leaf
    /// checkout (no `.git` gitlink file) is ignored, never used as a root.
    #[test]
    fn non_leaf_worktree_entry_is_ignored() {
        let temp = TempDir::new().unwrap();
        let (main, wt) = repo_with_worktree(&temp);
        fs::remove_file(wt.join(".git")).unwrap();
        let co = BranchCheckouts::discover(&main);
        assert_eq!(co.root_for("feat/x"), None);
    }

    /// Regression (2026-08-06 phantom-walk): a real linked worktree has a `.git`
    /// gitlink FILE and is accepted; the `.claude/worktrees` container (no
    /// `.git`) and a main repo (`.git` DIRECTORY) are both rejected, so neither
    /// is ever walked whole.
    #[test]
    fn leaf_worktree_root_accepts_gitlink_rejects_container_and_main() {
        let temp = TempDir::new().unwrap();

        // A linked worktree checkout: `.git` is a gitlink file.
        let wt = temp.path().join(".claude/worktrees/wt-feat");
        fs::create_dir_all(&wt).unwrap();
        fs::write(wt.join(".git"), "gitdir: /main/.git/worktrees/wt-feat\n").unwrap();
        assert!(
            is_leaf_worktree_root(&wt),
            "a `.git` gitlink file marks a real leaf worktree"
        );

        // The container that holds the worktrees: no `.git` at all → rejected
        // (this is the exact root that produced the phantom walk).
        let container = temp.path().join(".claude/worktrees");
        assert!(
            !is_leaf_worktree_root(&container),
            "the worktrees container must never be treated as a worktree root"
        );

        // A main repository root: `.git` is a directory, not a gitlink → rejected.
        let main_repo = temp.path().join("main");
        fs::create_dir_all(main_repo.join(".git")).unwrap();
        assert!(
            !is_leaf_worktree_root(&main_repo),
            "a `.git` directory (main repo) is not a linked-worktree gitlink"
        );

        // A bare directory with nothing: rejected.
        let bare = temp.path().join("bare");
        fs::create_dir_all(&bare).unwrap();
        assert!(!is_leaf_worktree_root(Path::new(&bare)));
    }

    #[test]
    fn wsl_unc_folds_to_native_posix() {
        assert_eq!(
            canonicalize_host_path(
                "\\\\wsl.localhost\\ubuntu-24.04\\home\\me\\repo\\.claude\\worktrees\\wt"
            ),
            "/home/me/repo/.claude/worktrees/wt"
        );
        // Forward-slash form (git may already store it normalized).
        assert_eq!(
            canonicalize_host_path("//wsl.localhost/Ubuntu-24.04/home/me/repo"),
            "/home/me/repo"
        );
        // Legacy `wsl$` share, case-insensitive host.
        assert_eq!(
            canonicalize_host_path("\\\\WSL$\\ubuntu\\home\\x"),
            "/home/x"
        );
    }

    #[test]
    fn native_posix_passes_through() {
        assert_eq!(
            canonicalize_host_path("/home/me/repo/.claude/worktrees/wt"),
            "/home/me/repo/.claude/worktrees/wt"
        );
    }

    #[test]
    fn collapses_and_trims_slashes() {
        assert_eq!(collapse_slashes("/home//me///x/"), "/home/me/x");
        assert_eq!(collapse_slashes("/"), "/");
        assert_eq!(collapse_slashes("/home/x"), "/home/x");
    }
}
