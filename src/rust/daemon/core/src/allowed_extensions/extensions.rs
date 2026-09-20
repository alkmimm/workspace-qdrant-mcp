use std::collections::HashSet;
use std::path::Path;

use glob::Pattern;
use wqm_common::constants::COLLECTION_LIBRARIES;

use super::types::FileRoute;

/// Extensions for binary/reference formats that route to the `libraries` collection
/// even when discovered inside a project folder.
///
/// These are document formats (PDF, EPUB, etc.) that are unlikely to be "source code"
/// and are better served by the library ingestion pipeline. Source-like formats
/// (e.g., `.md`, `.txt`, `.html`) stay in `projects` because they are typically
/// project documentation meant to be searched alongside code.
pub(super) const LIBRARY_ROUTED_EXTENSIONS: &[&str] = &[
    ".pdf", ".epub", ".docx", ".doc", ".rtf", ".odt", ".mobi", ".chm", ".pptx", ".ppt", ".pages",
    ".key", ".odp", ".xlsx", ".xls", ".ods", ".numbers", ".parquet",
];

/// Source code, config, and documentation extensions allowed in project collections.
const PROJECT_EXTENSION_LIST: &[&str] = &[
    // Rust
    ".rs",
    // Python
    ".py",
    // JavaScript / TypeScript
    ".js",
    ".ts",
    ".tsx",
    ".jsx",
    ".mjs",
    ".cjs",
    ".mts",
    ".cts",
    // Go
    ".go",
    // Java / JVM
    ".java",
    ".kt",
    ".scala",
    ".groovy",
    ".clj",
    ".cljs",
    // C / C++
    ".c",
    ".cpp",
    ".h",
    ".hpp",
    // Swift
    ".swift",
    // Ruby
    ".rb",
    // Lua
    ".lua",
    // Shell
    ".sh",
    ".bash",
    ".zsh",
    ".fish",
    // Config / Data
    ".toml",
    ".yaml",
    ".yml",
    ".json",
    ".xml",
    // Spreadsheets and data
    ".csv",
    ".tsv",
    // Notebooks
    ".ipynb",
    // Web
    ".html",
    ".css",
    ".scss",
    ".less",
    ".vue",
    ".svelte",
    ".astro",
    // SQL / GraphQL / Proto
    ".sql",
    ".graphql",
    ".proto",
    // Documentation
    ".md",
    ".txt",
    ".rst",
    ".tex",
    // Elixir / Erlang
    ".ex",
    ".exs",
    ".erl",
    ".hrl",
    // Haskell / ML / Elm
    ".hs",
    ".ml",
    ".mli",
    ".elm",
    // R (.r and .R kept separate for case-insensitive matching)
    ".r",
    ".R",
    // Dart
    ".dart",
    // .NET
    ".cs",
    ".fs",
    ".vb",
    // Perl / PHP
    ".pl",
    ".pm",
    ".php",
    // Nix
    ".nix",
    // Lean
    ".lean",
    // Zig
    ".zig",
    // Nim
    ".nim",
    // V / Odin / D
    ".v",
    ".odin",
    ".d",
    // Fortran
    ".f90",
    ".f95",
    // Pascal
    ".pas",
    // COBOL
    ".cob",
    ".cbl",
    // Build / CI files (by extension)
    ".dockerfile",
    ".makefile",
    ".cmake",
    ".mk",
    // PowerShell / Batch
    ".ps1",
    ".bat",
    ".cmd",
    // Text processing
    ".awk",
    ".sed",
    // Build tool configs
    ".sbt",
    ".gradle",
    ".pom",
    // ── Registry parity (2026-09-19) ─────────────────────────────────────
    // Every extension of a language in language_registry.yaml belongs here:
    // the daemon shipped grammars for 24 languages whose files this gate
    // rejected (C++ .cc/.cxx/.hh, Kotlin .kts, Julia .jl, Ada, Lisp, Fortran,
    // Pascal, Scheme, …) — an agent found them with native grep and the index
    // had never seen them. `registry_extensions_are_all_allowlisted` keeps
    // the two lists in step; `.fasl` (compiled Lisp image, a binary) is the
    // one registry entry left out on purpose.
    ".adb",
    ".ads", // Ada
    ".cljc",
    ".edn", // Clojure
    ".c++",
    ".cc",
    ".cxx",
    ".h++",
    ".hh",
    ".hxx",
    ".ipp",
    ".tpp", // C++
    ".f",
    ".f03",
    ".f08",
    ".for",
    ".fpp", // Fortran
    ".lhs", // Haskell (literate)
    ".htm",
    ".xhtml", // HTML
    ".jsonc", // JSON with comments
    ".jl",    // Julia
    ".kts",   // Kotlin script / Gradle Kotlin DSL
    ".cls",
    ".sty", // LaTeX
    ".cl",
    ".lisp",
    ".lsp", // Lisp
    ".markdown",
    ".mdx", // Markdown
    ".mll",
    ".mly", // OCaml lexers / parsers
    ".dpk",
    ".dpr",
    ".lfm",
    ".pp", // Pascal
    ".pod",
    ".psgi",
    ".t", // Perl
    ".php3",
    ".php4",
    ".php5",
    ".php7",
    ".phps",
    ".phtml", // PHP
    ".psd1",
    ".psm1", // PowerShell
    ".pyi",
    ".pyw", // Python
    ".rmd",
    ".rnw", // R
    ".gemspec",
    ".rake",
    ".rbw", // Ruby
    ".sc",  // Scala
    ".rkt",
    ".scm",
    ".ss", // Scheme
    ".vala",
    ".vapi", // Vala
    ".xsd",
    ".xsl",
    ".xslt", // XML
    // ── Source-like formats the coverage audit found unindexed ───────────
    // (`make coverage-audit`, 2026-09-19: 500 git-tracked files across nine
    // repos were promised by default_configuration.yaml and rejected here.)
    ".tf",
    ".tfvars",
    ".hcl", // Terraform / HCL
    ".jinja",
    ".jinja2",
    ".j2",
    ".hbs", // templates
    ".plist",
    ".xcconfig",
    ".pbxproj",
    ".storyboard",
    ".xib",
    ".entitlements", // Xcode
    ".service",
    ".timer", // systemd units
    ".patch",
    ".diff", // diffs
    // ── Configuration formats, admitted with credential redaction ────────
    // 16 of 130 .conf and 17 of 55 .properties in the audited repos carry
    // password= / secret= / token= lines (Spring application.properties,
    // keycloak.conf). They are indexed because document_processor::redaction
    // masks the VALUE of every credential-named key before chunking, so the
    // vectors, the payload and the FTS5 lines hold `password=<redacted>` and
    // a reader still learns the setting exists. `.env*` stays out: a file that
    // is nothing but secrets is not worth a regex's residual risk (the
    // `.env.example` / `.sample` / `.template` / `.dist` names, which hold
    // placeholders by convention, are admitted by exact name below).
    ".properties",
    ".conf",
    ".cfg",
    ".ini",
    ".cnf",
];

