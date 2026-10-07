//! The types a definition's text declares for its names — locals, parameters,
//! fields — read by the language's declaration shapes. Feeds the receiver
//! hints (`receivers`): a call through a name whose type is known binds to
//! that type's member.

use std::collections::HashMap;
use std::sync::LazyLock;

use regex::Regex;

/// Declared types of names (locals, parameters, fields), by name.
pub(super) type TypeMap = HashMap<String, String>;

/// Optional generic arguments, nested up to three levels deep:
/// `StreamSubscription<List<IncomeSource>>` declares a `StreamSubscription`.
/// A flat `<[^<>]*>` missed every nested declaration (275 in Finance alone).
const GENERICS: &str = r"(?:<(?:[^<>()=;]|<(?:[^<>()=;]|<[^<>()=;]*>)*>)*>)?";

/// `name = [new|const] Type(` — a name bound to a constructor call.
static CONSTRUCTED: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(&format!(
        r"([A-Za-z_]\w*)\s*=\s*(?:new\s+|const\s+)?([A-Z]\w*)\s*{GENERICS}\s*\("
    ))
    .expect("constructed-binding regex")
});

/// `Type name` followed by `=`, `;`, `,` or `)` — a C-style typed declaration
/// or parameter (Dart, Java, C#, C/C++, Vala).
static TYPE_THEN_NAME: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(&format!(
        r"\b([A-Z]\w*)\s*{GENERICS}\??\s+([A-Za-z_]\w*)\s*[=;,)]"
    ))
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

/// `name = other;` — a binding to another name, which shares its type
/// (`final previous = _subscription;`). Not `==`, `+=`, `<=`: the `=` must
/// follow the name and be followed by a name.
static ALIAS: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"\b([A-Za-z_]\w*)\s*=\s*([A-Za-z_]\w*)\s*;").expect("alias regex")
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

/// Names `content` binds to another typed name take that type, in text order
/// (an alias of an alias resolves); a name with a type of its own keeps it.
/// Run on the merged map, so a local bound to a FIELD resolves too.
pub(super) fn resolve_aliases(content: &str, types: &mut TypeMap) {
    for c in ALIAS.captures_iter(content) {
        let (name, other) = (&c[1], &c[2]);
        if types.contains_key(name) {
            continue;
        }
        if let Some(t) = types.get(other).cloned() {
            types.insert(name.to_string(), t);
        }
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    /// Finance `income_sources_controller.dart`: the subscription field's
    /// type nests two generic levels deep.
    #[test]
    fn nested_generic_declarations_name_their_outer_type() {
        let text = "  StreamSubscription<List<IncomeSource>>? _subscription;\n  \
                    Map<String, List<Map<String, int>>> _deep = {};\n  \
                    final cache = LinkedHashMap<String, List<int>>();\n";
        let types = declared_types(text, "dart");
        assert_eq!(
            types.get("_subscription").map(String::as_str),
            Some("StreamSubscription")
        );
        assert_eq!(types.get("_deep").map(String::as_str), Some("Map"));
        assert_eq!(
            types.get("cache").map(String::as_str),
            Some("LinkedHashMap")
        );
    }

    #[test]
    fn a_name_bound_to_a_typed_name_shares_its_type() {
        let mut types: TypeMap = [(
            "_subscription".to_string(),
            "StreamSubscription".to_string(),
        )]
        .into_iter()
        .collect();
        types.insert("kept".to_string(), "Own".to_string());
        let body = "final previous = _subscription;\nvar again = previous;\n\
                    kept = _subscription;\nif (a == b) {}\ncount += step;\nx = null;";
        resolve_aliases(body, &mut types);
        assert_eq!(
            types.get("previous").map(String::as_str),
            Some("StreamSubscription")
        );
        assert_eq!(
            types.get("again").map(String::as_str),
            Some("StreamSubscription")
        );
        assert_eq!(
            types.get("kept").map(String::as_str),
            Some("Own"),
            "its own type stays"
        );
        for untouched in ["a", "count", "x"] {
            assert_eq!(types.get(untouched), None, "{untouched}");
        }
    }
}
