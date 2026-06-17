//! Tree-sitter node helper functions.
//!
//! Utility functions for extracting text and navigating tree-sitter AST nodes.
//! Used by all language-specific extractors.

use tree_sitter::Node;

/// Helper to extract text from a node.
pub fn node_text<'a>(node: &Node, source: &'a str) -> &'a str {
    let start = node.start_byte();
    let end = node.end_byte();
    &source[start..end]
}

/// Helper to find a child node by kind.
pub fn find_child_by_kind<'a>(node: &'a Node<'a>, kind: &str) -> Option<Node<'a>> {
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if child.kind() == kind {
            return Some(child);
        }
    }
    None
}

/// Helper to find all children of a specific kind.
pub fn find_children_by_kind<'a>(node: &'a Node<'a>, kind: &str) -> Vec<Node<'a>> {
    let mut cursor = node.walk();
    node.children(&mut cursor)
        .filter(|child| child.kind() == kind)
        .collect()
}

/// Default call-expression node kinds recognized across grammars when a
/// language declares no extra `call_nodes` in the registry: the C-family / JS
/// `call_expression`, Python/Ruby `call`, C# `invocation_expression`, the
/// generic `function_call`, and Java/Kotlin `method_invocation` /
/// `object_creation_expression`.
pub const DEFAULT_CALL_NODE_KINDS: &[&str] = &[
    "call_expression",
    "function_call",
    "invocation_expression",
    "call",
    "method_invocation",
    "object_creation_expression",
];

/// Whether `kind` is a call-expression node. The registry-supplied
/// `extra_call_kinds` (per-language `SemanticPatterns::call_nodes`) are matched
/// IN ADDITION to [`DEFAULT_CALL_NODE_KINDS`], so a language extends recognition
/// (e.g. PHP `member_call_expression`) without losing the common ones.
fn is_call_node(kind: &str, extra_call_kinds: &[String]) -> bool {
    DEFAULT_CALL_NODE_KINDS.contains(&kind) || extra_call_kinds.iter().any(|k| k == kind)
}

/// Helper to extract function calls from a node.
///
/// `extra_call_kinds` are language-specific call-node kinds from the registry
/// (`SemanticPatterns::call_nodes`); pass an empty slice to recognize only the
/// universal [`DEFAULT_CALL_NODE_KINDS`].
pub fn extract_function_calls(
    node: &Node,
    source: &str,
    extra_call_kinds: &[String],
    arg_call_kinds: &[String],
) -> Vec<String> {
    let mut calls = Vec::new();
    let mut cursor = node.walk();

    fn push_call(calls: &mut Vec<String>, name: &str) {
        // Reduce the callee expression to its bare function name.
        // Generic/turbofish arguments are stripped first, so a call like
        // `foo::<String, _>()` yields `foo` rather than the type-argument
        // fragments `<String` / `_>`.
        if let Some(clean_name) = clean_callee_name(name) {
            if !calls.contains(&clean_name) {
                calls.push(clean_name);
            }
        }
    }

    fn visit(
        node: &Node,
        source: &str,
        calls: &mut Vec<String>,
        cursor: &mut tree_sitter::TreeCursor,
        extra_call_kinds: &[String],
        arg_call_kinds: &[String],
    ) {
        if is_call_node(node.kind(), extra_call_kinds) {
            // Resolve the callee node. `function`/`callee` cover the C-family
            // and JS grammars; `name` is the method identifier on Java/Kotlin
            // `method_invocation`; `type` is the constructed class on
            // `object_creation_expression`. `child(0)` is the last-resort
            // fallback for bare `call` nodes — kept LAST so it never wins over
            // the named fields above (which would otherwise yield the receiver
            // object `a` for `a.b()` instead of `b`).
            if let Some(callee) = node
                .child_by_field_name("function")
                .or_else(|| node.child_by_field_name("callee"))
                .or_else(|| node.child_by_field_name("name"))
                .or_else(|| node.child_by_field_name("type"))
                .or_else(|| node.child(0))
            {
                push_call(calls, node_text(&callee, source));
            }
        } else if !arg_call_kinds.is_empty() && arg_call_kinds.iter().any(|k| k == node.kind()) {
            // Postfix/selector grammars (Dart) have no discrete call node: an
            // invocation parses as `<callee> (selector (argument_part …))` with
            // the callee as a *sibling* of the argument wrapper, not a child
            // field. Resolve it from the argument node's position instead.
            if let Some(name) = resolve_postfix_callee(node, source) {
                push_call(calls, &name);
            }
        }

        // Visit children
        for i in 0..node.child_count() {
            if let Some(child) = node.child(i as u32) {
                visit(
                    &child,
                    source,
                    calls,
                    cursor,
                    extra_call_kinds,
                    arg_call_kinds,
                );
            }
        }
    }

    visit(
        node,
        source,
        &mut calls,
        &mut cursor,
        extra_call_kinds,
        arg_call_kinds,
    );
    calls
}

