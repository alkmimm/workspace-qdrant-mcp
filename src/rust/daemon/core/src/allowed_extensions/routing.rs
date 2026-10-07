//! Where a file goes: the format-based routing every path that admits or
//! enqueues a file shares.
//!
//! [`AllowedExtensions::route_file`] is the one answer to "may this file be
//! indexed, and into which collection". [`AllowedExtensions::is_indexable`] is
//! its yes/no form — the eligibility predicate. It is NOT
//! [`AllowedExtensions::is_allowed`] for a project folder: a library-format
//! document there (`.pdf`, `.docx`, `.odt`, …) is indexed into the project's
//! `-refs` library while `is_allowed(path, "projects")` refuses it.

use std::path::Path;

use wqm_common::constants::COLLECTION_LIBRARIES;

use super::extensions::AllowedExtensions;
use super::types::FileRoute;

/// Extensions for binary/reference formats that route to the `libraries` collection
/// even when discovered inside a project folder.
///
/// These are document formats (PDF, EPUB, etc.) that are unlikely to be "source code"
/// and are better served by the library ingestion pipeline. Source-like formats
/// (e.g., `.md`, `.txt`, `.html`) stay in `projects` because they are typically
/// project documentation meant to be searched alongside code.
///
/// `.key` is deliberately absent although Keynote uses it: inside a source
/// repository a `.key` file is a PEM private key (bws-engineer holds three on
/// 2026-10-05 — `ca/server.key`, `elasticsearch/client.key`), and routing it
/// would copy key material into the library collection. A watch folder
/// registered as a library still admits it (`LIBRARY_ONLY_EXTENSION_LIST`).
pub(super) const LIBRARY_ROUTED_EXTENSIONS: &[&str] = &[
    ".pdf", ".epub", ".docx", ".doc", ".rtf", ".odt", ".mobi", ".chm", ".pptx", ".ppt", ".pages",
    ".odp", ".xlsx", ".xls", ".ods", ".numbers", ".parquet",
];

/// Whether a file found inside a PROJECT folder is a library-format document,
/// stored in the project's `-refs` library rather than in `projects`.
pub fn is_library_routed(file_path: &str) -> bool {
    Path::new(file_path)
        .extension()
        .map(|ext| format!(".{}", ext.to_string_lossy().to_lowercase()))
        .is_some_and(|dotted| LIBRARY_ROUTED_EXTENSIONS.contains(&dotted.as_str()))
}

impl AllowedExtensions {
    /// Route a file to the appropriate Qdrant collection based on its extension
    /// and the watch folder's configured collection.
    ///
    /// # Routing logic
    ///
    /// 1. **Library watch folders** (`watch_collection == "libraries"`):
    ///    Files with extensions in the library allowlist route to `LibraryCollection`.
    ///    All others are `Excluded`.
    ///
    /// 2. **Project watch folders** (`watch_collection == "projects"`):
    ///    - If the extension is in `LIBRARY_ROUTED_EXTENSIONS` (binary document formats
    ///      like `.pdf`, `.docx`, `.epub`), the file routes to `LibraryCollection` with
    ///      `source_project_id` set to the project's tenant_id, so the library entry
    ///      can be traced back to its origin project.
    ///    - If the extension is in the project allowlist, it routes to `ProjectCollection`.
    ///    - Otherwise, the file is `Excluded`.
    ///
    /// # Arguments
    /// * `file_path` - Path to the file being routed.
    /// * `watch_collection` - The collection configured on the watch folder (`"projects"` or `"libraries"`).
    /// * `tenant_id` - The tenant identifier (project ID or library name) for the watch folder.
    pub fn route_file(
        &self,
        file_path: &str,
        watch_collection: &str,
        tenant_id: &str,
    ) -> FileRoute {
        let path = Path::new(file_path);
        let ext_dotted = path
            .extension()
            .map(|ext| format!(".{}", ext.to_string_lossy().to_lowercase()));

        if watch_collection == COLLECTION_LIBRARIES {
            // Library watch folder: accept any library-allowed extension or a
            // well-known filename.
            if ext_dotted
                .as_ref()
                .is_some_and(|d| self.library_extensions.contains(d))
                || self.filename_allowed(path)
            {
                return FileRoute::LibraryCollection {
                    source_project_id: None,
                };
            }
            return FileRoute::Excluded;
        }

        // Project watch folder: check for library-routed override first, then
        // the project extension allowlist.
        if is_library_routed(file_path) {
            return FileRoute::LibraryCollection {
                source_project_id: Some(tenant_id.to_string()),
            };
        }
        if ext_dotted
            .as_ref()
            .is_some_and(|d| self.project_extensions.contains(d))
        {
            return FileRoute::ProjectCollection;
        }

        // Extension missing or not allowlisted — well-known build/CI/config/docs
        // files (text/code, never binary documents) route to the project
        // collection so Dockerfiles, Jenkinsfiles and Makefiles get indexed.
        if self.filename_allowed(path) {
            return FileRoute::ProjectCollection;
        }

        FileRoute::Excluded
    }

    /// Whether a file in a watch folder of `watch_collection` is indexable at
    /// all — `route_file(..) != Excluded`.
    ///
    /// This is the eligibility every path that decides "should this file be in
    /// the index" must share: the folder scan, the startup reconciler
    /// (`eligible_walk::retain_indexable`), the exclusion cleanup, worktree
    /// membership and the branch tip follower. Until 2026-10-05 all but the
    /// reconciler and the file watcher asked [`Self::is_allowed`] with the
    /// folder's collection, so a project's `.docx` was ineligible there: worktree
    /// branches and branches without a checkout never followed it, and the
    /// exclusion cleanup queued its deletion.
    pub fn is_indexable(&self, file_path: &str, watch_collection: &str) -> bool {
        // The tenant only labels a library route; it never decides one.
        !matches!(
            self.route_file(file_path, watch_collection, ""),
            FileRoute::Excluded
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use wqm_common::constants::COLLECTION_PROJECTS;

    #[test]
    fn a_project_folder_indexes_its_documents_through_the_library() {
        let ae = AllowedExtensions::default();
        for doc in ["docs/guide.docx", "spec.PDF", "manual.odt", "deck.pptx"] {
            assert!(ae.is_indexable(doc, COLLECTION_PROJECTS), "{doc}");
            assert!(is_library_routed(doc), "{doc}");
            // The project allowlist alone refuses them — the predicate the
            // worktree / scan / cleanup paths used to ask.
            assert!(!ae.is_allowed(doc, COLLECTION_PROJECTS), "{doc}");
        }
        assert!(ae.is_indexable("src/main.rs", COLLECTION_PROJECTS));
        assert!(!is_library_routed("src/main.rs"));
        assert!(ae.is_indexable("Dockerfile", COLLECTION_PROJECTS));
        assert!(!ae.is_indexable("image.png", COLLECTION_PROJECTS));
    }

    /// Live 2026-10-05: bws-engineer holds three PEM private keys named `*.key`.
    #[test]
    fn a_private_key_in_a_project_folder_is_never_indexable() {
        let ae = AllowedExtensions::default();
        let key = "api-service/src/main/resources/ca/server.key";
        assert!(!is_library_routed(key));
        assert!(!ae.is_indexable(key, COLLECTION_PROJECTS));
        assert_eq!(
            ae.route_file(key, COLLECTION_PROJECTS, "t"),
            FileRoute::Excluded
        );
        // A folder registered as a library keeps Keynote decks.
        assert!(ae.is_indexable("talk.key", COLLECTION_LIBRARIES));
    }
}
