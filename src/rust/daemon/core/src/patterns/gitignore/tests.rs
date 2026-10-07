use std::fs;

use tempfile::TempDir;

use super::*;

fn tmp() -> TempDir {
    tempfile::tempdir().unwrap()
}

// ── no ignore files ────────────────────────────────────────────────────────

#[test]
fn no_ignore_files_returns_none() {
    let dir = tmp();
    assert!(ProjectIgnoreMatcher::for_dir(dir.path(), None).is_none());
}

// ── .gitignore only ────────────────────────────────────────────────────────

#[test]
fn gitignore_excludes_matching_directory() {
    let dir = tmp();
    fs::write(dir.path().join(".gitignore"), "datasets/\n").unwrap();
    let m = ProjectIgnoreMatcher::for_dir(dir.path(), None).unwrap();
    assert!(m.is_ignored(&dir.path().join("datasets"), true));
}

#[test]
fn gitignore_does_not_exclude_non_matching_directory() {
    let dir = tmp();
    fs::write(dir.path().join(".gitignore"), "datasets/\n").unwrap();
    let m = ProjectIgnoreMatcher::for_dir(dir.path(), None).unwrap();
    assert!(!m.is_ignored(&dir.path().join("src"), true));
}

#[test]
fn gitignore_excludes_file_by_extension() {
    let dir = tmp();
    fs::write(dir.path().join(".gitignore"), "*.log\n").unwrap();
    let m = ProjectIgnoreMatcher::for_dir(dir.path(), None).unwrap();
    assert!(m.is_ignored(&dir.path().join("debug.log"), false));
    assert!(!m.is_ignored(&dir.path().join("main.rs"), false));
}

// ── .wqmignore only ───────────────────────────────────────────────────────

#[test]
fn wqmignore_only_excludes_matching_directory() {
    let dir = tmp();
    fs::write(dir.path().join(".wqmignore"), "large_data/\n").unwrap();
    let m = ProjectIgnoreMatcher::for_dir(dir.path(), None).unwrap();
    assert!(m.is_ignored(&dir.path().join("large_data"), true));
    assert!(!m.is_ignored(&dir.path().join("src"), true));
}

// ── union semantics ────────────────────────────────────────────────────────

#[test]
fn union_semantics_gitignore_pattern_excludes() {
    let dir = tmp();
    fs::write(dir.path().join(".gitignore"), "git_only/\n").unwrap();
    fs::write(dir.path().join(".wqmignore"), "wqm_only/\n").unwrap();
    let m = ProjectIgnoreMatcher::for_dir(dir.path(), None).unwrap();
    assert!(m.is_ignored(&dir.path().join("git_only"), true));
    assert!(m.is_ignored(&dir.path().join("wqm_only"), true));
    assert!(!m.is_ignored(&dir.path().join("src"), true));
}

#[test]
fn union_semantics_both_files_can_match_independently() {
    let dir = tmp();
    fs::write(dir.path().join(".gitignore"), "build/\n").unwrap();
    fs::write(dir.path().join(".wqmignore"), "datasets/\n").unwrap();
    let m = ProjectIgnoreMatcher::for_dir(dir.path(), None).unwrap();
    // .gitignore hit
    assert!(m.is_ignored(&dir.path().join("build"), true));
    // .wqmignore hit
    assert!(m.is_ignored(&dir.path().join("datasets"), true));
    // neither
    assert!(!m.is_ignored(&dir.path().join("docs"), true));
}

// ── no false positives on non-ignored files ───────────────────────────────

#[test]
fn non_ignored_file_is_not_excluded() {
    let dir = tmp();
    fs::write(dir.path().join(".gitignore"), "*.bin\n").unwrap();
    let m = ProjectIgnoreMatcher::for_dir(dir.path(), None).unwrap();
    assert!(!m.is_ignored(&dir.path().join("README.md"), false));
    assert!(!m.is_ignored(&dir.path().join("main.rs"), false));
}

// ── .wqmignore negation syntax ─────────────────────────────────────────

#[test]
fn wqmignore_reinclusion_overrides_gitignore() {
    let dir = tmp();
    fs::write(dir.path().join(".gitignore"), "dist/\n").unwrap();
    fs::write(dir.path().join(".wqmignore"), "- dist/\n").unwrap();
    let m = ProjectIgnoreMatcher::for_dir(dir.path(), None).unwrap();
    // dist/ is in .gitignore but re-included by .wqmignore
    assert!(!m.is_ignored(&dir.path().join("dist"), true));
}

