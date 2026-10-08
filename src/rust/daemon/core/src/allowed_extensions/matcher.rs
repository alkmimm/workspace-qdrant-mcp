//! The allowlist matcher, built from the compiled lists in `super::extensions`
//! (the only ingestion allowlist the daemon reads).

use std::collections::HashSet;
use std::path::Path;

use glob::Pattern;
use wqm_common::constants::COLLECTION_LIBRARIES;

use super::extensions::{
    LIBRARY_ONLY_EXTENSION_LIST, PROJECT_EXTENSION_LIST, PROJECT_FILENAME_GLOB_LIST,
    PROJECT_FILENAME_LIST,
};

/// Two-tier allowlist of files (by extension AND well-known filename) for
/// project and library ingestion.
///
/// The library set is a superset of the project set: `library_extensions ⊇ project_extensions`.
/// This allows reference material (books, papers, documentation) containing code examples
/// to be fully processed when ingested into the libraries collection.
///
/// A file is accepted when EITHER its extension is in the collection's
/// allowlist OR its file name matches the well-known filename allowlist
/// ({@link PROJECT_FILENAME_LIST} / {@link PROJECT_FILENAME_GLOB_LIST}) — so
/// extensionless build/CI files (`Dockerfile`, `Jenkinsfile`, `Makefile`) and
/// their variants are indexed. The filename allowlist applies to both
/// collections (these files are project- and library-appropriate).
#[derive(Debug, Clone)]
pub struct AllowedExtensions {
    /// Extensions allowed for project collections (source code, config, docs).
    pub(super) project_extensions: HashSet<String>,
    /// Extensions allowed for library collections (superset of project_extensions
    /// plus document/reference formats like .pdf, .epub, .docx, etc.).
    pub(super) library_extensions: HashSet<String>,
    /// Lowercased well-known file names accepted regardless of extension.
    pub(super) filenames: HashSet<String>,
    /// Compiled glob patterns (lowercased) for well-known filename variants.
    pub(super) filename_globs: Vec<Pattern>,
}

impl Default for AllowedExtensions {
    fn default() -> Self {
        let project_extensions: HashSet<String> = PROJECT_EXTENSION_LIST
            .iter()
            .map(|s| s.to_string())
            .collect();

        let library_only: HashSet<String> = LIBRARY_ONLY_EXTENSION_LIST
            .iter()
            .map(|s| s.to_string())
            .collect();

        let mut library_extensions = project_extensions.clone();
        library_extensions.extend(library_only);

        let filenames: HashSet<String> = PROJECT_FILENAME_LIST
            .iter()
            .map(|s| s.to_lowercase())
            .collect();

        // Patterns are static, lowercased, and valid globs; `filter_map` keeps
        // this panic-free (a malformed pattern would simply be skipped) in line
        // with the daemon's no-unwrap protocol.
        let filename_globs: Vec<Pattern> = PROJECT_FILENAME_GLOB_LIST
            .iter()
            .filter_map(|p| Pattern::new(&p.to_lowercase()).ok())
            .collect();

        Self {
            project_extensions,
            library_extensions,
            filenames,
            filename_globs,
        }
    }
}

/// Whether `name` is one of the well-known file names the allowlist admits by
/// exact (case-insensitive) name — `.gitignore`, `Dockerfile`, `.env.example`.
///
/// The exclusion engine consults this so a name the project deliberately
/// admits is never lost to a generic rule. Until 2026-09-19 every hidden path
/// component was excluded there, so the folder scan and the file watcher
/// dropped `.gitignore` / `.editorconfig` / `.env.example` while the startup
/// reconciler (which does not consult that engine) indexed them: such files
/// were refreshed only at a daemon restart, and a forced re-embed never
/// revisited them.
pub fn is_allowlisted_filename(name: &str) -> bool {
    static NAMES: std::sync::LazyLock<HashSet<String>> = std::sync::LazyLock::new(|| {
        PROJECT_FILENAME_LIST
            .iter()
            .map(|s| s.to_ascii_lowercase())
            .collect()
    });
    NAMES.contains(&name.to_ascii_lowercase())
}

impl AllowedExtensions {
    /// Whether the file NAME (ignoring extension) is a well-known indexable
    /// file — a build/CI/config/docs file the extension allowlist misses.
    /// Case-insensitive; checks the exact set first, then the variant globs.
    pub(super) fn filename_allowed(&self, path: &Path) -> bool {
        let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
            return false;
        };
        let lower = name.to_lowercase();
        if self.filenames.contains(&lower) {
            return true;
        }
        self.filename_globs.iter().any(|p| p.matches(&lower))
    }

    /// Check whether a file is allowed for ingestion into the given collection.
    ///
    /// Returns `true` when EITHER the file's extension (case-insensitive) is in
    /// the collection's allowlist, OR the file name matches the well-known
    /// filename allowlist (so extensionless build/CI files like `Dockerfile`,
    /// `Jenkinsfile`, `Makefile` — and variants like `Jenkinsfile_ECS` — are
    /// accepted).
    ///
    /// # Arguments
    /// * `file_path` - Absolute or relative path to the file.
    /// * `collection` - Target collection name (`"libraries"` or anything else
    ///   which falls back to the project allowlist).
    pub fn is_allowed(&self, file_path: &str, collection: &str) -> bool {
        let path = Path::new(file_path);

        if let Some(ext) = path.extension() {
            let dotted = format!(".{}", ext.to_string_lossy().to_lowercase());
            let allowed = if collection == COLLECTION_LIBRARIES {
                self.library_extensions.contains(&dotted)
            } else {
                self.project_extensions.contains(&dotted)
            };
            if allowed {
                return true;
            }
        }

        // Extension missing or not allowlisted — fall back to the well-known
        // filename allowlist so extensionless build/CI/config files index.
        self.filename_allowed(path)
    }
}
