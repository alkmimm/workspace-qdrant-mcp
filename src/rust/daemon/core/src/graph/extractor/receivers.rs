//! The receiver type of a method call: `batch.set(…)` where the chunk (or its
//! class) declares `final batch = FirestoreFinanceBatch(…)`.
//!
//! Tree-sitter's call list keeps only the method NAME, so the resolver could
//! tell `FirestoreFinanceBatch.set` from any other `set` only by where the
//! files sit (live 2026-10-07: every test call to it was "ambiguous", and the
//! Batch's own `_batch.set(…)` — a Firestore `WriteBatch` — bound to the other
//! class's `set` in the same file). This reads the receiver back from the
//! definition's text: a declared or constructed variable's type, or the class
//! itself for a static call / named constructor (`Backup.fromJson(…)`). A call
//! site with no typed receiver (bare, chained, untyped) does not cancel what
//! the typed sites prove: the hint says some sites were untyped, and those
//! keep the plain by-name resolution.

use std::collections::{BTreeSet, HashMap};
use std::sync::LazyLock;

use regex::Regex;

use crate::graph::{EdgeType, GraphEdge, GraphNode, NodeType};

/// Declared types of names (locals, parameters, fields), by name.
pub(super) type TypeMap = HashMap<String, String>;

/// What a definition's call sites say about the receivers of one callee name.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(super) struct CallHint {
    /// Types of receivers that are declared or constructed variables.
    pub types: BTreeSet<String>,
    /// Classes named as the receiver itself (`Type.member(`).
    pub static_types: BTreeSet<String>,
    /// Some call site had no typed receiver.
    pub untyped_sites: bool,
}

/// Receiver hints of a definition's calls, by callee name.
pub(super) type CallHints = HashMap<String, CallHint>;

/// A CALLS edge from `caller` to the name-only stub of `callee`, carrying what
/// the call sites said about the receiver.
pub(super) fn calls_edge(
    tenant_id: &str,
    caller: &GraphNode,
    callee: &GraphNode,
    callee_name: &str,
    file_path: &str,
    hints: &CallHints,
) -> GraphEdge {
    let mut edge = GraphEdge::new(
        tenant_id,
        &caller.node_id,
        &callee.node_id,
        EdgeType::Calls,
        file_path,
    );
    edge.metadata_json = hints.get(callee_name).map(hint_metadata);
    edge
}

/// Whether a node kind declares members (its fields type its members' calls).
pub(super) fn is_container(node_type: NodeType) -> bool {
    matches!(
        node_type,
        NodeType::Class
            | NodeType::Struct
            | NodeType::Interface
            | NodeType::Trait
            | NodeType::Impl
            | NodeType::Enum
    )
}

/// `name = [new|const] Type(` — a name bound to a constructor call.
static CONSTRUCTED: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"([A-Za-z_]\w*)\s*=\s*(?:new\s+|const\s+)?([A-Z]\w*)\s*(?:<[^<>()=;]*>)?\s*\(")
        .expect("constructed-binding regex")
});

/// `Type name` followed by `=`, `;`, `,` or `)` — a C-style typed declaration
/// or parameter (Dart, Java, C#, C/C++, Vala).
static TYPE_THEN_NAME: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"\b([A-Z]\w*)\s*(?:<[^<>()=;]*>)?\??\s+([A-Za-z_]\w*)\s*[=;,)]")
        .expect("typed-declaration regex")
});

/// `name: Type` — a typed binding or parameter (TypeScript, Kotlin, Rust,
/// Python hints, Swift, Scala).
static NAME_COLON_TYPE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"\b([A-Za-z_]\w*)\s*:\s*&?(?:mut\s+)?([A-Z]\w*)").expect("colon-typed regex")
});

/// `name = Type::new(` and friends (Rust).
static RUST_CONSTRUCTOR: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"([a-z_]\w*)\s*=\s*([A-Z]\w*)::(?:new|default|from|with_capacity)\s*\(")
        .expect("rust-constructor regex")
});

