//! Tests for the Language enum: extension mapping, identifiers, and LSP support flags.

use crate::lsp::Language;

#[test]
fn test_language_enumeration() {
    // Test language identification
    assert_eq!(Language::from_extension("py"), Language::Python);
    assert_eq!(Language::from_extension("rs"), Language::Rust);
    assert_eq!(Language::from_extension("js"), Language::JavaScript);
    assert_eq!(Language::from_extension("ts"), Language::TypeScript);
    assert_eq!(Language::from_extension("go"), Language::Go);
    assert_eq!(Language::from_extension("c"), Language::C);
    assert_eq!(Language::from_extension("cpp"), Language::Cpp);
    assert_eq!(
        Language::from_extension("unknown"),
        Language::Other("unknown".to_string())
    );

    // Test language identifiers
    assert_eq!(Language::Python.identifier(), "python");
    assert_eq!(Language::Rust.identifier(), "rust");
    assert_eq!(Language::JavaScript.identifier(), "javascript");

    // Test extensions
    assert!(Language::Python.extensions().contains(&"py"));
    assert!(Language::Rust.extensions().contains(&"rs"));
    assert!(Language::JavaScript.extensions().contains(&"js"));
}

#[test]
fn test_dart_and_r_language_support() {
    // Dart: .dart files → Language::Dart
    assert_eq!(Language::from_id("dart"), Language::Dart);
    assert_eq!(Language::Dart.identifier(), "dart");
    assert!(Language::Dart.extensions().contains(&"dart"));
    assert!(Language::Dart.has_lsp_support());

    // R: .r/.R/.Rmd files → Language::R
    assert_eq!(Language::from_id("r"), Language::R);
    assert_eq!(Language::R.identifier(), "r");
    assert!(Language::R.extensions().contains(&"r"));
    assert!(Language::R.extensions().contains(&"rmd"));
    assert!(Language::R.has_lsp_support());
}

#[test]
fn test_language_has_lsp_support() {
    // Languages with LSP server support
    assert!(Language::Python.has_lsp_support());
    assert!(Language::Rust.has_lsp_support());
    assert!(Language::TypeScript.has_lsp_support());
    assert!(Language::JavaScript.has_lsp_support());
    assert!(Language::Json.has_lsp_support());
    assert!(Language::C.has_lsp_support());
    assert!(Language::Cpp.has_lsp_support());
    assert!(Language::Go.has_lsp_support());

    // Programming languages that could have LSP servers
    assert!(Language::Java.has_lsp_support());
    assert!(Language::Dart.has_lsp_support());
    assert!(Language::R.has_lsp_support());
    assert!(Language::Ruby.has_lsp_support());
    assert!(Language::Php.has_lsp_support());
    assert!(Language::Shell.has_lsp_support());
    assert!(Language::Html.has_lsp_support());

    // Data/config formats — should skip LSP enrichment
    assert!(!Language::Yaml.has_lsp_support());
    assert!(!Language::Toml.has_lsp_support());
    assert!(!Language::Css.has_lsp_support());
    assert!(!Language::Sql.has_lsp_support());
    assert!(!Language::Xml.has_lsp_support());
    assert!(!Language::Other("md".to_string()).has_lsp_support());
}

#[test]
fn test_non_code_extensions_skip_lsp() {
    // Markdown files should not have LSP support
    let md_lang = Language::from_extension("md");
    assert!(!md_lang.has_lsp_support());

    // Config files should not have LSP support
    let yaml_lang = Language::from_extension("yaml");
    assert!(!yaml_lang.has_lsp_support());
    let toml_lang = Language::from_extension("toml");
    assert!(!toml_lang.has_lsp_support());
    let json_lang = Language::from_extension("json");
    assert!(json_lang.has_lsp_support()); // JSON has vscode-json-languageserver

    // Code files should have LSP support
    let rs_lang = Language::from_extension("rs");
    assert!(rs_lang.has_lsp_support());
    let ts_lang = Language::from_extension("ts");
    assert!(ts_lang.has_lsp_support());
    let py_lang = Language::from_extension("py");
    assert!(py_lang.has_lsp_support());
}

#[test]
fn test_classifier_id_matches_graph_vocabulary() {
    // classifier_id() must line up with the classifier/graph `language` id so the
    // graph_lsp_superseded_total metric label overlays graph_nodes_by_language.
    // Shell is the divergence: LSP languageId "shellscript" vs classifier "bash".
    assert_eq!(Language::Shell.identifier(), "shellscript");
    assert_eq!(Language::Shell.classifier_id(), "bash");
    // `bash` round-trips: it is the id the classifier emits (from_id("bash") == Shell).
    assert_eq!(Language::from_id("bash"), Language::Shell);

    // Everywhere else classifier_id() equals identifier() (the lowercase id the
    // graph stores), so those series already align.
    for lang in [
        Language::Python,
        Language::Rust,
        Language::TypeScript,
        Language::JavaScript,
        Language::Go,
        Language::Php,
        Language::Cpp,
    ] {
        assert_eq!(lang.classifier_id(), lang.identifier());
    }
    assert_eq!(
        Language::Other("kotlin".to_string()).classifier_id(),
        "kotlin"
    );
}