/// Document/reference formats added only to the library allowlist.
/// library_extensions = project_extensions ∪ LIBRARY_ONLY_EXTENSION_LIST
const LIBRARY_ONLY_EXTENSION_LIST: &[&str] = &[
    // Documents
    ".pdf", ".epub", ".docx", ".doc", ".rtf", ".odt", // Ebooks
    ".mobi", ".chm", // Presentations
    ".pptx", ".ppt", ".pages", ".key", ".odp",
    // Spreadsheets (formats not already in project set)
    ".xlsx", ".xls", ".ods", ".numbers", ".parquet",
    // Web (variant not in project set)
    ".htm",
];

/// Well-known code-adjacent files that carry NO usable extension (or an
/// extension that is not itself an allowlisted language). Matched against the
/// whole file name, case-insensitively. This is the Linguist "filenames" set
/// for build/CI/config/docs files — the heart of an infra repo (Dockerfiles,
/// Jenkinsfiles, Makefiles) that the extension allowlist alone is blind to.
///
/// Deliberately EXCLUDES credential-bearing dotfiles (`.env`, `.netrc`,
/// `.npmrc`, `.pypirc`, `id_rsa`, ...): indexing those would copy secrets into
/// the vector store. Keep this list to files whose content is safe to search.
const PROJECT_FILENAME_LIST: &[&str] = &[
    // Containers
    "Dockerfile",
    "Containerfile",
    // Make / build systems
    "Makefile",
    "GNUmakefile",
    "BSDmakefile",
    "justfile",
    "Kbuild",
    "SConstruct",
    "SConscript",
    "wscript",
    "meson.build",
    "BUILD",
    "WORKSPACE",
    "Taskfile",
    // CI / orchestration
    "Jenkinsfile",
    "Vagrantfile",
    "Procfile",
    "Caddyfile",
    "Earthfile",
    "Tiltfile",
    // Ruby / package-manager manifests (extensionless by convention)
    "Rakefile",
    "Gemfile",
    "Guardfile",
    "Capfile",
    "Berksfile",
    "Brewfile",
    "Podfile",
    "Fastfile",
    "Appfile",
    "Deliverfile",
    "Snapfile",
    "Thorfile",
    "Dangerfile",
    "Cheffile",
    "Puppetfile",
    "Pipfile",
    // Go / Rust manifests whose extension is not a language of its own
    "go.mod",
    "go.sum",
    // Environment TEMPLATES only — placeholders by convention, and redaction
    // masks anything real that slips in. `.env`, `.env.local`,
    // `.env.production` and friends are never listed: see the note above.
    ".env.example",
    ".env.sample",
    ".env.template",
    ".env.dist",
    // Git / tooling ignore + config files (safe to index, no secrets)
    ".gitignore",
    ".gitattributes",
    ".gitmodules",
    ".gitconfig",
    ".mailmap",
    ".dockerignore",
    ".npmignore",
    ".eslintignore",
    ".prettierignore",
    ".editorconfig",
    ".nvmrc",
    ".babelrc",
    ".eslintrc",
    ".prettierrc",
    ".stylelintrc",
    ".browserslistrc",
    // Project-level MCP client configuration (which servers a repo wires up).
    // Its `env` blocks can hold credentials; the key/value redaction layer
    // masks them, so the file is admitted like any other JSON.
    ".mcp.json",
    ".bashrc",
    ".bash_profile",
    ".bash_logout",
    ".profile",
    ".zshrc",
    ".zprofile",
    ".zshenv",
    ".inputrc",
    "CODEOWNERS",
    // Common extensionless project docs
    "README",
    "LICENSE",
    "LICENCE",
    "COPYING",
    "COPYRIGHT",
    "NOTICE",
    "AUTHORS",
    "CONTRIBUTORS",
    "CHANGELOG",
    "CHANGES",
    "INSTALL",
    "MAINTAINERS",
    "TODO",
    "NEWS",
    "HACKING",
];