fn type_then_name(language: &str) -> bool {
    matches!(language, "dart" | "java" | "c-sharp" | "cpp" | "c" | "vala")
}

fn name_colon_type(language: &str) -> bool {
    matches!(
        language,
        "typescript" | "tsx" | "kotlin" | "rust" | "python" | "swift" | "scala"
    )
}

/// Languages where a capitalized receiver that no variable declares is a
/// class (`DateTime.now()`). Not C#, whose properties are PascalCase too, nor
/// Go, whose exported fields are; Rust and C++ spell static calls `::`.
fn class_named_receivers(language: &str) -> bool {
    matches!(
        language,
        "dart"
            | "java"
            | "kotlin"
            | "typescript"
            | "tsx"
            | "javascript"
            | "python"
            | "swift"
            | "scala"
    )
}

/// The types `content` declares for names, by the language's declaration
/// shapes. A constructor binding wins over a declared type: `I x = A();` calls
/// `A`'s methods.
pub(super) fn declared_types(content: &str, language: &str) -> TypeMap {
    let mut types = TypeMap::new();
    if type_then_name(language) {
        for c in TYPE_THEN_NAME.captures_iter(content) {
            types.insert(c[2].to_string(), c[1].to_string());
        }
    }
    if name_colon_type(language) {
        for c in NAME_COLON_TYPE.captures_iter(content) {
            types.insert(c[1].to_string(), c[2].to_string());
        }
    }
    if language == "rust" {
        for c in RUST_CONSTRUCTOR.captures_iter(content) {
            types.insert(c[1].to_string(), c[2].to_string());
        }
    }
    for c in CONSTRUCTED.captures_iter(content) {
        types.insert(c[1].to_string(), c[2].to_string());
    }
    types
}

/// The part of a class body outside its members' bodies: what the class
/// itself declares (fields, constructor parameters), not its methods' locals.
pub(super) fn class_level_text(content: &str) -> String {
    if !content.contains('{') {
        return content.to_string();
    }
    let mut out = String::with_capacity(content.len());
    let mut depth = 0usize;
    for ch in content.chars() {
        match ch {
            '{' => depth += 1,
            '}' => depth = depth.saturating_sub(1),
            _ if depth <= 1 => out.push(ch),
            _ => {}
        }
    }
    out
}

/// For each name in `calls`, what its call sites in `content` say about the
/// receiver — present when at least one site has a typed receiver.
///
/// `own_symbol` is the definition's own name: its occurrence in the signature
/// is the declaration, not a call.
pub(super) fn receiver_types(
    content: &str,
    calls: &[String],
    types: &TypeMap,
    own_symbol: &str,
    language: &str,
) -> CallHints {
    let body_start = content
        .find('{')
        .into_iter()
        .chain(content.find("=>"))
        .min()
        .unwrap_or(0);
    let site = CallSite {
        content,
        types,
        own_symbol,
        body_start,
        class_receivers: class_named_receivers(language),
    };
    let mut hints = HashMap::new();
    for name in calls {
        if let Some(found) = site.hint_for(name) {
            hints.insert(name.clone(), found);
        }
    }
    hints
}

/// The definition text a call name is looked up in.
struct CallSite<'a> {
    content: &'a str,
    types: &'a TypeMap,
    own_symbol: &'a str,
    body_start: usize,
    class_receivers: bool,
}

