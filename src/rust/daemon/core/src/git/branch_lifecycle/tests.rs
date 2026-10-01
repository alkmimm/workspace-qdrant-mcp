use super::*;
use std::path::Path;
use std::time::Duration;
use tempfile::tempdir;

/// Helper to create a Git repository with initial commit
fn create_test_repo(path: &Path) -> Result<git2::Repository, git2::Error> {
    let repo = git2::Repository::init(path)?;

    let sig = git2::Signature::now("Test User", "test@example.com")?;
    let tree_id = {
        let mut index = repo.index()?;
        index.write_tree()?
    };
    {
        let tree = repo.find_tree(tree_id)?;
        repo.commit(Some("HEAD"), &sig, &sig, "Initial commit", &tree, &[])?;
    }

    Ok(repo)
}

#[test]
fn test_branch_event_branch_name() {
    let event = BranchEvent::Created {
        branch: "feature/test".to_string(),
        commit_hash: Some("abc123".to_string()),
    };
    assert_eq!(event.branch_name(), "feature/test");

    let event = BranchEvent::Deleted {
        branch: "old-branch".to_string(),
    };
    assert_eq!(event.branch_name(), "old-branch");

    let event = BranchEvent::Renamed {
        old_name: "old".to_string(),
        new_name: "new".to_string(),
    };
    assert_eq!(event.branch_name(), "new");
}

#[test]
fn test_branch_event_affects_branch() {
    let event = BranchEvent::Renamed {
        old_name: "feature/old".to_string(),
        new_name: "feature/new".to_string(),
    };
    assert!(event.affects_branch("feature/old"));
    assert!(event.affects_branch("feature/new"));
    assert!(!event.affects_branch("main"));
}

#[test]
fn test_branch_lifecycle_config_default() {
    let config = BranchLifecycleConfig::default();
    assert!(config.enabled);
    assert!(config.auto_delete_on_branch_delete);
    assert_eq!(config.scan_interval_seconds, 5);
    assert_eq!(config.rename_correlation_timeout_ms, 500);
}

#[tokio::test]
async fn test_branch_lifecycle_detector_initialize() {
    let temp_dir = tempdir().unwrap();
    let repo_path = temp_dir.path();

    create_test_repo(repo_path).unwrap();

    let detector = BranchLifecycleDetector::with_defaults(repo_path.to_path_buf());
    detector.initialize().await.unwrap();

    let branches = detector.get_tracked_branches().await;
    assert!(!branches.is_empty());
}

#[tokio::test]
async fn test_branch_lifecycle_detector_list_branches() {
    let temp_dir = tempdir().unwrap();
    let repo_path = temp_dir.path();

    let repo = create_test_repo(repo_path).unwrap();

    let head = repo.head().unwrap();
    let commit = head.peel_to_commit().unwrap();
    repo.branch("feature/test", &commit, false).unwrap();
    repo.branch("develop", &commit, false).unwrap();

    let detector = BranchLifecycleDetector::with_defaults(repo_path.to_path_buf());
    let branches = detector.list_all_branches().unwrap();

    assert!(branches.len() >= 3);

    let branch_names: Vec<_> = branches.iter().map(|(n, _, _)| n.as_str()).collect();
    assert!(branch_names.contains(&"feature/test"));
    assert!(branch_names.contains(&"develop"));
}

#[tokio::test]
async fn test_branch_lifecycle_detect_creation() {
    let temp_dir = tempdir().unwrap();
    let repo_path = temp_dir.path();

    let repo = create_test_repo(repo_path).unwrap();

    let detector = BranchLifecycleDetector::with_defaults(repo_path.to_path_buf());
    detector.initialize().await.unwrap();

    let head = repo.head().unwrap();
    let commit = head.peel_to_commit().unwrap();
    repo.branch("new-feature", &commit, false).unwrap();

    let events = detector.scan_for_changes().await.unwrap();

    assert!(!events.is_empty());
    assert!(events.iter().any(|e| matches!(
        e,
        BranchEvent::Created { branch, .. } if branch == "new-feature"
    )));
}

#[tokio::test]
async fn test_branch_lifecycle_detect_deletion() {
    let temp_dir = tempdir().unwrap();
    let repo_path = temp_dir.path();

    let repo = create_test_repo(repo_path).unwrap();

    let head = repo.head().unwrap();
    let commit = head.peel_to_commit().unwrap();
    repo.branch("to-delete", &commit, false).unwrap();

    let detector = BranchLifecycleDetector::with_defaults(repo_path.to_path_buf());
    detector.initialize().await.unwrap();

    let mut branch = repo
        .find_branch("to-delete", git2::BranchType::Local)
        .unwrap();
    branch.delete().unwrap();

    let _events = detector.scan_for_changes().await.unwrap();

    tokio::time::sleep(Duration::from_millis(600)).await;

    let events = detector.scan_for_changes().await.unwrap();

    assert!(events.iter().any(|e| matches!(
        e,
        BranchEvent::Deleted { branch } if branch == "to-delete"
    )));
}

#[tokio::test]
async fn test_branch_lifecycle_stats() {
    let temp_dir = tempdir().unwrap();
    let repo_path = temp_dir.path();

    let repo = create_test_repo(repo_path).unwrap();

    let head = repo.head().unwrap();
    let commit = head.peel_to_commit().unwrap();
    repo.branch("feature-a", &commit, false).unwrap();
    repo.branch("feature-b", &commit, false).unwrap();

    let detector = BranchLifecycleDetector::with_defaults(repo_path.to_path_buf());
    detector.initialize().await.unwrap();

    let stats = detector.stats().await;
    assert!(stats.tracked_branches >= 3);
    assert_eq!(stats.pending_deletes, 0);
}

/// Checking out another existing branch creates, deletes and renames nothing,
/// so a scan must report nothing.
///
/// It used to report a default-branch change: the detector read `.git/HEAD` —
/// the CHECKED-OUT branch — and called it the default, so every `git checkout`
/// of a feature branch surfaced as `DefaultChanged { main -> feature }`. The
/// older tests could not see this because they renamed the current branch,
/// where "current" and "default" happen to be the same name.
#[tokio::test]
async fn test_switching_head_is_not_a_lifecycle_event() {
    let temp_dir = tempdir().unwrap();
    let repo_path = temp_dir.path();

    let repo = create_test_repo(repo_path).unwrap();
    let head = repo.head().unwrap();
    let commit = head.peel_to_commit().unwrap();
    repo.branch("feature-a", &commit, false).unwrap();

    let detector = BranchLifecycleDetector::with_defaults(repo_path.to_path_buf());
    detector.initialize().await.unwrap();

    repo.set_head("refs/heads/feature-a").unwrap();

    let events = detector.scan_for_changes().await.unwrap();
    assert!(
        events.is_empty(),
        "a checkout is not a branch lifecycle event, got {:?}",
        events
    );
}
