//! Rust inline-test detection for the generic AST chunker.
//!
//! A Rust unit test usually lives in the *same production `.rs` file* as the
//! code it exercises — `#[cfg(test)] mod tests { #[test] fn foo() {…} }` — so a
//! file-path test check (`is_test_file`) cannot see it. This module tags such
//! symbols at extraction time from the tree structure, so the code graph can
//! mark their nodes and call-graph test-gap detection seeds the BFS from inline
//! tests instead of reading the production symbols they call as untested gaps.
//!
//! Rust only: `#[cfg(test)]` and the `#[test]`-family attributes are Rust
//! syntax, and the node kinds we inspect (`attribute_item`, `mod_item`) are
//! tree-sitter-rust kinds. Callers gate on `language == "rust"`.
//!
//! In tree-sitter-rust, outer attributes are `attribute_item` nodes that are
//! **siblings preceding** the item they annotate (the item node's byte range
//! does NOT include them — the same reason the docstring extractor walks
//! `prev_sibling`). Detection therefore walks preceding siblings for attributes
//! and ancestors for an enclosing `#[cfg(test)]` module.

use tree_sitter::Node;

use crate::tree_sitter::chunker::helpers::node_text;

/// True if `node` is a Rust test symbol independent of file path — it carries a
/// test-function attribute (`#[test]`, `#[tokio::test]` / other `*::test`,
/// `#[rstest]`, `#[test_case]`) or it is (or is nested inside) a `#[cfg(test)]`
/// module. Helper functions inside a `#[cfg(test)]` module are included on
/// purpose: they are test code that exercises production symbols, so seeding the
/// gap BFS from them is correct.
pub(super) fn rust_is_test_node(node: &Node, source: &str) -> bool {
    // 1. The node's OWN attributes mark it test-only: a test-function attribute
    //    (`#[test]` &c.) or a direct `#[cfg(test)]` gate on any item (a
    //    cfg(test) fn/struct/mod is test-only code). This also tags the
    //    `#[cfg(test)] mod tests` container itself.
    if any_preceding_attr(node, source, |t| {
        attr_text_marks_test_fn(t) || attr_text_is_cfg_test(t)
    }) {
        return true;
    }
    // 2. The node is nested inside a `#[cfg(test)]` module (the common idiom for
    //    inline tests and their non-`#[test]` helpers). Walk ancestors from the
    //    parent — the node itself was covered in step 1.
    let mut cur = node.parent();
    while let Some(n) = cur {
        if n.kind() == "mod_item" && any_preceding_attr(&n, source, attr_text_is_cfg_test) {
            return true;
        }
        cur = n.parent();
    }
    false
}

/// Apply `pred` to each `attribute_item` immediately preceding `node`, skipping
/// intervening doc/line comments (an attribute may sit above or below a `///`
/// doc comment). Stops at the first non-attribute, non-comment sibling.
fn any_preceding_attr(node: &Node, source: &str, pred: impl Fn(&str) -> bool) -> bool {
    let mut sib = node.prev_sibling();
    while let Some(s) = sib {
        match s.kind() {
            "attribute_item" | "inner_attribute_item" => {
                if pred(node_text(&s, source)) {
                    return true;
                }
            }
            // Doc/line comments can appear between the attribute and the item.
            "line_comment" | "block_comment" => {}
            _ => break,
        }
        sib = s.prev_sibling();
    }
    false
}

/// The path + argument text inside `#[ … ]` / `#![ … ]`, e.g. `tokio::test`,
/// `cfg(test)`, `test_case(1, 2)`. Returns `None` when the text is not an
/// attribute.
fn attr_inner(attr_text: &str) -> Option<&str> {
    let t = attr_text.trim();
    let t = t.strip_prefix("#")?;
    let t = t.strip_prefix("!").unwrap_or(t); // inner attribute `#![…]`
    let t = t.strip_prefix("[")?;
    let t = t.strip_suffix("]")?;
    Some(t.trim())
}

/// The attribute *path* — everything up to the first `(` or whitespace, e.g.
/// `#[tokio::test]` → `tokio::test`, `#[test_case(1)]` → `test_case`.
fn attr_path(inner: &str) -> &str {
    inner
        .split(|c: char| c == '(' || c.is_whitespace())
        .next()
        .unwrap_or("")
        .trim()
}

/// True for a test-function attribute: last path segment `test`
/// (`#[test]`, `#[tokio::test]`, `#[async_std::test]`, `#[actix_rt::test]`,
/// `#[googletest::test]`), or the whole path `rstest` / `test_case`.
fn attr_text_marks_test_fn(attr_text: &str) -> bool {
    let Some(inner) = attr_inner(attr_text) else {
        return false;
    };
    let path = attr_path(inner);
    let last = path.rsplit("::").next().unwrap_or(path);
    last == "test" || path == "rstest" || path == "test_case"
}

/// True for `#[cfg(test)]` and `test`-bearing cfg predicates
/// (`#[cfg(all(test, unix))]`, `#[cfg(any(unix, test))]`). String literals are
/// stripped first so `#[cfg(feature = "test")]` (a feature literally named
/// "test") does NOT count.
fn attr_text_is_cfg_test(attr_text: &str) -> bool {
    let Some(inner) = attr_inner(attr_text) else {
        return false;
    };
    if attr_path(inner) != "cfg" {
        return false;
    }
    let mut without_strings = String::with_capacity(inner.len());
    let mut in_str = false;
    for c in inner.chars() {
        if c == '"' {
            in_str = !in_str;
            continue;
        }
        if !in_str {
            without_strings.push(c);
        }
    }
    without_strings
        .split(|c: char| !(c.is_alphanumeric() || c == '_'))
        .any(|tok| tok == "test")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_fn_attrs_are_recognized() {
        for a in [
            "#[test]",
            "#[tokio::test]",
            "#[async_std::test]",
            "#[actix_rt::test]",
            "#[googletest::test]",
            "#[rstest]",
            "#[test_case(1, 2)]",
        ] {
            assert!(attr_text_marks_test_fn(a), "{a} should mark a test fn");
        }
        for a in [
            "#[cfg(test)]",
            "#[should_panic]",
            "#[inline]",
            "#[derive(Debug)]",
        ] {
            assert!(!attr_text_marks_test_fn(a), "{a} is NOT a test-fn attr");
        }
    }

    #[test]
    fn cfg_test_predicates_are_recognized() {
        for a in [
            "#[cfg(test)]",
            "#[cfg(all(test, unix))]",
            "#[cfg(any(unix, test))]",
            "#![cfg(test)]",
        ] {
            assert!(attr_text_is_cfg_test(a), "{a} should be cfg(test)");
        }
        // Not cfg(test): a feature literally named "test", or unrelated cfgs.
        for a in [
            "#[cfg(feature = \"test\")]",
            "#[cfg(unix)]",
            "#[test]",
            "#[cfg(all(unix, feature = \"testing\"))]",
        ] {
            assert!(!attr_text_is_cfg_test(a), "{a} is NOT cfg(test)");
        }
    }
}