impl CallSite<'_> {
    fn hint_for(&self, name: &str) -> Option<CallHint> {
        let bytes = self.content.as_bytes();
        let mut hint = CallHint::default();
        for (at, _) in self.content.match_indices(name) {
            let end = at + name.len();
            if (at > 0 && is_ident(bytes[at - 1])) || (end < bytes.len() && is_ident(bytes[end])) {
                continue; // part of a longer identifier
            }
            if !is_call_after(&self.content[end..]) {
                continue;
            }
            if name == self.own_symbol && at < self.body_start {
                continue; // the declaration itself
            }
            let receiver = receiver_of(self.content, at);
            match receiver.and_then(|r| self.types.get(r)) {
                Some(declared) => {
                    hint.types.insert(declared.clone());
                }
                None => match receiver.filter(|r| self.class_receivers && is_class_name(r)) {
                    Some(class) => {
                        hint.static_types.insert(class.to_string());
                    }
                    None => hint.untyped_sites = true,
                },
            }
        }
        (!hint.types.is_empty() || !hint.static_types.is_empty()).then_some(hint)
    }
}

/// `Backup` in `Backup.fromJson(`: a capitalized name, by convention a type.
fn is_class_name(receiver: &str) -> bool {
    receiver.chars().next().is_some_and(char::is_uppercase)
}

/// Whether the text after a name is a call: optional generic arguments, then `(`.
fn is_call_after(rest: &str) -> bool {
    let rest = rest.trim_start();
    let rest = match rest.strip_prefix('<') {
        Some(generic) => match generic.find('>') {
            Some(close) => generic[close + 1..].trim_start(),
            None => return false,
        },
        None => rest,
    };
    rest.starts_with('(')
}

/// The receiver of the call whose name starts at `at`: `x` in `x.name(` or
/// `this.x.name(` / `self.x.name(`. `None` for a bare call or a receiver that
/// is itself an expression (`a.b.name(`, `f().name(`).
fn receiver_of(content: &str, at: usize) -> Option<&str> {
    let before = content[..at].trim_end();
    let before = before.strip_suffix('?').unwrap_or(before);
    let before = before.strip_suffix('.')?.trim_end();
    // The identifier starts after the last non-identifier CHARACTER, whose
    // width is not 1 byte in general: `i + 1` split `ç`/`á` in Portuguese
    // comments and panicked the whole queue item (live 2026-10-07). Unicode
    // letters count as identifier characters (Java/Kotlin/Dart allow them).
    let start = before
        .char_indices()
        .rev()
        .find(|&(_, c)| !(c.is_alphanumeric() || c == '_'))
        .map_or(0, |(i, c)| i + c.len_utf8());
    let receiver = &before[start..];
    if receiver.is_empty() {
        return None;
    }
    let ahead = before[..start].trim_end();
    match ahead.strip_suffix('.') {
        None => Some(receiver),
        Some(chain) => {
            let chain = chain.trim_end();
            (chain.ends_with("this") || chain.ends_with("self")).then_some(receiver)
        }
    }
}

fn is_ident(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_'
}

/// Field types of every container in a file, by container name, from the
/// container chunks' class-level text. `(name, content, language)` per chunk.
pub(super) fn field_types<'a>(
    containers: impl Iterator<Item = (&'a str, &'a str, &'a str)>,
) -> HashMap<String, TypeMap> {
    let mut by_class: HashMap<String, TypeMap> = HashMap::new();
    for (name, content, language) in containers {
        by_class
            .entry(name.to_string())
            .or_default()
            .extend(declared_types(&class_level_text(content), language));
    }
    by_class
}

/// Receiver hints for one callable definition (`content` is its WHOLE text,
/// every fragment): its class's fields, overridden by what it declares.
pub(super) fn chunk_hints(
    content: &str,
    language: &str,
    calls: &[String],
    own_symbol: &str,
    parent: Option<&str>,
    fields: &HashMap<String, TypeMap>,
) -> CallHints {
    let mut types = parent
        .and_then(|p| fields.get(p))
        .cloned()
        .unwrap_or_default();
    types.extend(declared_types(content, language));
    receiver_types(content, calls, &types, own_symbol, language)
}

