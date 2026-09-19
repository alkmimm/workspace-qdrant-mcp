#[cfg(test)]
mod tests {
    use crate::allowed_extensions::extensions::LIBRARY_ROUTED_EXTENSIONS;
    use crate::allowed_extensions::{AllowedExtensions, FileRoute};

    #[test]
    fn test_default_has_common_project_extensions() {
        let ae = AllowedExtensions::default();
        for ext in &[
            ".rs", ".py", ".js", ".ts", ".go", ".java", ".c", ".cpp", ".md", ".toml", ".yaml",
        ] {
            assert!(
                ae.project_extensions.contains(*ext),
                "Expected project extension {} to be present",
                ext
            );
        }
    }

    #[test]
    fn test_default_has_common_library_extensions() {
        let ae = AllowedExtensions::default();
        // Library-only document formats
        for ext in &[".pdf", ".epub", ".docx", ".mobi", ".parquet"] {
            assert!(
                ae.library_extensions.contains(*ext),
                "Expected library-only extension {} to be present",
                ext
            );
        }
        // Project extensions should also be present (superset)
        for ext in &[".rs", ".py", ".js", ".md", ".txt", ".html"] {
            assert!(
                ae.library_extensions.contains(*ext),
                "Expected project extension {} to also be in library set",
                ext
            );
        }
    }

    #[test]
    fn test_is_allowed_project_source_files() {
        let ae = AllowedExtensions::default();
        assert!(ae.is_allowed("/home/user/project/src/main.rs", "projects"));
        assert!(ae.is_allowed("/home/user/project/lib.py", "projects"));
        assert!(ae.is_allowed("/home/user/project/index.ts", "projects"));
        assert!(ae.is_allowed("README.md", "projects"));
    }

    #[test]
    fn test_is_allowed_library_documents() {
        let ae = AllowedExtensions::default();
        assert!(ae.is_allowed("/docs/manual.pdf", "libraries"));
        assert!(ae.is_allowed("/docs/book.epub", "libraries"));
        assert!(ae.is_allowed("/docs/notes.md", "libraries"));
        assert!(ae.is_allowed("/docs/report.docx", "libraries"));
    }

    #[test]
    fn test_rejects_binary_and_media_files() {
        let ae = AllowedExtensions::default();
        // These should not be in either allowlist
        assert!(!ae.is_allowed("image.png", "projects"));
        assert!(!ae.is_allowed("photo.jpg", "projects"));
        assert!(!ae.is_allowed("video.mp4", "projects"));
        assert!(!ae.is_allowed("archive.zip", "projects"));
        assert!(!ae.is_allowed("binary.exe", "projects"));
        assert!(!ae.is_allowed("data.sqlite", "projects"));
        assert!(!ae.is_allowed("model.onnx", "projects"));
    }

    #[test]
    fn test_known_extensionless_files_allowed() {
        let ae = AllowedExtensions::default();
        // Well-known build/CI/config/docs files carry no extension but are the
        // heart of an infra repo — they must be indexed.
        assert!(ae.is_allowed("Makefile", "projects"));
        assert!(ae.is_allowed("Dockerfile", "projects"));
        assert!(ae.is_allowed("Jenkinsfile", "projects"));
        assert!(ae.is_allowed("LICENSE", "projects"));
        assert!(ae.is_allowed("/repo/gateway/Jenkinsfile", "projects"));
        assert!(ae.is_allowed("/home/user/.bashrc", "projects"));
        assert!(ae.is_allowed(".gitignore", "projects"));
        // Also accepted in the library set (superset).
        assert!(ae.is_allowed("Makefile", "libraries"));
    }

