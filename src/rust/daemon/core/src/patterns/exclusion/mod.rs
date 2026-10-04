//! Advanced exclusion rules system for build artifacts and unwanted files
//!
//! This module provides sophisticated file exclusion capabilities using the
//! comprehensive configuration data. Optimized for performance with multi-tier
//! filtering and context-aware exclusion logic.

use std::path::Path;

mod engine;
pub(crate) mod helpers;
#[cfg(test)]
mod tests;

pub use engine::ExclusionEngine;

/// Exclusion rule categories with different priorities
#[derive(Debug, Clone)]
pub enum ExclusionCategory {
    /// Critical system files that should never be processed
    Critical,
    /// Build artifacts and generated files
    BuildArtifacts,
    /// Cache directories and temporary files
    Cache,
    /// Version control metadata
    VersionControl,
    /// IDE and editor files
    IdeFiles,
    /// Media and binary files
    Media,
    /// Security sensitive files
    Security,
}

/// Exclusion rule with metadata
#[derive(Debug, Clone)]
pub struct ExclusionRule {
    pub pattern: String,
    pub category: ExclusionCategory,
    pub reason: String,
    pub is_regex: bool,
    pub case_sensitive: bool,
}

/// Result of exclusion checking
#[derive(Debug, Clone)]
pub struct ExclusionResult {
    pub excluded: bool,
    pub rule: Option<ExclusionRule>,
    pub reason: String,
}

/// Exclusion engine statistics
#[derive(Debug, Clone)]
pub struct ExclusionStats {
    pub total_rules: usize,
    pub exact_matches: usize,
    pub prefix_patterns: usize,
    pub suffix_patterns: usize,
    pub contains_patterns: usize,
    pub category_counts: std::collections::HashMap<String, usize>,
}

/// Whether the compiled exclusion engine drops `file_path`, honouring the
/// directories `global.wqmignore` explicitly re-includes.
///
/// Every gate that decides eligibility calls this — the git and FS scans, the
/// watcher, startup recovery, post-scan cleanup, libraries — while the ignore
/// reconciler reads `global.wqmignore` directly. When the engine's `out` token
/// ignored the file's `!**/src/**/out/` negation the two disagreed: ~440
/// hexagonal `ports/out` sources were indexed by the reconciler yet skipped by
/// the scan and the watcher (so edits never reached the index) and flagged
/// "excluded" by recovery on every restart. `global.wqmignore` is now the one
/// place a directory is re-included.
///
/// The re-inclusion is judged on a path RELATIVE to its project root: the
/// global matcher is anchored at `/`, so on an absolute path a `src` segment
/// ABOVE the project (repositories cloned under `~/src/`) would make
/// `!**/src/**/target/` re-include real build output. Without a root, only a
/// relative `file_path` (startup recovery, post-scan cleanup) gets the
/// re-inclusion; absolute callers use [`should_exclude_file_in`].
pub fn should_exclude_file(file_path: &str) -> bool {
    should_exclude_file_using(file_path, &|dir| {
        let dir = Path::new(dir);
        dir.is_relative() && crate::patterns::global_ignore::is_globally_whitelisted(dir, true)
    })
}

/// [`should_exclude_file`] for a path inside the project (or library) rooted
/// at `root`, with the re-inclusion judged relative to `root`.
pub fn should_exclude_file_in(root: &Path, file_path: &str) -> bool {
    should_exclude_file_using(file_path, &|dir| is_reincluded_dir(root, Path::new(dir)))
}

/// Whether `global.wqmignore` explicitly re-includes directory `dir`, judged on
/// its path relative to `root` (a relative `dir` already is). A directory at or
/// above the root, or outside it, is never re-included. Shared by the file
/// checks and by the directory-pruning walks (folder scan, library walk).
pub fn is_reincluded_dir(root: &Path, dir: &Path) -> bool {
    is_reincluded_dir_with(root, dir, &|rel| {
        crate::patterns::global_ignore::is_globally_whitelisted(rel, true)
    })
}

/// [`is_reincluded_dir`] with the whitelist source injected (tests).
pub(crate) fn is_reincluded_dir_with(
    root: &Path,
    dir: &Path,
    whitelisted: &dyn Fn(&Path) -> bool,
) -> bool {
    let rel = if dir.is_relative() {
        dir
    } else {
        match dir.strip_prefix(root) {
            Ok(rel) => rel,
            Err(_) => return false,
        }
    };
    !rel.as_os_str().is_empty() && whitelisted(rel)
}

/// [`should_exclude_file`] with the re-inclusion source injected (tests).
pub(crate) fn should_exclude_file_using(
    file_path: &str,
    reincluded_dir: &dyn Fn(&str) -> bool,
) -> bool {
    match ExclusionEngine::global() {
        Ok(engine) => {
            engine
                .should_exclude_with(file_path, reincluded_dir)
                .excluded
        }
        Err(_) => false, // If engine fails to initialize, don't exclude anything
    }
}

/// Check if a directory should be skipped entirely during filesystem walks.
///
/// Uses the existing exclusion engine by testing if a synthetic file path
/// under this directory would be excluded. This allows WalkDir's `filter_entry`
/// to skip entire subtrees (e.g., target/, node_modules/, .git/) without
/// enumerating their contents.
pub fn should_exclude_directory(dir_name: &str) -> bool {
    // .github is explicitly whitelisted — never skip it
    if dir_name == ".github" {
        return false;
    }
    // Hidden directories (start with '.') are always excluded
    if dir_name.starts_with('.') {
        return true;
    }
    // Check if the exclusion engine would exclude files under this directory
    match ExclusionEngine::global() {
        Ok(engine) => {
            let synthetic_path = format!("{}/placeholder.txt", dir_name);
            engine.should_exclude(&synthetic_path).excluded
        }
        Err(_) => false,
    }
}

/// Convenient function for contextual exclusion checking
pub fn should_exclude_file_with_context(file_path: &str, project_type: &str) -> ExclusionResult {
    match ExclusionEngine::global() {
        Ok(engine) => engine.check_with_context(file_path, Some(project_type)),
        Err(e) => ExclusionResult {
            excluded: false,
            rule: None,
            reason: format!("Engine initialization failed: {}", e),
        },
    }
}