/// Resolve the callee name for a postfix/selector call grammar from the
/// argument-list wrapper node (Dart's `argument_part`).
///
/// Dart shapes an invocation as a chain of siblings:
/// - `foo(a)` → `(identifier foo) (selector (argument_part …))` — the callee is
///   the `selector`'s preceding sibling, the base `identifier`.
/// - `recv.method(a)` → `… (selector (… (identifier method))) (selector
///   (argument_part …))` — the callee is the method `identifier` inside the
///   `selector` preceding the argument selector.
/// - `..method(a)` (cascade) → `(cascade_section (cascade_selector (identifier
///   method)) (argument_part …))` — the callee is the argument's preceding
///   sibling within the cascade.
///
/// Returns `None` for call-on-call (`foo()()`) and other shapes with no
/// identifiable callee; downstream `is_valid_symbol_name` discards stragglers.
fn resolve_postfix_callee(arg_node: &Node, source: &str) -> Option<String> {
    let parent = arg_node.parent()?;
    let prev = if parent.kind() == "cascade_section" {
        arg_node.prev_sibling()?
    } else {
        // `parent` is the `selector` wrapping the argument list; the callee sits
        // before that selector.
        parent.prev_sibling()?
    };

    if prev.kind() == "identifier" {
        return Some(node_text(&prev, source).to_string());
    }
    // A preceding `selector` that itself applies arguments is a call-on-call —
    // there is no named callee to attribute, so skip it.
    if prev.kind() == "selector" {
        let mut cursor = prev.walk();
        if prev.children(&mut cursor).any(|c| c.kind() == "argument_part") {
            return None;
        }
    }
    first_identifier_descendant(&prev, source).map(|s| s.to_string())
}

/// Depth-first search for the first `identifier` node at or under `node`.
fn first_identifier_descendant<'a>(node: &Node, source: &'a str) -> Option<&'a str> {
    if node.kind() == "identifier" {
        return Some(node_text(node, source));
    }
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if let Some(found) = first_identifier_descendant(&child, source) {
            return Some(found);
        }
    }
    None
}

/// Reduce a callee expression to its bare function name.
///
/// Strips balanced generic/turbofish argument lists (`foo::<T>` → `foo`,
/// `Vec::<u8>::new` → `new`) and qualifier paths (`a::b::c` → `c`,
/// `obj.method` → `method`). Returns `None` when nothing identifier-like
/// remains. Stripping generics here keeps type-argument fragments such as
/// `<String` or `_>` (and the comma between them) out of the call list at the
/// source, rather than relying on a downstream filter to discard them.
fn clean_callee_name(name: &str) -> Option<String> {
    let stripped = strip_generic_args(name);
    let after_colons = stripped.rsplit("::").find(|s| !s.is_empty()).unwrap_or("");
    let base = after_colons
        .rsplit('.')
        .find(|s| !s.is_empty())
        .unwrap_or("")
        .trim();
    if base.is_empty() {
        None
    } else {
        Some(base.to_string())
    }
}

/// Remove balanced `<...>` generic/turbofish sections from a callee expression.
///
/// Characters inside angle brackets (including the commas that separate type
/// arguments) are dropped; everything at bracket depth zero is kept. Nesting is
/// handled so `Foo<Bar<Baz>>::method` collapses cleanly.
fn strip_generic_args(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut depth: u32 = 0;
    for ch in s.chars() {
        match ch {
            '<' => depth += 1,
            '>' => depth = depth.saturating_sub(1),
            _ if depth == 0 => out.push(ch),
            _ => {}
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::{clean_callee_name, strip_generic_args};

    #[test]
    fn strip_generics_removes_balanced_sections() {
        assert_eq!(strip_generic_args("foo::<String, _>"), "foo::");
        assert_eq!(strip_generic_args("Vec::<u8>::new"), "Vec::::new");
        assert_eq!(strip_generic_args("Foo<Bar<Baz>>::method"), "Foo::method");
        assert_eq!(strip_generic_args("plain"), "plain");
    }

    #[test]
    fn clean_callee_strips_turbofish() {
        // The turbofish must not leak `<String` / `_>` into the call list.
        assert_eq!(
            clean_callee_name("foo::<String, _>").as_deref(),
            Some("foo")
        );
        assert_eq!(
            clean_callee_name("query::<String, _>").as_deref(),
            Some("query")
        );
    }

    #[test]
    fn clean_callee_keeps_last_segment() {
        assert_eq!(clean_callee_name("println").as_deref(), Some("println"));
        assert_eq!(
            clean_callee_name("std::collections::HashMap::new").as_deref(),
            Some("new")
        );
        assert_eq!(clean_callee_name("Vec::<u8>::new").as_deref(), Some("new"));
        assert_eq!(
            clean_callee_name("self.process").as_deref(),
            Some("process")
        );
        assert_eq!(
            clean_callee_name("obj.method::<T>").as_deref(),
            Some("method")
        );
    }

    #[test]
    fn clean_callee_rejects_pure_generic() {
        // A callee that is nothing but a (mangled) generic list has no name.
        assert_eq!(clean_callee_name("<String, _>"), None);
        assert_eq!(clean_callee_name(""), None);
    }
}