    #[test]
    fn test_filename_variant_globs_allowed() {
        let ae = AllowedExtensions::default();
        // The real-world reason this was filed: suffixed Jenkinsfiles and
        // Dockerfiles must index.
        assert!(ae.is_allowed("/repo/worker-command/Jenkinsfile_ECS", "projects"));
        assert!(ae.is_allowed("/repo/api-service/Jenkinsfile_dev", "projects"));
        assert!(ae.is_allowed("Dockerfile.prod", "projects"));
        assert!(ae.is_allowed("api.Dockerfile", "projects"));
        assert!(ae.is_allowed("Makefile.am", "projects"));
        // Case-insensitive.
        assert!(ae.is_allowed("DOCKERFILE", "projects"));
        assert!(ae.is_allowed("jenkinsfile_ecs", "projects"));
    }

    #[test]
    fn test_unknown_extensionless_files_rejected() {
        let ae = AllowedExtensions::default();
        // A random extensionless file with no known name stays excluded.
        assert!(!ae.is_allowed("randomfile", "projects"));
        assert!(!ae.is_allowed("/tmp/notes", "projects"));
        assert!(!ae.is_allowed("data", "projects"));
    }

    #[test]
    fn test_glob_does_not_overmatch() {
        let ae = AllowedExtensions::default();
        // Files that merely START with a well-known stem but carry an unrelated,
        // non-allowlisted extension must NOT be swallowed by the variant globs.
        assert!(!ae.is_allowed("jenkinsfileresults.log", "projects"));
        assert!(!ae.is_allowed("dockerfiles.zip", "projects"));
    }

    #[test]
    fn test_credential_files_never_indexed() {
        let ae = AllowedExtensions::default();
        // Secret-bearing files must NOT be indexed even though they are
        // extensionless / dotfiles — indexing would copy secrets into Qdrant.
        assert!(!ae.is_allowed("/repo/.env", "projects"));
        assert!(!ae.is_allowed("/home/user/.netrc", "projects"));
        assert!(!ae.is_allowed("/home/user/.npmrc", "projects"));
        assert!(!ae.is_allowed("/home/user/.ssh/id_rsa", "projects"));
    }

    #[test]
    fn test_case_insensitive_matching() {
        let ae = AllowedExtensions::default();
        assert!(ae.is_allowed("file.RS", "projects"));
        assert!(ae.is_allowed("file.Py", "projects"));
        assert!(ae.is_allowed("file.PDF", "libraries"));
        assert!(ae.is_allowed("FILE.Html", "libraries"));
    }

    #[test]
    fn test_library_collection_uses_library_set() {
        let ae = AllowedExtensions::default();
        // .pdf is in library but not project
        assert!(ae.is_allowed("doc.pdf", "libraries"));
        assert!(!ae.is_allowed("doc.pdf", "projects"));
    }

    #[test]
    fn test_project_extensions_allowed_in_libraries() {
        let ae = AllowedExtensions::default();
        // Project extensions like .rs are now also in library set (superset)
        assert!(ae.is_allowed("main.rs", "projects"));
        assert!(ae.is_allowed("main.rs", "libraries"));
    }

    #[test]
    fn test_unknown_collection_falls_back_to_project() {
        let ae = AllowedExtensions::default();
        // Any collection name other than "libraries" uses project set
        assert!(ae.is_allowed("main.rs", "some_custom_collection"));
        assert!(!ae.is_allowed("doc.pdf", "some_custom_collection"));
    }

    #[test]
    fn test_empty_path() {
        let ae = AllowedExtensions::default();
        assert!(!ae.is_allowed("", "projects"));
    }

    #[test]
    fn test_dot_only_extension() {
        let ae = AllowedExtensions::default();
        // A file like "file." has an empty extension
        assert!(!ae.is_allowed("file.", "projects"));
    }

    #[test]
    fn test_r_case_sensitivity() {
        let ae = AllowedExtensions::default();
        // Both .r and .R should work via case-insensitive matching
        assert!(ae.is_allowed("analysis.r", "projects"));
        assert!(ae.is_allowed("analysis.R", "projects"));
    }

    #[test]
    fn test_shared_extensions_between_project_and_library() {
        let ae = AllowedExtensions::default();
        // .md and .txt are in both sets
        assert!(ae.is_allowed("notes.md", "projects"));
        assert!(ae.is_allowed("notes.md", "libraries"));
        assert!(ae.is_allowed("readme.txt", "projects"));
        assert!(ae.is_allowed("readme.txt", "libraries"));
    }

