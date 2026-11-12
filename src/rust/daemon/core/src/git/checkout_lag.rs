//! How far the main folder's checkout of the trunk trails `origin/<trunk>`.
//!
//! When the main folder has the trunk checked out, its disk is the truth the
//! watcher indexes, and the daemon never moves a user's working tree. So a
//! main checkout nobody pulls keeps the trunk's index — and every branch view
//! filled in from it — at that old commit. Measured 2026-11-12: DOC-V2's main
//! checkout sat 412 commits behind `origin/main` (emnify 274), while the
//! agents' worktrees branched off origin; search and grep on the trunk
//! answered from code months old, and nothing said so.
//!
//! [`main_checkout_lag`] measures it so the read surfaces and the metrics can.
//! It reads git only (no fetch): `origin/<trunk>` is as fresh as the last
//! fetch anyone ran in the repository.

use std::path::Path;

use super::tips::default_branch_in;

/// The main folder's trunk checkout against `origin/<trunk>`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CheckoutLag {
    /// The trunk the main folder has checked out.
    pub trunk: String,
    /// Commits `origin/<trunk>` has that the checkout does not.
    pub behind: usize,
    /// Commits the checkout has that `origin/<trunk>` does not.
    pub ahead: usize,
}

/// The lag of `repo_root`'s main checkout behind `origin/<trunk>`.
///
/// `None` when it does not apply: not a repository, no trunk, the main folder
/// on another branch (that branch is the user's work, not a stale trunk), or
/// no `origin/<trunk>` to compare with.
pub fn main_checkout_lag(repo_root: &Path) -> Option<CheckoutLag> {
    let repo = git2::Repository::open(repo_root).ok()?;
    let trunk = default_branch_in(&repo)?;
    let local_ref = format!("refs/heads/{trunk}");
    let head = repo.find_reference("HEAD").ok()?;
    if head.symbolic_target() != Some(local_ref.as_str()) {
        return None;
    }
    let local = repo.refname_to_id(&local_ref).ok()?;
    let origin = repo
        .refname_to_id(&format!("refs/remotes/origin/{trunk}"))
        .ok()?;
    let (ahead, behind) = repo.graph_ahead_behind(local, origin).ok()?;
    Some(CheckoutLag {
        trunk,
        behind,
        ahead,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn commit(repo: &git2::Repository, content: &str, parent: Option<git2::Oid>) -> git2::Oid {
        let mut builder = repo.treebuilder(None).unwrap();
        let blob = repo.blob(content.as_bytes()).unwrap();
        builder.insert("a.rs", blob, 0o100644).unwrap();
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

    /// The main folder on `main` at c1; origin three commits ahead.
    fn fixture() -> (tempfile::TempDir, git2::Repository, git2::Oid, git2::Oid) {
        let dir = tempfile::tempdir().unwrap();
        let repo = git2::Repository::init(dir.path()).unwrap();
        let c1 = commit(&repo, "1", None);
        let mut tip = c1;
        for i in 2..=4 {
            tip = commit(&repo, &i.to_string(), Some(tip));
        }
        repo.reference("refs/heads/main", c1, true, "").unwrap();
        repo.reference("refs/remotes/origin/main", tip, true, "")
            .unwrap();
        repo.set_head("refs/heads/main").unwrap();
        (dir, repo, c1, tip)
    }

    #[test]
    fn a_main_checkout_of_the_trunk_reports_how_far_origin_is_ahead() {
        let (dir, repo, c1, tip) = fixture();
        let lag = main_checkout_lag(dir.path()).expect("main is checked out");
        assert_eq!(
            lag,
            CheckoutLag {
                trunk: "main".into(),
                behind: 3,
                ahead: 0
            }
        );

        // A local commit of its own: diverged, both sides counted.
        let local = commit(&repo, "local", Some(c1));
        repo.reference("refs/heads/main", local, true, "").unwrap();
        let lag = main_checkout_lag(dir.path()).unwrap();
        assert_eq!((lag.behind, lag.ahead), (3, 1));

        // Pulled: current.
        repo.reference("refs/heads/main", tip, true, "").unwrap();
        let lag = main_checkout_lag(dir.path()).unwrap();
        assert_eq!((lag.behind, lag.ahead), (0, 0));
    }

    #[test]
    fn another_branch_or_no_origin_reports_nothing() {
        let (dir, repo, c1, _) = fixture();
        repo.reference("refs/heads/feat", c1, true, "").unwrap();
        repo.set_head("refs/heads/feat").unwrap();
        assert_eq!(main_checkout_lag(dir.path()), None, "the user's own work");

        repo.set_head("refs/heads/main").unwrap();
        repo.find_reference("refs/remotes/origin/main")
            .unwrap()
            .delete()
            .unwrap();
        assert_eq!(
            main_checkout_lag(dir.path()),
            None,
            "nothing to compare with"
        );
        assert_eq!(main_checkout_lag(&dir.path().join("nope")), None);
    }

    /// The trunk measured is the daemon's trunk: `wqm.trunk` (tecsul's
    /// `release/platform`, with a slash), else what `origin/HEAD` names.
    #[test]
    fn the_configured_or_remote_default_trunk_is_the_one_measured() {
        let (dir, repo, c1, tip) = fixture();
        repo.reference("refs/heads/release/platform", c1, true, "")
            .unwrap();
        repo.reference("refs/remotes/origin/release/platform", tip, true, "")
            .unwrap();
        repo.config()
            .unwrap()
            .set_str(super::super::tips::TRUNK_CONFIG_KEY, "release/platform")
            .unwrap();
        repo.set_head("refs/heads/release/platform").unwrap();
        let lag = main_checkout_lag(dir.path()).expect("on the configured trunk");
        assert_eq!((lag.trunk.as_str(), lag.behind), ("release/platform", 3));

        // On `main`, which is no longer the trunk: nothing to report.
        repo.set_head("refs/heads/main").unwrap();
        assert_eq!(main_checkout_lag(dir.path()), None);

        // No config: origin/HEAD names the trunk.
        repo.config()
            .unwrap()
            .remove(super::super::tips::TRUNK_CONFIG_KEY)
            .unwrap();
        repo.reference("refs/heads/develop", c1, true, "").unwrap();
        repo.reference("refs/remotes/origin/develop", tip, true, "")
            .unwrap();
        repo.reference_symbolic(
            "refs/remotes/origin/HEAD",
            "refs/remotes/origin/develop",
            true,
            "",
        )
        .unwrap();
        repo.set_head("refs/heads/develop").unwrap();
        let lag = main_checkout_lag(dir.path()).expect("on origin's default");
        assert_eq!((lag.trunk.as_str(), lag.behind), ("develop", 3));
    }

    #[test]
    fn a_detached_head_reports_nothing() {
        let (dir, repo, c1, _) = fixture();
        repo.set_head_detached(c1).unwrap();
        assert_eq!(main_checkout_lag(dir.path()), None);
    }
}
