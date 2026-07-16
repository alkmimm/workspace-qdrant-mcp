//! Tests for WatchType, collection constants, and library watch configuration.

use super::super::*;
use tempfile::tempdir;

#[test]
fn test_get_current_branch_non_git() {
    let temp_dir = tempdir().unwrap();
    let branch = get_current_branch(temp_dir.path());
    assert_eq!(branch, "main");
}

/// Helper: init a repo on branch `feat/x` with one commit.
fn init_repo_on_branch(path: &std::path::Path) -> git2::Repository {
    let mut opts = git2::RepositoryInitOptions::new();
    opts.initial_head("feat/x");
    let repo = git2::Repository::init_opts(path, &opts).unwrap();
    let sig = git2::Signature::now("Test", "test@example.com").unwrap();
    let tree_id = {
        let mut index = repo.index().unwrap();
        index.write_tree().unwrap()
    };
    {
        let tree = repo.find_tree(tree_id).unwrap();
        repo.commit(Some("HEAD"), &sig, &sig, "init", &tree, &[])
            .unwrap();
    }
    repo
}

// ── get_current_branch_opt (#224): no "main" fallback ──────────────────────
// The branch re-stamp ACTS on the resolved value, so it must get `None` (and
// stand down) in every case where `get_current_branch` would confidently
// guess "main".

#[test]
fn branch_opt_is_none_for_non_git_dir() {
    let temp_dir = tempdir().unwrap();
    assert_eq!(get_current_branch_opt(temp_dir.path()), None);
}

#[test]
fn branch_opt_is_none_for_repo_without_commits() {
    let temp_dir = tempdir().unwrap();
    git2::Repository::init(temp_dir.path()).unwrap();
    assert_eq!(get_current_branch_opt(temp_dir.path()), None);
}

#[test]
fn branch_opt_reads_the_checked_out_branch() {
    let temp_dir = tempdir().unwrap();
    init_repo_on_branch(temp_dir.path());
    assert_eq!(
        get_current_branch_opt(temp_dir.path()),
        Some("feat/x".to_string())
    );
    // ...and the fallback form agrees when git CAN answer.
    assert_eq!(get_current_branch(temp_dir.path()), "feat/x");
}

#[test]
fn branch_opt_is_none_for_detached_head() {
    let temp_dir = tempdir().unwrap();
    let repo = init_repo_on_branch(temp_dir.path());
    let head_oid = repo.head().unwrap().target().unwrap();
    repo.set_head_detached(head_oid).unwrap();
    assert_eq!(get_current_branch_opt(temp_dir.path()), None);
}

// Multi-tenant routing tests
#[test]
fn test_watch_type_default() {
    assert_eq!(WatchType::default(), WatchType::Project);
}

#[test]
fn test_watch_type_from_str() {
    assert_eq!(WatchType::from_str("project"), Some(WatchType::Project));
    assert_eq!(WatchType::from_str("library"), Some(WatchType::Library));
    assert_eq!(WatchType::from_str("PROJECT"), Some(WatchType::Project));
    assert_eq!(WatchType::from_str("LIBRARY"), Some(WatchType::Library));
    assert_eq!(WatchType::from_str("invalid"), None);
}

#[test]
fn test_watch_type_as_str() {
    assert_eq!(WatchType::Project.as_str(), "project");
    assert_eq!(WatchType::Library.as_str(), "library");
}

#[test]
fn test_unified_collection_constants() {
    use wqm_common::constants::{COLLECTION_LIBRARIES, COLLECTION_PROJECTS};
    // Canonical collection names (without underscore prefix)
    assert_eq!(COLLECTION_PROJECTS, "projects");
    assert_eq!(COLLECTION_LIBRARIES, "libraries");
}

// Library watch ID format tests
#[test]
fn test_library_watch_id_format() {
    let library_name = "langchain";
    let id = format!("lib_{}", library_name);

    assert!(id.starts_with("lib_"));
    assert_eq!(id, "lib_langchain");

    // Test stripping prefix
    let extracted = id.strip_prefix("lib_").unwrap_or(&id);
    assert_eq!(extracted, "langchain");
}

#[test]
fn test_library_watch_config_creation() {
    use std::path::PathBuf;
    let library_name = "my_docs";
    let id = format!("lib_{}", library_name);

    let config = WatchConfig {
        id: id.clone(),
        path: PathBuf::from("/path/to/docs"),
        tenant_id: library_name.to_string(),
        collection: format!("_{}", library_name),
        patterns: vec!["*.pdf".to_string(), "*.md".to_string()],
        ignore_patterns: vec![".git/*".to_string()],
        recursive: true,
        debounce_ms: 2000,
        enabled: true,
        watch_type: WatchType::Library,
        library_name: Some(library_name.to_string()),
    };

    assert_eq!(config.watch_type, WatchType::Library);
    assert_eq!(config.library_name, Some("my_docs".to_string()));
    assert_eq!(config.collection, "_my_docs");
}