    #[test]
    fn test_path_with_dots_in_directory() {
        let ae = AllowedExtensions::default();
        // Directories with dots should not confuse extension extraction
        assert!(ae.is_allowed("/home/user/my.project/src/main.rs", "projects"));
        assert!(!ae.is_allowed("/home/user/my.project/src/data.bin", "projects"));
    }

    #[test]
    fn test_library_extensions_is_superset_of_project() {
        let ae = AllowedExtensions::default();
        // Every project extension must also be in library extensions
        for ext in &ae.project_extensions {
            assert!(
                ae.library_extensions.contains(ext),
                "Project extension {} missing from library set (superset violation)",
                ext
            );
        }
    }

    #[test]
    fn test_library_only_extensions_rejected_for_projects() {
        let ae = AllowedExtensions::default();
        // Document/reference formats should not be allowed in projects
        for path in &[
            "doc.pdf",
            "book.epub",
            "report.docx",
            "novel.mobi",
            "slides.pptx",
            "data.parquet",
            "budget.xlsx",
        ] {
            assert!(
                !ae.is_allowed(path, "projects"),
                "Library-only file {} should be rejected for projects",
                path
            );
            assert!(
                ae.is_allowed(path, "libraries"),
                "Library-only file {} should be accepted for libraries",
                path
            );
        }
    }

    #[test]
    fn test_case_insensitive_library_only_extensions() {
        let ae = AllowedExtensions::default();
        assert!(ae.is_allowed("doc.PDF", "libraries"));
        assert!(ae.is_allowed("book.EPUB", "libraries"));
        assert!(ae.is_allowed("report.DOCX", "libraries"));
    }

    // --- FileRoute / route_file() tests ---

    #[test]
    fn test_route_source_file_in_project() {
        let ae = AllowedExtensions::default();
        assert_eq!(
            ae.route_file("/project/src/main.rs", "projects", "my-project"),
            FileRoute::ProjectCollection
        );
        assert_eq!(
            ae.route_file("lib.py", "projects", "my-project"),
            FileRoute::ProjectCollection
        );
        assert_eq!(
            ae.route_file("index.ts", "projects", "my-project"),
            FileRoute::ProjectCollection
        );
    }

    #[test]
    fn test_route_pdf_in_project_goes_to_library() {
        let ae = AllowedExtensions::default();
        let route = ae.route_file("/project/docs/manual.pdf", "projects", "my-project");
        assert_eq!(
            route,
            FileRoute::LibraryCollection {
                source_project_id: Some("my-project".to_string())
            }
        );
    }

    #[test]
    fn test_route_docx_in_project_goes_to_library() {
        let ae = AllowedExtensions::default();
        let route = ae.route_file("report.docx", "projects", "my-project");
        assert_eq!(
            route,
            FileRoute::LibraryCollection {
                source_project_id: Some("my-project".to_string())
            }
        );
    }

    #[test]
    fn test_route_all_library_routed_extensions_in_project() {
        let ae = AllowedExtensions::default();
        for ext in LIBRARY_ROUTED_EXTENSIONS {
            let filename = format!("file{}", ext);
            let route = ae.route_file(&filename, "projects", "proj-1");
            assert_eq!(
                route,
                FileRoute::LibraryCollection {
                    source_project_id: Some("proj-1".to_string())
                },
                "Extension {} in project should route to LibraryCollection",
                ext
            );
        }
    }

    #[test]
    fn test_route_source_file_in_library() {
        let ae = AllowedExtensions::default();
        // .rs is in the library set (superset), so it's allowed in library folders
        assert_eq!(
            ae.route_file("main.rs", "libraries", "my-lib"),
            FileRoute::LibraryCollection {
                source_project_id: None
            }
        );
        assert_eq!(
            ae.route_file("example.py", "libraries", "my-lib"),
            FileRoute::LibraryCollection {
                source_project_id: None
            }
        );
    }

