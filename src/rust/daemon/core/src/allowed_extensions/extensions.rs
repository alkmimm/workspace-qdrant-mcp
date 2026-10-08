//! The compiled allowlists: the only ingestion allowlist the daemon reads.
//! `scripts/gen-allowlist-yaml.py` mirrors them into
//! `assets/default_configuration.yaml`. The matcher built from them lives in
//! `super::matcher`; which collection a file routes to (and the documents a
//! project folder sends to its library) lives in `super::routing`.

/// Source code, config, and documentation extensions allowed in project collections.
pub(super) const PROJECT_EXTENSION_LIST: &[&str] = &[
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
    ".jsp",
    ".jspf",
    ".jspx", // JavaServer Pages: a Java web app's views
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
pub(super) const LIBRARY_ONLY_EXTENSION_LIST: &[&str] = &[
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
pub(super) const PROJECT_FILENAME_LIST: &[&str] = &[
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
pub(super) const PROJECT_FILENAME_GLOB_LIST: &[&str] = &[
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
