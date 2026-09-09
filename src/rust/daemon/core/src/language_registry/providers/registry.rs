//! Language registry provider — single source of truth.
//!
//! Embeds language definitions at compile time from `language_registry.yaml`.
//! This provider serves as the offline fallback when no network
//! providers are available.

use async_trait::async_trait;

use crate::error::DaemonError;
use crate::language_registry::provider::LanguageSourceProvider;
use crate::language_registry::types::{
    GrammarEntry, LanguageEntry, LspEntry, LspServerEntry, ProviderData,
};
use crate::language_registry::LanguageDefinition;

const REGISTRY_YAML: &str = include_str!("../language_registry.yaml");

/// Provider that loads language definitions from the embedded registry YAML.
///
/// This provider has the lowest priority (255) and serves as the offline
/// fallback when no network-based providers are available. All 44 known
/// language grammars are included, with semantic patterns for the 25
/// languages that have dedicated extractors.
pub struct RegistryProvider {
    definitions: Vec<LanguageDefinition>,
}

impl RegistryProvider {
    /// Create a new bundled provider by parsing the embedded YAML.
    ///
    /// # Errors
    ///
    /// Returns `DaemonError::Config` if the embedded YAML fails to parse.
    /// This should never happen in practice since the YAML is validated
    /// at test time.
    pub fn new() -> Result<Self, DaemonError> {
        let definitions: Vec<LanguageDefinition> = serde_yaml_ng::from_str(REGISTRY_YAML)
            .map_err(|e| DaemonError::Other(format!("Failed to parse bundled languages: {e}")))?;
        Ok(Self { definitions })
    }

    /// Access the full language definitions (including semantic patterns).
    pub fn definitions(&self) -> &[LanguageDefinition] {
        &self.definitions
    }
}

#[async_trait]
impl LanguageSourceProvider for RegistryProvider {
    fn name(&self) -> &str {
        "registry"
    }

    fn priority(&self) -> u8 {
        255
    }

    fn last_updated(&self) -> Option<chrono::DateTime<chrono::Utc>> {
        None
    }

    async fn fetch_languages(&self) -> Result<Vec<LanguageEntry>, DaemonError> {
        Ok(self
            .definitions
            .iter()
            .map(|d| LanguageEntry {
                name: d.language.clone(),
                id: d.id(),
                aliases: d.aliases.clone(),
                extensions: d.extensions.clone(),
                language_type: d.language_type,
            })
            .collect())
    }

    async fn fetch_grammars(&self) -> Result<Vec<GrammarEntry>, DaemonError> {
        let mut entries = Vec::new();
        for def in &self.definitions {
            for src in &def.grammar.sources {
                entries.push(GrammarEntry {
                    language: def.id(),
                    repo: src.repo.clone(),
                    quality: src.quality,
                    has_cpp_scanner: def.grammar.has_cpp_scanner,
                    src_subdir: def.grammar.src_subdir.clone(),
                    symbol_name: def.grammar.symbol_name.clone(),
                    archive_branch: def.grammar.archive_branch.clone(),
                });
            }
        }
        Ok(entries)
    }

    async fn fetch_lsp_servers(&self) -> Result<Vec<LspEntry>, DaemonError> {
        let mut entries = Vec::new();
        for def in &self.definitions {
            for server in &def.lsp_servers {
                entries.push(LspEntry {
                    language: def.id(),
                    server: LspServerEntry {
                        name: server.name.clone(),
                        binary: server.binary.clone(),
                        args: server.args.clone(),
                        priority: server.priority,
                        install_methods: server.install_methods.clone(),
                    },
                });
            }
        }
        Ok(entries)
    }

    async fn refresh(&self) -> Result<ProviderData, DaemonError> {
        Ok(ProviderData {
            languages: self.fetch_languages().await?,
            grammars: self.fetch_grammars().await?,
            lsp_servers: self.fetch_lsp_servers().await?,
        })
    }