    #[test]
    fn test_route_pdf_in_library() {
        let ae = AllowedExtensions::default();
        assert_eq!(
            ae.route_file("book.pdf", "libraries", "my-lib"),
            FileRoute::LibraryCollection {
                source_project_id: None
            }
        );
    }

    #[test]
    fn test_route_binary_file_excluded() {
        let ae = AllowedExtensions::default();
        assert_eq!(
            ae.route_file("image.png", "projects", "proj"),
            FileRoute::Excluded
        );
        assert_eq!(
            ae.route_file("photo.jpg", "libraries", "lib"),
            FileRoute::Excluded
        );
        assert_eq!(
            ae.route_file("archive.zip", "projects", "proj"),
            FileRoute::Excluded
        );
    }

    #[test]
    fn test_route_extensionless_known_files() {
        let ae = AllowedExtensions::default();
        // Known build/CI files route to the project collection (text/code,
        // never binary documents), and to the library collection under a
        // library watch.
        assert_eq!(
            ae.route_file("Makefile", "projects", "proj"),
            FileRoute::ProjectCollection
        );
        assert_eq!(
            ae.route_file("/repo/worker-command/Jenkinsfile_ECS", "projects", "proj"),
            FileRoute::ProjectCollection
        );
        assert_eq!(
            ae.route_file("Dockerfile", "libraries", "lib"),
            FileRoute::LibraryCollection {
                source_project_id: None
            }
        );
        // An unknown extensionless file is still excluded.
        assert_eq!(
            ae.route_file("randomfile", "projects", "proj"),
            FileRoute::Excluded
        );
    }

    #[test]
    fn test_route_case_insensitive() {
        let ae = AllowedExtensions::default();
        assert_eq!(
            ae.route_file("file.RS", "projects", "proj"),
            FileRoute::ProjectCollection
        );
        assert_eq!(
            ae.route_file("doc.PDF", "projects", "proj"),
            FileRoute::LibraryCollection {
                source_project_id: Some("proj".to_string())
            }
        );
        assert_eq!(
            ae.route_file("doc.PDF", "libraries", "lib"),
            FileRoute::LibraryCollection {
                source_project_id: None
            }
        );
    }

    #[test]
    fn test_route_md_stays_in_project() {
        // .md is a project extension, not in LIBRARY_ROUTED_EXTENSIONS,
        // so it stays in the project collection when found in project folders
        let ae = AllowedExtensions::default();
        assert_eq!(
            ae.route_file("README.md", "projects", "proj"),
            FileRoute::ProjectCollection
        );
    }

    #[test]
    fn test_route_empty_path() {
        let ae = AllowedExtensions::default();
        assert_eq!(ae.route_file("", "projects", "proj"), FileRoute::Excluded);
    }

    #[test]
    fn test_route_unknown_collection_uses_project_logic() {
        let ae = AllowedExtensions::default();
        // Non-"libraries" collections fall through to project logic
        assert_eq!(
            ae.route_file("main.rs", "custom", "proj"),
            FileRoute::ProjectCollection
        );
        assert_eq!(
            ae.route_file("doc.pdf", "custom", "proj"),
            FileRoute::LibraryCollection {
                source_project_id: Some("proj".to_string())
            }
        );
    }

