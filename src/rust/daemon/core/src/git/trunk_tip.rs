//! The trunk is followed from git, not from a checkout of it.
//!
//! A repository's trunk ([`super::default_branch`]) is what every branch view
//! fills unchanged files in from. Unless the main folder has it checked out —
//! the watcher follows that disk — its index moved only when someone updated
//! the local branch: a trunk nobody had checked out when its local ref was
//! updated, one checked out in a linked worktree when that worktree was pulled.
//! Agents branch off `origin/<trunk>`, so the local copy fell behind the code
//! every branch was built on. Measured 2026-11-11: tecsul's `release/platform`
//! worktree sat 1,406 commits (2,329 files) behind its origin for 13 days,
//! bws-engineer `master` 52 commits, backend-api `main` 27.
//!
//! [`followed_trunk`] names the tip the index follows instead: the local trunk,
//! or `origin/<trunk>` when that is the local trunk fast-forwarded — what
//! `git pull --ff-only` would check out. A local trunk with commits of its own
//! (ahead or diverged) is followed as it is: local work is never overridden.
//! The daemon never fetches; `origin/<trunk>` is as fresh as the last fetch
//! anyone ran in the repository.
//!
//! One answer serves every consumer, so they agree on what the trunk holds:
//! the branch tip follower indexes it, worktree discovery leaves a linked
//! worktree on it alone, `BranchCheckouts` judges none of its paths against
//! that worktree's disk, and the fill-in diffs compare branches against it
//! (the MCP server's `git-branch-diff.ts` mirrors the rule).

use std::path::Path;

use super::tips::default_branch_in;

/// The trunk and the tip the index follows it at.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FollowedTrunk {
    pub name: String,
    /// The tip the index holds the trunk at.
    pub tip: git2::Oid,
    /// The local trunk's own tip: behind `tip` when origin fast-forwards it.
    pub local: git2::Oid,
}

impl FollowedTrunk {
    /// Whether the tip is origin's, ahead of the local trunk.
    pub fn from_origin(&self) -> bool {
        self.tip != self.local
    }
}

/// The trunk of `repo_root` when the index follows it from git, with its tip.
///
/// `None` when it is not followed from git: not a repository, no trunk, the
/// main folder's own HEAD (its disk is the truth there, and the watcher
/// follows it), or no local trunk (a branch git has only on the remote is the
/// branch prune's business, never tagged by this).
pub fn followed_trunk(repo_root: &Path) -> Option<FollowedTrunk> {
    followed_trunk_in(&git2::Repository::open(repo_root).ok()?)
}

/// [`followed_trunk`] of a repository already open (at its main folder).
pub(crate) fn followed_trunk_in(repo: &git2::Repository) -> Option<FollowedTrunk> {
    let name = default_branch_in(repo)?;
    let local_ref = format!("refs/heads/{name}");
    let head = repo.find_reference("HEAD").ok();
    if head.as_ref().and_then(|h| h.symbolic_target()) == Some(local_ref.as_str()) {
        return None;
    }
    let local = commit_of(repo, &local_ref)?;
    let tip = match commit_of(repo, &format!("refs/remotes/origin/{name}")) {
        Some(origin)
            if origin == local || repo.graph_descendant_of(origin, local).unwrap_or(false) =>
        {
            origin
        }
        _ => local,
    };
    Some(FollowedTrunk { name, tip, local })
}

/// Whether `branch` is the trunk of `repo_root` and followed from git.
pub fn is_followed_trunk(repo_root: &Path, branch: &str) -> bool {
    followed_trunk(repo_root).is_some_and(|t| t.name == branch)
}