#[test]
fn wqmignore_reinclusion_does_not_affect_other_exclusions() {
    let dir = tmp();
    fs::write(dir.path().join(".gitignore"), "dist/\nnode_modules/\n").unwrap();
    fs::write(dir.path().join(".wqmignore"), "- dist/\n").unwrap();
    let m = ProjectIgnoreMatcher::for_dir(dir.path(), None).unwrap();
    // dist/ re-included
    assert!(!m.is_ignored(&dir.path().join("dist"), true));
    // node_modules/ still excluded by .gitignore
    assert!(m.is_ignored(&dir.path().join("node_modules"), true));
}

#[test]
fn wqmignore_mixed_exclusion_and_reinclusion() {
    let dir = tmp();
    fs::write(dir.path().join(".gitignore"), "build/\n").unwrap();
    fs::write(dir.path().join(".wqmignore"), "tmp/\n- build/\n").unwrap();
    let m = ProjectIgnoreMatcher::for_dir(dir.path(), None).unwrap();
    // build/ is in .gitignore but re-included by .wqmignore
    assert!(!m.is_ignored(&dir.path().join("build"), true));
    // tmp/ is excluded by .wqmignore
    assert!(m.is_ignored(&dir.path().join("tmp"), true));
    // src/ is not excluded
    assert!(!m.is_ignored(&dir.path().join("src"), true));
}

#[test]
fn wqmignore_reinclusion_with_glob_pattern() {
    let dir = tmp();
    fs::write(dir.path().join(".gitignore"), "*.generated.js\n").unwrap();
    fs::write(dir.path().join(".wqmignore"), "- *.generated.js\n").unwrap();
    let m = ProjectIgnoreMatcher::for_dir(dir.path(), None).unwrap();
    assert!(!m.is_ignored(&dir.path().join("api.generated.js"), false));
}

#[test]
fn wqmignore_comments_and_blank_lines_ignored() {
    let dir = tmp();
    fs::write(dir.path().join(".gitignore"), "dist/\n").unwrap();
    fs::write(
        dir.path().join(".wqmignore"),
        "# Re-include dist for indexing\n\n- dist/\n\n# Extra exclusion\ntmp/\n",
    )
    .unwrap();
    let m = ProjectIgnoreMatcher::for_dir(dir.path(), None).unwrap();
    assert!(!m.is_ignored(&dir.path().join("dist"), true));
    assert!(m.is_ignored(&dir.path().join("tmp"), true));
}

// ── canonical !pattern syntax ──────────────────────────────────────────

#[test]
fn wqmignore_exclamation_reinclusion_overrides_gitignore() {
    let dir = tmp();
    fs::write(dir.path().join(".gitignore"), "dist/\n").unwrap();
    fs::write(dir.path().join(".wqmignore"), "!dist/\n").unwrap();
    let m = ProjectIgnoreMatcher::for_dir(dir.path(), None).unwrap();
    assert!(!m.is_ignored(&dir.path().join("dist"), true));
}

#[test]
fn wqmignore_exclamation_with_glob_pattern() {
    let dir = tmp();
    fs::write(dir.path().join(".gitignore"), "*.generated.js\n").unwrap();
    fs::write(dir.path().join(".wqmignore"), "!*.generated.js\n").unwrap();
    let m = ProjectIgnoreMatcher::for_dir(dir.path(), None).unwrap();
    assert!(!m.is_ignored(&dir.path().join("api.generated.js"), false));
}

#[test]
fn wqmignore_mixed_legacy_and_canonical_syntax() {
    let dir = tmp();
    fs::write(dir.path().join(".gitignore"), "build/\nvendor/\n").unwrap();
    fs::write(dir.path().join(".wqmignore"), "- build/\n!vendor/\n").unwrap();
    let m = ProjectIgnoreMatcher::for_dir(dir.path(), None).unwrap();
    // Both legacy `- build/` and canonical `!vendor/` re-include
    assert!(!m.is_ignored(&dir.path().join("build"), true));
    assert!(!m.is_ignored(&dir.path().join("vendor"), true));
}

