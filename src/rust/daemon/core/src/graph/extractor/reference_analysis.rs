//! Symbols named in argument position without being invoked (REFERENCES edges).
//!
//! `ref.watch(activeContextProvider)` calls `watch`; `activeContextProvider` is
//! passed by reference. That is neither a call nor a type use, so before this
//! module such a symbol had no incoming edge and `usages` answered 0 for it
//! (#369). Measured on DOC-V2: 1 of 506 Dart top-level constants had any
//! inbound edge at all.
//!
//! **Scope: Dart only, lower-camel identifiers.** Both limits are deliberate.
//! The mechanism is not Dart-specific — any language that passes symbols by
//! reference has it — but the blast radius was measured on Dart and it should
//! not be extrapolated. Upper-camel identifiers (`find.byType(HomePage)`) are
//! the same class of miss and are also uncovered today, but they are a
//! different population that deserves its own measurement before being added.
//!
//! **Why text parsing rather than the AST.** `SemanticChunk` carries call
//! NAMES, not arguments, so the AST route means a new chunker field, a
//! `CHUNKER_LOGIC_VERSION` bump and a full re-chunk of every tenant. The
//! observable result is the same, so this follows `type_analysis` and
//! `import_parsers` and parses the chunk text. What that costs is precision on
//! the extraction side, which the resolver then absorbs: a name that is not a
//! real top-level symbol becomes a stub and its edge is dropped before it
//! persists. Measured on DOC-V2, that is the difference between 51,199 edges
//! (+8.5%) and 1,719 (+0.28%).
//!
//! Strings and comments ARE stripped first. Without that a symbol name quoted
//! inside a log message would produce a real, wrong, persisted edge — the
//! resolver cannot catch that one, because the name does resolve.

/// Extract identifiers named in argument position.
///
/// Returns each distinct name once, in first-seen order so the edge set is
/// deterministic for a given chunk.
pub fn extract_argument_references(content: &str, language: &str) -> Vec<String> {
    // Dart only for now — see the module docs.
    if !language.eq_ignore_ascii_case("dart") {
        return Vec::new();
    }

    let stripped = strip_comments_and_strings(content);
    let bytes = stripped.as_bytes();

    let mut refs = Vec::new();
    let mut seen = std::collections::HashSet::new();
    let mut i = 0usize;

    while i < bytes.len() {
        // An argument slot opens after `(` or `,`.
        if bytes[i] != b'(' && bytes[i] != b',' {
            i += 1;
            continue;
        }
        let mut j = i + 1;
        while j < bytes.len() && (bytes[j] as char).is_ascii_whitespace() {
            j += 1;
        }
        let start = j;
        while j < bytes.len() && is_ident_byte(bytes[j]) {
            j += 1;
        }
        if j == start {
            i += 1;
            continue;
        }
        let end = j;
        while j < bytes.len() && (bytes[j] as char).is_ascii_whitespace() {
            j += 1;
        }
        // The slot must CLOSE right after the identifier. Anything else means
        // the identifier was the head of a larger expression — `foo.bar`,
        // `foo(1)`, `a + b`, `name: value` — and the bare name is not what is
        // being passed.
        let closes = j < bytes.len() && (bytes[j] == b',' || bytes[j] == b')');
        if closes {
            let name = &stripped[start..end];
            if is_referencable_identifier(name) && seen.insert(name.to_string()) {
                refs.push(name.to_string());
            }
        }
        // Resume at the delimiter we landed on so `f(a, b, c)` yields all three.
        i = if j > i { j } else { i + 1 };
    }

    refs
}

fn is_ident_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_' || b == b'$'
}

/// Lower-camel (or `_`-prefixed) identifiers only, and never a reserved word or
/// literal. `true`/`null`/`this` are argument-position tokens in real code and
/// would otherwise become stub nodes on every file that passes one.
fn is_referencable_identifier(name: &str) -> bool {
    let mut chars = name.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    if !(first.is_ascii_lowercase() || first == '_') {
        return false;
    }
    // A bare `_` (Dart's throwaway parameter) names nothing.
    if name.chars().all(|c| c == '_') {
        return false;
    }
    !is_dart_reserved(name)
}

fn is_dart_reserved(name: &str) -> bool {
    matches!(
        name,
        "abstract"
            | "as"
            | "assert"
            | "async"
            | "await"
            | "base"
            | "break"
            | "case"
            | "catch"
            | "class"
            | "const"
            | "continue"
            | "covariant"
            | "default"
            | "deferred"
            | "do"
            | "dynamic"
            | "else"
            | "enum"
            | "export"
            | "extends"
            | "extension"
            | "external"
            | "factory"
            | "false"
            | "final"
            | "finally"
            | "for"
            | "get"
            | "hide"
            | "if"
            | "implements"
            | "import"
            | "in"
            | "interface"
            | "is"
            | "late"
            | "library"
            | "mixin"
            | "new"
            | "null"
            | "on"
            | "operator"
            | "part"
            | "required"
            | "rethrow"
            | "return"
            | "sealed"
            | "set"
            | "show"
            | "static"
            | "super"
            | "switch"
            | "sync"
            | "this"
            | "throw"
            | "true"
            | "try"
            | "typedef"
            | "var"
            | "void"
            | "when"
            | "while"
            | "with"
            | "yield"
    )
}