    fn full_definitions(&self) -> Option<&[LanguageDefinition]> {
        Some(&self.definitions)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_bundled_yaml_parses() {
        let provider = RegistryProvider::new().expect("bundled YAML should parse");
        assert!(
            provider.definitions.len() > 40,
            "expected > 40 languages, got {}",
            provider.definitions.len()
        );
    }

    #[test]
    fn test_all_44_languages_present() {
        let provider = RegistryProvider::new().unwrap();
        let ids: Vec<String> = provider.definitions.iter().map(|d| d.id()).collect();

        let expected = [
            "ada",
            "bash",
            "c",
            "c-sharp",
            "clojure",
            "cpp",
            "css",
            "dart",
            "elixir",
            "elm",
            "erlang",
            "fortran",
            "go",
            "haskell",
            "html",
            "java",
            "javascript",
            "json",
            "julia",
            "kotlin",
            "latex",
            "lisp",
            "lua",
            "markdown",
            "nix",
            "ocaml",
            "odin",
            "pascal",
            "perl",
            "php",
            "python",
            "r",
            "ruby",
            "rust",
            "scala",
            "scheme",
            "sql",
            "swift",
            "toml",
            "tsx",
            "typescript",
            "vala",
            "vue",
            "yaml",
            "zig",
        ];

        for lang in &expected {
            assert!(
                ids.contains(&(*lang).to_string()),
                "missing language: {lang}"
            );
        }
    }

    #[tokio::test]
    async fn test_bundled_languages_cover_known_grammars() {
        let provider = RegistryProvider::new().unwrap();
        let languages = provider.fetch_languages().await.unwrap();
        let ids: Vec<&str> = languages.iter().map(|l| l.id.as_str()).collect();
        assert!(ids.contains(&"rust"));
        assert!(ids.contains(&"python"));
        assert!(ids.contains(&"javascript"));
    }

    #[tokio::test]
    async fn test_bundled_grammars() {
        let provider = RegistryProvider::new().unwrap();
        let grammars = provider.fetch_grammars().await.unwrap();
        assert!(
            grammars.len() >= 44,
            "expected >= 44 grammars, got {}",
            grammars.len()
        );
        let rust_grammar = grammars.iter().find(|g| g.language == "rust").unwrap();
        assert_eq!(rust_grammar.repo, "tree-sitter/tree-sitter-rust");
    }

    #[tokio::test]
    async fn test_bundled_lsp_servers() {
        let provider = RegistryProvider::new().unwrap();
        let servers = provider.fetch_lsp_servers().await.unwrap();

        // We have LSP entries for: python, rust, javascript, typescript, tsx,
        // json, c, cpp, go, java, ruby, php, bash, html
        assert!(
            servers.len() >= 10,
            "expected >= 10 LSP entries, got {}",
            servers.len()
        );

        let rust_servers: Vec<_> = servers.iter().filter(|s| s.language == "rust").collect();
        assert_eq!(rust_servers.len(), 1);
        assert_eq!(rust_servers[0].server.name, "rust-analyzer");

        let python_servers: Vec<_> = servers.iter().filter(|s| s.language == "python").collect();
        assert_eq!(python_servers.len(), 3);
    }

    #[tokio::test]
    async fn test_bundled_refresh() {
        let provider = RegistryProvider::new().unwrap();
        let data = provider.refresh().await.unwrap();

        assert!(!data.languages.is_empty());
        assert!(!data.grammars.is_empty());
        assert!(!data.lsp_servers.is_empty());
    }

    #[test]
    fn test_provider_metadata() {
        let provider = RegistryProvider::new().unwrap();
        assert_eq!(provider.name(), "registry");
        assert_eq!(provider.priority(), 255);
        assert!(provider.last_updated().is_none());
        assert!(provider.is_enabled());
    }

    #[test]
    fn test_semantic_patterns_present_for_extractors() {
        let provider = RegistryProvider::new().unwrap();

        let languages_with_patterns = [
            "python",
            "rust",
            "go",
            "java",
            "javascript",
            "typescript",
            "tsx",
            "c",
            "cpp",
            "ruby",
            "swift",
            "bash",
            "lua",
            "elixir",
            "erlang",
            "scala",
            "haskell",
            "zig",
            "odin",
            "clojure",
            "ocaml",
            "fortran",
            "ada",
            "perl",
            "pascal",
            "lisp",
            "protobuf",
        ];

        for lang_id in &languages_with_patterns {
            let def = provider
                .definitions
                .iter()
                .find(|d| d.id() == *lang_id)
                .unwrap_or_else(|| panic!("missing language: {lang_id}"));
            assert!(
                def.has_semantic_patterns(),
                "{lang_id} should have semantic patterns"
            );
        }
    }

    /// Dart must extract top-level bindings, not just callables and types.
    ///
    /// `final xProvider = Provider(…)` — the Riverpod idiom, and the shape most
    /// Dart dependency wiring takes — matched NO pattern at all, so no graph node
    /// was ever created and `usages` answered 0 against 71 real references on
    /// disk (#369). A zero there is indistinguishable from "genuinely unused",
    /// which is what made it a trap rather than a gap.
    ///
    /// The WRAPPER is the load-bearing half. Verified by parsing each form with
    /// the cached grammar this daemon actually uses:
    ///
    /// ```text
    /// const bool kX = false;   program > static_final_declaration_list > static_final_declaration
    /// final p = Provider(…);   program > static_final_declaration_list > static_final_declaration
    /// var counter = 0;         program > initialized_identifier_list   > initialized_identifier
    /// late final String t;     program > initialized_identifier_list   > initialized_identifier
    /// ```
    ///
    /// The walker classifies the ROOT child, so without `root_wrappers` it sees
    /// only the list and never reaches the declaration. The first attempt at this
    /// issue listed inner kinds alone and changed nothing: `.dart` still had ZERO
    /// `constant` nodes while `.ts` had 3181, and `usages` still answered 0.
    ///
    /// **This test pins CONFIGURATION, not BEHAVIOUR — and that is a known
    /// weakness, not an oversight.** The behaviour tests that would catch a
    /// silently-non-extracting language live in `generic_extractor/tests.rs` and
    /// are structurally inert (see #376): they gate on `get_language`, which is an
    /// alias for a function whose whole body is `None`. Until #376 lands, the
    /// strongest honest assertion here is that the pairing the grammar requires is
    /// present. Do not read a green run as proof that extraction works.
    #[test]
    fn dart_extracts_top_level_bindings() {
        let provider = RegistryProvider::new().unwrap();
        let dart = provider
            .definitions
            .iter()
            .find(|d| d.id() == "dart")
            .expect("missing language: dart");
        let patterns = dart
            .semantic_patterns
            .as_ref()
            .expect("dart must have semantic patterns");

        for kind in ["static_final_declaration", "initialized_identifier"] {
            assert!(
                patterns.constant.node_types.iter().any(|t| t == kind),
                "dart constant.node_types must cover {kind}; got {:?}",
                patterns.constant.node_types
            );
        }
        // Without these the node_types above are unreachable — the defect that
        // made the first fix a no-op.
        for wrapper in [
            "static_final_declaration_list",
            "initialized_identifier_list",
        ] {
            assert!(
                patterns.root_wrappers.iter().any(|t| t == wrapper),
                "dart root_wrappers must unwrap {wrapper}, or the walker never \
                 reaches the declaration inside it; got {:?}",
                patterns.root_wrappers
            );
        }
        // `initialized_variable_definition` appears in NO top-level form — it was
        // dead configuration in the first attempt, kept out so the registry
        // describes the grammar rather than a guess about it.
        assert!(
            !patterns
                .constant
                .node_types
                .iter()
                .any(|t| t == "initialized_variable_definition"),
            "a node kind no top-level form produces must not be listed"
        );
        // Function locals stay out: not a referenceable API, pure graph noise.
        assert!(
            !patterns
                .constant
                .node_types
                .iter()
                .any(|t| t == "local_variable_declaration"),
            "function locals must stay out of the graph"
        );
    }

    /// Java must classify records as types, not skip them.
    ///
    /// Found by the same audit as the Dart hole above, and larger: `usages` on a
    /// record answered 0 because `record_declaration` matched no pattern group, so
    /// `classify_node` returned `None` and no symbol was created. Measured on
    /// DOC-V2 — 468 records against 728 classes, 39% of the declared types
    /// invisible to the graph. A record's compact constructor and an `@interface`
    /// are likewise their own node kinds rather than reusing the class-body forms.
    #[test]
    fn java_extracts_records_and_annotation_types() {
        let provider = RegistryProvider::new().unwrap();
        let java = provider
            .definitions
            .iter()
            .find(|d| d.id() == "java")
            .expect("missing language: java");
        let patterns = java
            .semantic_patterns
            .as_ref()
            .expect("java must have semantic patterns");

        for (group, kinds, kind) in [
            ("class", &patterns.class.node_types, "record_declaration"),
            (
                "interface",
                &patterns.interface.node_types,
                "annotation_type_declaration",
            ),
            (
                "method",
                &patterns.method.node_types,
                "compact_constructor_declaration",
            ),
        ] {
            assert!(
                kinds.iter().any(|t| t == kind),
                "java {group} must cover {kind}; got {kinds:?}"
            );
        }
    }

    /// No language may list the same AST node kind in both `function.node_types`
    /// and `function.async_node_types`. The chunker flags a node async purely by
    /// `node.kind()` membership in `async_node_types` (it does not inspect for an
    /// `async` child token), so an overlap marks EVERY function of that kind
    /// async. This is a regression guard for the Rust `function_item` overlap that
    /// mislabeled all sync `fn`s as `async_function`: modifier-async grammars must
    /// leave `async_node_types` empty; only a grammar with a genuinely distinct
    /// async node kind (e.g. Python's `async_function_definition`) may populate it.
    #[test]
    fn test_async_node_types_disjoint_from_function_node_types() {
        let provider = RegistryProvider::new().unwrap();
        for def in &provider.definitions {
            let Some(patterns) = def.semantic_patterns.as_ref() else {
                continue;
            };
            for async_kind in &patterns.function.async_node_types {
                assert!(
                    !patterns.function.node_types.contains(async_kind),
                    "{}: node kind `{async_kind}` is in both function.node_types and \
                     function.async_node_types — this marks every `{async_kind}` async. \
                     Leave async_node_types empty unless the grammar has a distinct \
                     async node kind.",
                    def.id(),
                );
            }
        }
    }

    #[test]
    fn test_protobuf_grammar_symbol_and_patterns() {
        let provider = RegistryProvider::new().unwrap();
        let def = provider
            .definitions
            .iter()
            .find(|d| d.id() == "protobuf")
            .expect("protobuf must be in the registry");

        // The mitchellh grammar declares `name: 'proto'`, so the compiled
        // library exports tree_sitter_proto — without this override the
        // loader would probe tree_sitter_protobuf and fail.
        assert_eq!(def.grammar.symbol_name.as_deref(), Some("proto"));

        // Without these, .proto files fall back to plain-text chunking
        // (chunk_type="text", symbol="_text") and lose symbol breadcrumbs.
        let patterns = def
            .semantic_patterns
            .as_ref()
            .expect("protobuf must have semantic patterns");
        assert_eq!(patterns.class.node_types, vec!["service"]);
        assert_eq!(patterns.method.node_types, vec!["rpc"]);
        assert_eq!(patterns.struct_def.node_types, vec!["message"]);
        assert_eq!(patterns.enum_def.node_types, vec!["enum"]);
        for preamble in ["syntax", "package", "import", "option"] {
            assert!(
                patterns.preamble.node_types.contains(&preamble.to_string()),
                "preamble should include {preamble}"
            );
        }
    }

    #[test]
    fn test_grammar_quality_tiers() {
        let provider = RegistryProvider::new().unwrap();

        // Official grammars (tree-sitter org)
        let rust_def = provider
            .definitions
            .iter()
            .find(|d| d.id() == "rust")
            .unwrap();
        assert_eq!(
            rust_def.grammar.sources[0].quality,
            crate::language_registry::types::GrammarQuality::Official
        );

        // Curated grammars (tree-sitter-grammars org)
        let lua_def = provider
            .definitions
            .iter()
            .find(|d| d.id() == "lua")
            .unwrap();
        assert_eq!(
            lua_def.grammar.sources[0].quality,
            crate::language_registry::types::GrammarQuality::Curated
        );

        // Community grammars
        let ada_def = provider
            .definitions
            .iter()
            .find(|d| d.id() == "ada")
            .unwrap();
        assert_eq!(
            ada_def.grammar.sources[0].quality,
            crate::language_registry::types::GrammarQuality::Community
        );
    }
}