#[test]
fn wqmignore_exclamation_does_not_affect_other_exclusions() {
    let dir = tmp();
    fs::write(dir.path().join(".gitignore"), "dist/\nnode_modules/\n").unwrap();
    fs::write(dir.path().join(".wqmignore"), "!dist/\n").unwrap();
    let m = ProjectIgnoreMatcher::for_dir(dir.path(), None).unwrap();
    // dist/ re-included via canonical syntax
    assert!(!m.is_ignored(&dir.path().join("dist"), true));
    // node_modules/ still excluded by .gitignore
    assert!(m.is_ignored(&dir.path().join("node_modules"), true));
}

#[test]
fn wqmignore_no_reinclusions_has_no_reinclusion_matcher() {
    let dir = tmp();
    fs::write(dir.path().join(".wqmignore"), "data/\n").unwrap();
    let m = ProjectIgnoreMatcher::for_dir(dir.path(), None).unwrap();
    assert!(m.reinclusion_layers.is_empty());
    assert!(m.is_ignored(&dir.path().join("data"), true));
}

#[test]
fn wqmignore_only_reinclusions_no_exclusions() {
    let dir = tmp();
    fs::write(dir.path().join(".gitignore"), "vendor/\n").unwrap();
    fs::write(dir.path().join(".wqmignore"), "- vendor/\n").unwrap();
    let m = ProjectIgnoreMatcher::for_dir(dir.path(), None).unwrap();
    assert!(!m.is_ignored(&dir.path().join("vendor"), true));
}

// ── parent cascade (project_root) ─────────────────────────────────────

#[test]
fn parent_cascade_gitignore_inherited_by_subdir() {
    let root = tmp();
    // project root has .gitignore excluding dist/
    fs::write(root.path().join(".gitignore"), "dist/\n").unwrap();
    // Create subdir/deep/ with no ignore files
    let deep = root.path().join("subdir").join("deep");
    fs::create_dir_all(&deep).unwrap();

    let m = ProjectIgnoreMatcher::for_dir(&deep, Some(root.path())).unwrap();
    // dist/ pattern from root .gitignore should apply in deep subdir
    assert!(m.is_ignored(&deep.join("dist"), true));
    // unmatched paths still pass
    assert!(!m.is_ignored(&deep.join("src"), true));
}

#[test]
fn parent_cascade_wqmignore_inherited_by_subdir() {
    let root = tmp();
    fs::write(root.path().join(".wqmignore"), "tmp/\n").unwrap();
    let subdir = root.path().join("subdir");
    fs::create_dir_all(&subdir).unwrap();

    let m = ProjectIgnoreMatcher::for_dir(&subdir, Some(root.path())).unwrap();
    assert!(m.is_ignored(&subdir.join("tmp"), true));
}

#[test]
fn parent_cascade_reinclusion_overrides_ancestor_gitignore() {
    let root = tmp();
    // Root .gitignore excludes build/
    fs::write(root.path().join(".gitignore"), "build/\n").unwrap();
    // Root .wqmignore re-includes build/
    fs::write(root.path().join(".wqmignore"), "!build/\n").unwrap();
    let subdir = root.path().join("subdir");
    fs::create_dir_all(&subdir).unwrap();

    let m = ProjectIgnoreMatcher::for_dir(&subdir, Some(root.path())).unwrap();
    // build/ excluded by .gitignore but re-included by .wqmignore
    assert!(!m.is_ignored(&subdir.join("build"), true));
}

#[test]
fn parent_cascade_mid_level_gitignore_adds_patterns() {
    let root = tmp();
    fs::write(root.path().join(".gitignore"), "*.log\n").unwrap();
    let mid = root.path().join("mid");
    fs::create_dir_all(&mid).unwrap();
    fs::write(mid.join(".gitignore"), "*.tmp\n").unwrap();
    let deep = mid.join("deep");
    fs::create_dir_all(&deep).unwrap();

    let m = ProjectIgnoreMatcher::for_dir(&deep, Some(root.path())).unwrap();
    // Root pattern
    assert!(m.is_ignored(&deep.join("debug.log"), false));
    // Mid-level pattern
    assert!(m.is_ignored(&deep.join("scratch.tmp"), false));
    // Neither
    assert!(!m.is_ignored(&deep.join("main.rs"), false));
}