/// Glob patterns for filename VARIANTS of the same well-known files — the
/// suffix/prefix conventions Linguist recognises and that real infra repos use
/// heavily (`Jenkinsfile_ECS`, `Jenkinsfile_dev`, `Dockerfile.prod`,
/// `Makefile.am`, `foo.Dockerfile`). Matched against the whole file name,
/// case-insensitively. Anchored to a separator (`.`/`-`/`_`) so they do not
/// accidentally swallow unrelated files (e.g. `jenkinsfileresults.log`).
const PROJECT_FILENAME_GLOB_LIST: &[&str] = &[
    "Dockerfile.*",
    "Dockerfile-*",
    "Dockerfile_*",
    "*.Dockerfile",
    "Jenkinsfile.*",
    "Jenkinsfile-*",
    "Jenkinsfile_*",
    "Makefile.*",
    "GNUmakefile.*",
    "Gemfile.*",
    "Rakefile.*",
    "Vagrantfile.*",
];

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
    fn filename_allowed(&self, path: &Path) -> bool {
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
        if let Some(ref dotted) = ext_dotted {
            if LIBRARY_ROUTED_EXTENSIONS.contains(&dotted.as_str()) {
                return FileRoute::LibraryCollection {
                    source_project_id: Some(tenant_id.to_string()),
                };
            }
            if self.project_extensions.contains(dotted) {
                return FileRoute::ProjectCollection;
            }
        }

        // Extension missing or not allowlisted — well-known build/CI/config/docs
        // files (text/code, never binary documents) route to the project
        // collection so Dockerfiles, Jenkinsfiles and Makefiles get indexed.
        if self.filename_allowed(path) {
            return FileRoute::ProjectCollection;
        }

        FileRoute::Excluded
    }
}