/// Blank out comments and string literals, preserving byte offsets so the
/// caller can slice the result directly.
///
/// Replacement is space-for-byte rather than removal: it keeps `(` / `,` /
/// identifier positions intact and cannot accidentally join two tokens that
/// were separated only by a string.
fn strip_comments_and_strings(src: &str) -> String {
    let bytes = src.as_bytes();
    let mut out: Vec<u8> = bytes.to_vec();
    let mut i = 0usize;

    // Blank [from, to) but keep newlines, so line structure survives.
    let blank = |out: &mut Vec<u8>, from: usize, to: usize| {
        for b in out.iter_mut().take(to).skip(from) {
            if *b != b'\n' {
                *b = b' ';
            }
        }
    };

    while i < bytes.len() {
        match bytes[i] {
            b'/' if i + 1 < bytes.len() && bytes[i + 1] == b'/' => {
                let start = i;
                while i < bytes.len() && bytes[i] != b'\n' {
                    i += 1;
                }
                blank(&mut out, start, i);
            }
            b'/' if i + 1 < bytes.len() && bytes[i + 1] == b'*' => {
                let start = i;
                i += 2;
                while i + 1 < bytes.len() && !(bytes[i] == b'*' && bytes[i + 1] == b'/') {
                    i += 1;
                }
                i = (i + 2).min(bytes.len());
                blank(&mut out, start, i);
            }
            q @ (b'\'' | b'"') => {
                let start = i;
                // Triple-quoted strings close only on a matching triple.
                let triple = i + 2 < bytes.len() && bytes[i + 1] == q && bytes[i + 2] == q;
                i += if triple { 3 } else { 1 };
                while i < bytes.len() {
                    if bytes[i] == b'\\' {
                        i += 2;
                        continue;
                    }
                    if bytes[i] == q {
                        if triple {
                            if i + 2 < bytes.len() && bytes[i + 1] == q && bytes[i + 2] == q {
                                i += 3;
                                break;
                            }
                        } else {
                            i += 1;
                            break;
                        }
                    }
                    i += 1;
                }
                i = i.min(bytes.len());
                blank(&mut out, start, i);
            }
            _ => i += 1,
        }
    }

    // Every replacement is a single ASCII byte swapped for another single ASCII
    // byte, so the result is still valid UTF-8 with identical offsets.
    String::from_utf8(out).unwrap_or_else(|_| src.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_the_reported_riverpod_idiom() {
        let src = "Widget build(BuildContext context, WidgetRef ref) {\n  \
                   final ctx = ref.watch(activeContextProvider);\n  return X();\n}";
        assert_eq!(
            extract_argument_references(src, "dart"),
            vec!["activeContextProvider"]
        );
    }

    #[test]
    fn every_slot_of_a_multi_argument_call_is_seen() {
        let refs = extract_argument_references("f(alpha, beta, gamma);", "dart");
        assert_eq!(refs, vec!["alpha", "beta", "gamma"]);
    }

    #[test]
    fn a_name_is_reported_once_however_often_it_appears() {
        let refs = extract_argument_references("f(dup); g(dup); h(dup);", "dart");
        assert_eq!(refs, vec!["dup"]);
    }

    /// The bare name has to BE the argument. A head of a larger expression is
    /// not passed by reference, and treating it as one would wire the caller to
    /// a receiver it merely reads a member from.
    #[test]
    fn expression_heads_are_not_references() {
        assert!(extract_argument_references("f(obj.field);", "dart").is_empty());
        assert!(extract_argument_references("f(other(1));", "dart").is_empty());
        assert!(extract_argument_references("f(a + b);", "dart").is_empty());
        assert!(extract_argument_references("f(name: value);", "dart").is_empty());
        assert!(extract_argument_references("f(list[0]);", "dart").is_empty());
    }

    #[test]
    fn reserved_words_and_literals_are_not_symbols() {
        let refs = extract_argument_references("f(true, null, this, _, context);", "dart");
        assert_eq!(refs, vec!["context"]);
    }

    #[test]
    fn upper_camel_is_out_of_scope_for_now() {
        // `find.byType(HomePage)` is the same class of miss, but a different
        // population — see the module docs.
        assert!(extract_argument_references("find.byType(HomePage);", "dart").is_empty());
    }

    /// The resolver cannot save us here: a symbol name quoted in a log line
    /// DOES resolve, so a wrong edge would persist. Stripping is load-bearing.
    #[test]
    fn a_symbol_name_inside_a_string_is_not_a_reference() {
        assert!(
            extract_argument_references("log('watching activeContextProvider');", "dart")
                .is_empty()
        );
        assert!(
            extract_argument_references("log(\"see (activeContextProvider)\");", "dart").is_empty()
        );
        assert!(
            extract_argument_references("log('''see (activeContextProvider)''');", "dart")
                .is_empty()
        );
    }

    #[test]
    fn commented_out_code_is_not_a_reference() {
        assert!(extract_argument_references("// ref.watch(oldProvider);", "dart").is_empty());
        assert!(extract_argument_references("/* ref.watch(oldProvider); */", "dart").is_empty());
        // ...but live code on the line after a comment still counts.
        assert_eq!(
            extract_argument_references("// gone\nf(live);", "dart"),
            vec!["live"]
        );
    }

    #[test]
    fn other_languages_are_untouched() {
        for lang in ["rust", "typescript", "python", "java", ""] {
            assert!(
                extract_argument_references("f(someProvider);", lang).is_empty(),
                "{lang} must be unaffected until its own blast radius is measured"
            );
        }
    }

    #[test]
    fn stripping_preserves_byte_offsets() {
        // A multi-byte character before the call would corrupt the slice
        // indices if stripping changed the byte length.
        let refs = extract_argument_references("// ç\nf(afterAccent);", "dart");
        assert_eq!(refs, vec!["afterAccent"]);
    }
}