    /// Every extension the bundled language registry knows must pass the
    /// project allowlist. Until 2026-09-19 the daemon shipped grammars for 24
    /// languages whose files this gate rejected (C++ .cc/.cxx/.hh, Kotlin
    /// .kts, Julia .jl, Ada, Lisp, Fortran, Pascal, Scheme, …): the registry
    /// said "supported", the walk never queued them, and native grep found
    /// what the index could not. `.fasl` (a compiled Lisp image, binary) is
    /// the one registry entry excluded on purpose.
    #[tokio::test]
    async fn registry_extensions_are_all_allowlisted() {
        use crate::language_registry::providers::registry::RegistryProvider;
        use crate::language_registry::LanguageRegistry;

        let mut registry = LanguageRegistry::new();
        registry.register_provider(Box::new(RegistryProvider::new().unwrap()));
        registry.load().await.unwrap();
        let languages = registry.all().await;
        assert!(
            languages.len() >= 40,
            "bundled registry loaded {}",
            languages.len()
        );

        const BINARY_BY_DESIGN: &[&str] = &[".fasl"];
        let ae = AllowedExtensions::default();
        let mut rejected = Vec::new();
        let mut checked = 0usize;
        for (id, lang) in &languages {
            for ext in &lang.extensions {
                let dotted = if ext.starts_with('.') {
                    ext.to_lowercase()
                } else {
                    format!(".{}", ext.to_lowercase())
                };
                if BINARY_BY_DESIGN.contains(&dotted.as_str()) {
                    continue;
                }
                checked += 1;
                if !ae.is_allowed(&format!("/repo/src/probe{dotted}"), "projects") {
                    rejected.push(format!("{id}: {dotted}"));
                }
            }
        }
        assert!(checked > 100, "registry exposed only {checked} extensions");
        assert!(
            rejected.is_empty(),
            "language_registry.yaml extensions the project allowlist rejects — add them to \
             PROJECT_EXTENSION_LIST in allowed_extensions/extensions.rs:\n  {}",
            rejected.join("\n  ")
        );
    }

    /// assets/default_configuration.yaml documents `watching.allowed_extensions`
    /// as THE ingestion gate, but the daemon never reads that list: every
    /// ingest path uses `AllowedExtensions::default()`. The YAML is therefore a
    /// generated mirror (scripts/gen-allowlist-yaml.py) and this test is what
    /// keeps it honest — on 2026-09-19 it promised 355 extensions against 91
    /// compiled (.conf, .properties, .cc, .kts, man-page `.1`/`.5`, …), 500
    /// git-tracked files across the watched repos that `make coverage-audit`
    /// reported as "promised but never indexed".
    #[test]
    fn default_configuration_yaml_mirrors_the_compiled_allowlist() {
        let yaml: serde_yaml_ng::Value = serde_yaml_ng::from_str(include_str!(
            "../../../../../../assets/default_configuration.yaml"
        ))
        .expect("default_configuration.yaml parses");
        let list = |key: &str| -> std::collections::HashSet<String> {
            yaml["watching"][key]
                .as_sequence()
                .unwrap_or_else(|| panic!("watching.{key} is a list"))
                .iter()
                .map(|v| v.as_str().expect("string entry").to_lowercase())
                .collect()
        };
        let ae = AllowedExtensions::default();

        let yaml_exts = list("allowed_extensions");
        let compiled_exts: std::collections::HashSet<String> = ae
            .project_extensions
            .iter()
            .map(|e| e.to_lowercase())
            .collect();
        let only_yaml: Vec<_> = yaml_exts.difference(&compiled_exts).collect();
        let only_rust: Vec<_> = compiled_exts.difference(&yaml_exts).collect();
        assert!(
            only_yaml.is_empty() && only_rust.is_empty(),
            "watching.allowed_extensions drifted from PROJECT_EXTENSION_LIST — run \
             scripts/gen-allowlist-yaml.py. only in yaml: {only_yaml:?}; only in rust: {only_rust:?}"
        );

        let yaml_names = list("allowed_filenames");
        let compiled_names: std::collections::HashSet<String> =
            ae.filenames.iter().map(|n| n.to_lowercase()).collect();
        let only_yaml: Vec<_> = yaml_names.difference(&compiled_names).collect();
        let only_rust: Vec<_> = compiled_names.difference(&yaml_names).collect();
        assert!(
            only_yaml.is_empty() && only_rust.is_empty(),
            "watching.allowed_filenames drifted from PROJECT_FILENAME_LIST — run \
             scripts/gen-allowlist-yaml.py. only in yaml: {only_yaml:?}; only in rust: {only_rust:?}"
        );
    }
}