#[test]
fn parent_cascade_none_root_falls_back_to_dir_only() {
    let root = tmp();
    fs::write(root.path().join(".gitignore"), "dist/\n").unwrap();
    let subdir = root.path().join("subdir");
    fs::create_dir_all(&subdir).unwrap();

    // Without project_root, subdir has no ignore files → None
    assert!(ProjectIgnoreMatcher::for_dir(&subdir, None).is_none());
}

// ── nested ignore files anchor at their OWN directory ──────────────────
// Regression for the example-monorepo proto incident (2026-06-10): proto/.gitignore
// whitelists sources via slash-anchored negations. With a single
// root-anchored builder the negations resolved against the project root
// (inert) while the bare `*.proto` glob leaked tree-wide — hand-authored
// sources under proto/common/ were silently dropped from the index.

#[test]
fn nested_gitignore_negations_anchor_at_their_own_dir() {
    let root = tmp();
    let proto = root.path().join("proto");
    fs::create_dir_all(proto.join("common")).unwrap();
    fs::write(
        proto.join(".gitignore"),
        "*.proto\n!common/\n!common/**/*.proto\n",
    )
    .unwrap();

    let m = ProjectIgnoreMatcher::for_dir(&proto.join("common"), Some(root.path())).unwrap();
    // The slash-anchored negation must resolve relative to proto/ (like
    // git), re-including the nested source...
    assert!(
        !m.is_ignored(&proto.join("common/policy.proto"), false),
        "negation-whitelisted nested source must NOT be ignored"
    );
    // ...while a non-whitelisted sibling in the same dir stays excluded.
    assert!(m.is_ignored(&proto.join("scratch.proto"), false));
}

#[test]
fn nested_bare_glob_does_not_leak_outside_its_dir() {
    let root = tmp();
    let proto = root.path().join("proto");
    fs::create_dir_all(&proto).unwrap();
    fs::write(proto.join(".gitignore"), "*.proto\n").unwrap();

    let m = ProjectIgnoreMatcher::for_dir(&proto, Some(root.path())).unwrap();
    // Inside proto/ the bare glob applies...
    assert!(m.is_ignored(&proto.join("x.proto"), false));
    // ...but a .proto OUTSIDE proto/ must not be affected by it.
    assert!(
        !m.is_ignored(&root.path().join("schema.proto"), false),
        "nested bare glob must stay contained to its directory subtree"
    );
}

#[test]
fn nested_gitignore_overrides_ancestor_for_its_subtree_only() {
    let root = tmp();
    // Root excludes *.gen everywhere; vendored/ re-includes them locally.
    fs::write(root.path().join(".gitignore"), "*.gen\n").unwrap();
    let vendored = root.path().join("vendored");
    fs::create_dir_all(&vendored).unwrap();
    fs::write(vendored.join(".gitignore"), "!*.gen\n").unwrap();

    let m = ProjectIgnoreMatcher::for_dir(&vendored, Some(root.path())).unwrap();
    // Deeper layer wins inside its subtree...
    assert!(!m.is_ignored(&vendored.join("api.gen"), false));
    // ...and the ancestor rule still applies outside it.
    assert!(m.is_ignored(&root.path().join("api.gen"), false));
}

/// A directory's own `.gitignore` governs what it contains, never the
/// directory itself — git's semantics. The Laravel placeholder shape
/// (`*` + `!.gitignore`, keeping an empty dir in the repository) read the
/// directory as ignored by its own `*`, so the dequeue gate's ancestor
/// replay dropped the tracked `.gitignore` the walk kept (#402).
#[test]
fn a_directorys_own_gitignore_never_ignores_the_directory() {
    let root = tmp();
    let logs = root.path().join("storage").join("logs");
    fs::create_dir_all(&logs).unwrap();
    fs::write(logs.join(".gitignore"), "*\n!.gitignore\n").unwrap();

    let m = ProjectIgnoreMatcher::for_dir(&logs, Some(root.path())).unwrap();
    assert!(!m.is_ignored(&logs, true), "the directory itself is kept");
    assert!(!m.is_ignored(&logs.join(".gitignore"), false));
    assert!(m.is_ignored(&logs.join("laravel.log"), false));
}