fn commit_of(repo: &git2::Repository, refname: &str) -> Option<git2::Oid> {
    let oid = repo.refname_to_id(refname).ok()?;
    repo.find_commit(oid).ok().map(|c| c.id())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::git::{
        blob_at_branch_tip, paths_changed_between, BranchCheckouts, CheckoutPresence,
    };

    fn commit(
        repo: &git2::Repository,
        files: &[(&str, &str)],
        parent: Option<git2::Oid>,
    ) -> git2::Oid {
        let mut builder = repo.treebuilder(None).unwrap();
        for (path, content) in files {
            let blob = repo.blob(content.as_bytes()).unwrap();
            builder.insert(path, blob, 0o100644).unwrap();
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

    /// The main folder on `develop`; the trunk `main` at c1 locally, and
    /// origin's `main` at c2, one commit ahead (a.rs changed, new.rs added).
    struct Fixture {
        dir: tempfile::TempDir,
        repo: git2::Repository,
        c1: git2::Oid,
        c2: git2::Oid,
    }

    fn fixture() -> Fixture {
        let dir = tempfile::tempdir().unwrap();
        let repo = git2::Repository::init(dir.path().join("main")).unwrap();
        let c1 = commit(&repo, &[("a.rs", "1"), ("b.rs", "1")], None);
        let c2 = commit(
            &repo,
            &[("a.rs", "2"), ("b.rs", "1"), ("new.rs", "1")],
            Some(c1),
        );
        repo.reference("refs/heads/main", c1, true, "").unwrap();
        repo.reference("refs/remotes/origin/main", c2, true, "")
            .unwrap();
        repo.reference("refs/heads/develop", c1, true, "").unwrap();
        repo.set_head("refs/heads/develop").unwrap();
        Fixture { dir, repo, c1, c2 }
    }

    fn root(f: &Fixture) -> std::path::PathBuf {
        f.dir.path().join("main")
    }

    #[test]
    fn the_trunk_follows_origin_when_origin_fast_forwards_it() {
        let f = fixture();
        let trunk = followed_trunk(&root(&f)).expect("main is followed from git");
        assert_eq!(
            (trunk.name.as_str(), trunk.tip, trunk.local),
            ("main", f.c2, f.c1)
        );
        assert!(trunk.from_origin());
        assert!(is_followed_trunk(&root(&f), "main"));
        assert!(!is_followed_trunk(&root(&f), "develop"));

        // Pulled: the same tip either way.
        f.repo.reference("refs/heads/main", f.c2, true, "").unwrap();
        let trunk = followed_trunk(&root(&f)).unwrap();
        assert_eq!(trunk.tip, f.c2);
        assert!(!trunk.from_origin());
    }

    #[test]
    fn local_work_on_the_trunk_is_never_overridden() {
        let f = fixture();
        // Ahead of origin: a local commit on top of c2.
        let ahead = commit(&f.repo, &[("a.rs", "3")], Some(f.c2));
        f.repo
            .reference("refs/heads/main", ahead, true, "")
            .unwrap();
        assert_eq!(followed_trunk(&root(&f)).unwrap().tip, ahead);
        // Diverged from origin: a local commit on top of c1.
        let diverged = commit(&f.repo, &[("a.rs", "4")], Some(f.c1));
        f.repo
            .reference("refs/heads/main", diverged, true, "")
            .unwrap();
        assert_eq!(followed_trunk(&root(&f)).unwrap().tip, diverged);
    }

    #[test]
    fn the_main_folders_own_trunk_or_a_remote_only_one_is_not_followed() {
        let f = fixture();
        f.repo.set_head("refs/heads/main").unwrap();
        assert_eq!(
            followed_trunk(&root(&f)),
            None,
            "the watcher follows that disk"
        );

        f.repo.set_head("refs/heads/develop").unwrap();
        f.repo
            .find_reference("refs/heads/main")
            .unwrap()
            .delete()
            .unwrap();
        // Configured, so the trunk stays `main` with only the remote's ref left.
        f.repo
            .config()
            .unwrap()
            .set_str(super::super::tips::TRUNK_CONFIG_KEY, "main")
            .unwrap();
        assert_eq!(
            followed_trunk(&root(&f)),
            None,
            "main exists only as origin/main: the branch prune's, never tagged here"
        );
    }

    /// The fill-in compares a branch with the trunk the index holds: a branch
    /// cut from origin's tip changed nothing, though the stale local trunk
    /// differs from it on two paths. Graph backfill reads the same version.
    #[test]
    fn fill_in_diffs_and_trunk_reads_use_the_followed_tip() {
        let f = fixture();
        f.repo.reference("refs/heads/feat", f.c2, true, "").unwrap();
        let changed = paths_changed_between(&root(&f), "main", "feat").unwrap();
        assert!(changed.is_empty(), "feat is origin's main: {changed:?}");
        assert_eq!(
            blob_at_branch_tip(&root(&f), "main", "a.rs").as_deref(),
            Some(&b"2"[..])
        );

        // The main folder on the trunk: its local ref is the trunk, as before.
        f.repo.set_head("refs/heads/main").unwrap();
        let changed = paths_changed_between(&root(&f), "main", "feat").unwrap();
        assert_eq!(changed.len(), 2, "a.rs and new.rs: {changed:?}");
        assert_eq!(
            blob_at_branch_tip(&root(&f), "main", "a.rs").as_deref(),
            Some(&b"1"[..])
        );
    }

    /// A linked worktree on the followed trunk is a copy git may have moved
    /// past: no path of the trunk is judged against its disk. Other worktrees
    /// are still their branches' checkouts.
    #[test]
    fn a_worktree_on_the_followed_trunk_is_no_checkout_of_it() {
        let f = fixture();
        let main_ref = f.repo.find_reference("refs/heads/main").unwrap();
        let mut opts = git2::WorktreeAddOptions::new();
        opts.reference(Some(&main_ref));
        let wt_trunk = f.dir.path().join("wt-main");
        f.repo.worktree("wt-main", &wt_trunk, Some(&opts)).unwrap();
        f.repo.reference("refs/heads/feat", f.c2, true, "").unwrap();
        let feat_ref = f.repo.find_reference("refs/heads/feat").unwrap();
        let mut opts = git2::WorktreeAddOptions::new();
        opts.reference(Some(&feat_ref));
        let wt_feat = f.dir.path().join("wt-feat");
        f.repo.worktree("wt-feat", &wt_feat, Some(&opts)).unwrap();

        let co = BranchCheckouts::discover(&root(&f));
        assert_eq!(co.root_for("main"), None);
        assert_eq!(co.presence("main", "a.rs"), CheckoutPresence::NoCheckout);
        assert_eq!(
            co.root_for("feat")
                .map(std::fs::canonicalize)
                .transpose()
                .unwrap(),
            Some(std::fs::canonicalize(&wt_feat).unwrap())
        );
    }
}