/// The CALLS edge metadata carrying a call's receiver hint to the resolver
/// (read back by `sqlite_store::resolution_tiers::receiver_hint`).
pub(super) fn hint_metadata(hint: &CallHint) -> String {
    let mut meta = serde_json::Map::new();
    if !hint.types.is_empty() {
        meta.insert("receiver_types".into(), serde_json::json!(hint.types));
    }
    if !hint.static_types.is_empty() {
        meta.insert(
            "static_receivers".into(),
            serde_json::json!(hint.static_types),
        );
    }
    if hint.untyped_sites {
        meta.insert("untyped_sites".into(), serde_json::Value::Bool(true));
    }
    serde_json::Value::Object(meta).to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn calls(names: &[&str]) -> Vec<String> {
        names.iter().map(|s| s.to_string()).collect()
    }

    fn set(names: &[&str]) -> BTreeSet<String> {
        names.iter().map(|s| s.to_string()).collect()
    }

    /// A hint whose every call site has a declared receiver of these types.
    fn declared(names: &[&str]) -> Option<CallHint> {
        Some(CallHint {
            types: set(names),
            ..CallHint::default()
        })
    }

    /// Finance `finance_revision_test.dart`, lines 31–35.
    #[test]
    fn a_constructed_local_types_its_method_calls() {
        let body = "() async {\n    final batch = FirestoreFinanceBatch(firestore: db);\n    \
                    batch.set(db.doc('a'), {'price': 10});\n    batch.set(db.doc('b'), {});\n    \
                    await batch.commit();\n  }";
        let types = declared_types(body, "dart");
        let hints = receiver_types(
            body,
            &calls(&["set", "commit", "doc"]),
            &types,
            "main",
            "dart",
        );
        assert_eq!(
            hints.get("set").cloned(),
            declared(&["FirestoreFinanceBatch"])
        );
        assert_eq!(
            hints.get("commit").cloned(),
            declared(&["FirestoreFinanceBatch"])
        );
        assert_eq!(hints.get("doc"), None, "db is not declared here");
    }

    /// The Batch's own `set` forwards to a Firestore `WriteBatch` field, and its
    /// own signature is not a call.
    #[test]
    fn a_field_types_the_call_and_the_signature_is_skipped() {
        let class = "class FirestoreFinanceBatch implements WriteBatch {\n  \
                     final WriteBatch _batch;\n  void other() { final x = Other(); }\n}";
        let fields = declared_types(&class_level_text(class), "dart");
        assert_eq!(fields.get("_batch").map(String::as_str), Some("WriteBatch"));
        assert_eq!(fields.get("x"), None, "a method's local is not a field");

        let method = "void set<T>(DocumentReference<T> document, T data) {\n    \
                      _touch(document);\n    _batch.set(document, data);\n  }";
        let mut types = fields.clone();
        types.extend(declared_types(method, "dart"));
        let hints = receiver_types(method, &calls(&["set", "_touch"]), &types, "set", "dart");
        assert_eq!(hints.get("set").cloned(), declared(&["WriteBatch"]));
        assert_eq!(hints.get("_touch"), None, "a bare call has no receiver");
    }

    /// An untyped site (`other.run()`) no longer cancels what the typed one
    /// proves: the hint keeps `Alpha` and says some site was untyped.
    #[test]
    fn an_untyped_call_site_keeps_its_by_name_fallback() {
        let body = "{ final a = Alpha(); a.run(); other.run(); }";
        let hints = receiver_types(
            body,
            &calls(&["run"]),
            &declared_types(body, "dart"),
            "f",
            "dart",
        );
        let run = hints.get("run").expect("a typed site");
        assert_eq!(run.types, set(&["Alpha"]));
        assert!(run.untyped_sites);
    }

    /// `I x = B();` calls `B`'s methods; a chained receiver is an expression.
    #[test]
    fn the_constructor_wins_over_the_declared_interface() {
        let body = "{ Runner x = Beta(); x.run(); this.y.go(); a.b.stop(); }";
        let types = declared_types(body, "java");
        assert_eq!(types.get("x").map(String::as_str), Some("Beta"));
        let hints = receiver_types(body, &calls(&["run", "stop"]), &types, "f", "java");
        assert_eq!(hints.get("run").cloned(), declared(&["Beta"]));
        assert_eq!(hints.get("stop"), None);
    }

    #[test]
    fn colon_typed_languages_bind_their_parameters() {
        let body = "function f(store: OrderStore) { store.save(o); }";
        let hints = receiver_types(
            body,
            &calls(&["save"]),
            &declared_types(body, "typescript"),
            "f",
            "typescript",
        );
        assert_eq!(hints.get("save").cloned(), declared(&["OrderStore"]));
    }

    /// Finance tests call `CashFlowBackup.fromJson(…)` and `DateTime.now()`:
    /// the receiver names the class. Not in C#, where `Logger.Log()` reads a
    /// PascalCase property.
    #[test]
    fn a_class_named_as_the_receiver_types_a_static_call() {
        let body = "{ final b = CashFlowBackup.fromJson(m); DateTime.now(); repo.save(b); }";
        let hints = receiver_types(
            body,
            &calls(&["fromJson", "now", "save"]),
            &declared_types(body, "dart"),
            "f",
            "dart",
        );
        let from_json = hints.get("fromJson").expect("static hint");
        assert_eq!(from_json.static_types, set(&["CashFlowBackup"]));
        assert!(from_json.types.is_empty() && !from_json.untyped_sites);
        assert_eq!(hints["now"].static_types, set(&["DateTime"]));
        assert_eq!(hints.get("save"), None, "repo is untyped");

        let csharp = "{ Logger.Log(x); }";
        let hints = receiver_types(csharp, &calls(&["Log"]), &TypeMap::new(), "f", "c-sharp");
        assert_eq!(hints.get("Log"), None);
    }

    #[test]
    fn the_metadata_carries_only_what_the_sites_said() {
        let hint = CallHint {
            types: set(&["Batch"]),
            static_types: set(&["Backup"]),
            untyped_sites: true,
        };
        let meta: serde_json::Value = serde_json::from_str(&hint_metadata(&hint)).unwrap();
        assert_eq!(meta["receiver_types"][0], "Batch");
        assert_eq!(meta["static_receivers"][0], "Backup");
        assert_eq!(meta["untyped_sites"], true);
        let plain = hint_metadata(&declared(&["Batch"]).unwrap());
        assert_eq!(plain, r#"{"receiver_types":["Batch"]}"#);
    }

    /// Live 2026-10-07: Java tests with Portuguese comments panicked the queue
    /// item — the scan stepped one BYTE past the last non-identifier char,
    /// which split `ç`/`á`. Accented words are identifier text, `—` is not.
    #[test]
    fn multibyte_text_before_a_receiver_never_splits_a_char() {
        let body = "void t() {\n    // valida a configuração.set(x) — ação.set(y)\n    \
                    Batch configuração = new Batch();\n    configuração.set(z);\n}";
        let types = declared_types(body, "java");
        // `configuração.set` ends in `ação.set` too: aim at the bare word.
        let at = |needle: &str| body.find(needle).unwrap() + needle.len() - "set".len();
        assert_eq!(receiver_of(body, at("— ação.set")), Some("ação"));
        assert_eq!(
            receiver_of(body, at("a configuração.set")),
            Some("configuração")
        );
        let hints = receiver_types(body, &calls(&["set"]), &types, "t", "java");
        let both = hints.get("set").expect("configuração is typed");
        assert_eq!(both.types, set(&["Batch"]));
        assert!(both.untyped_sites, "ação has no declared type");

        let typed = "void t() {\n    Batch configuração = new Batch();\n    // ação —\n    \
                     configuração.set(z);\n}";
        let hints = receiver_types(
            typed,
            &calls(&["set"]),
            &declared_types(typed, "java"),
            "t",
            "java",
        );
        assert_eq!(hints.get("set").cloned(), declared(&["Batch"]));
    }
}
