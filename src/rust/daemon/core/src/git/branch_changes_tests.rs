use std::collections::HashSet;
use std::path::Path;

use git2::{Commit, Oid, Repository, Signature};

use super::paths_changed_since_fork;
use crate::git::modified_paths_head_vs_branch;

/// A commit of exactly `files` (flat paths) on `refname`, with `parents`.
fn commit(repo: &Repository, refname: &str, files: &[(&str, &str)], parents: &[&Commit]) -> Oid {
    let mut tb = repo.treebuilder(None).unwrap();
    for (path, body) in files {
        let blob = repo.blob(body.as_bytes()).unwrap();
        tb.insert(path, blob, 0o100644).unwrap();
    }
    let tree = repo.find_tree(tb.write().unwrap()).unwrap();
    let sig = Signature::now("t", "t@t").unwrap();
    repo.commit(Some(refname), &sig, &sig, refname, &tree, parents)
        .unwrap()
}

fn set(items: &[&str]) -> HashSet<String> {
    items.iter().map(|s| s.to_string()).collect()
}

/// tecsul's layout: trunk `release` (via `wqm.trunk`), the main folder on a
/// personal branch that lacks the trunk's files, an agent branch forked from
/// the trunk, and the trunk moving on after the fork.
fn tecsul_like(head_on_trunk: bool) -> (tempfile::TempDir, Repository) {
    let tmp = tempfile::tempdir().unwrap();
    let repo = Repository::init(tmp.path()).unwrap();
    repo.config()
        .unwrap()
        .set_str("wqm.trunk", "release")
        .unwrap();
    let c0 = commit(
        &repo,
        "refs/heads/release",
        &[("obj.rs", "v1"), ("same.rs", "s"), ("gone.rs", "g")],
        &[],
    );
    let c0 = repo.find_commit(c0).unwrap();
    // The agent's own work: an edit, a new file, a delete.
    commit(
        &repo,
        "refs/heads/agent/x",
        &[("obj.rs", "v2"), ("same.rs", "s"), ("new.rs", "n")],
        &[&c0],
    );
    // The trunk after the fork: not the branch's work.
    commit(
        &repo,
        "refs/heads/release",
        &[
            ("obj.rs", "v1"),
            ("same.rs", "s"),
            ("gone.rs", "g"),
            ("later.rs", "l"),
        ],
        &[&c0],
    );
    commit(&repo, "refs/heads/mine", &[("legacy.rs", "old")], &[]);
    // A Commit borrows its Repository: release it before moving the repo out.
    drop(c0);
    let head = if head_on_trunk {
        "refs/heads/release"
    } else {
        "refs/heads/mine"
    };
    repo.set_head(head).unwrap();
    (tmp, repo)
}

#[test]
fn the_branchs_own_work_since_the_fork_added_files_included() {
    let (tmp, _repo) = tecsul_like(false);
    assert_eq!(
        paths_changed_since_fork(tmp.path(), "agent/x"),
        set(&["obj.rs", "new.rs"]),
        "the edit and the new file; not the delete, not the trunk's later file"
    );
    // The old blind spot this fixes: the main HEAD lacks obj.rs, so git calls
    // it Added there and the Modified-only diff never sees the edit.
    assert!(!modified_paths_head_vs_branch(tmp.path(), "agent/x").contains("obj.rs"));
}

#[test]
fn the_same_answer_when_the_main_folder_has_the_trunk_checked_out() {
    let (tmp, _repo) = tecsul_like(true);
    assert_eq!(
        paths_changed_since_fork(tmp.path(), "agent/x"),
        set(&["obj.rs", "new.rs"])
    );
}

#[test]
fn nothing_for_an_unknown_branch_or_one_with_no_commits_of_its_own() {
    let (tmp, repo) = tecsul_like(false);
    assert!(paths_changed_since_fork(tmp.path(), "nope").is_empty());
    let trunk_tip = repo.refname_to_id("refs/heads/release").unwrap();
    repo.reference("refs/heads/fresh", trunk_tip, true, "fresh")
        .unwrap();
    assert!(paths_changed_since_fork(tmp.path(), "fresh").is_empty());
    assert!(paths_changed_since_fork(Path::new("/nonexistent/repo"), "agent/x").is_empty());
}
