//! Import statement parsers for Rust, Python, JavaScript/TypeScript, Go,
//! Java/Kotlin, and Dart.
//!
//! Each parser extracts imported symbol names from a single line of source code.

use super::super::{compute_node_id, EdgeType, GraphEdge, GraphNode, NodeType};
use super::ExtractionResult;

/// Extract IMPORTS edges from preamble content.
///
/// Parses import/use statements to create edges from the File node to
/// imported symbols. Uses simple line-by-line regex-free matching for
/// the most common patterns.
pub(crate) fn extract_imports_from_content(
    content: &str,
    language: &str,
    tenant_id: &str,
    file_path: &str,
    result: &mut ExtractionResult,
) {
    let file_node_id = compute_node_id(tenant_id, file_path, file_path, NodeType::File);

    for line in content.lines() {
        let line = line.trim();
        // R4: retain the module/path locator for this import line so
        // `resolve_stub_edges` can anchor an ambiguous call to the definition file
        // the caller actually imported. All symbols on one line share the locator.
        let module = parse_import_module(line, language);
        let imports = parse_import_line(line, language);
        for symbol in imports {
            if symbol.is_empty() || symbol.len() < 2 {
                continue;
            }
            let stub = GraphNode::stub(tenant_id, &symbol, NodeType::Module);
            let mut edge = GraphEdge::new(
                tenant_id,
                &file_node_id,
                &stub.node_id,
                EdgeType::Imports,
                file_path,
            );
            if let Some(ref m) = module {
                edge.metadata_json = Some(format!("{{\"module\":{}}}", json_quote(m)));
            }
            result.nodes.push(stub);
            result.edges.push(edge);
        }
    }
}

/// Minimal JSON string-quoter for the import module locator (escapes the only
/// two characters that can break the object: `"` and `\`). Import paths are
/// otherwise plain (`::`, `.`, `/`, identifiers), so this is sufficient and
/// avoids a serde_json round-trip for one field.
fn json_quote(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            _ => out.push(c),
        }
    }
    out.push('"');
    out
}

/// Extract the MODULE / path locator from an import line — the "where" the
/// imported symbols come from (the path before the imported names, or the quoted
/// URI). Retained on the IMPORTS edge metadata for R4 import-anchored resolution;
/// normalization to comparable segments happens at match time. Returns None when
/// the line is not an import or carries no usable locator (wildcards included).
pub(crate) fn parse_import_module(line: &str, language: &str) -> Option<String> {
    let line = line.trim();
    let non_empty = |s: &str| {
        let s = s.trim();
        (!s.is_empty()).then(|| s.to_string())
    };
    match language {
        "rust" => {
            let l = line.trim_end_matches(';');
            let path = l.strip_prefix("use ")?.trim();
            if path.ends_with("::*") {
                return None;
            }
            // Grouped `use a::b::{...}` -> `a::b`.
            if let Some(brace) = path.find('{') {
                return non_empty(path[..brace].trim().trim_end_matches("::"));
            }
            // Simple `use a::b::C` -> `a::b`.
            if let Some(pos) = path.rfind("::") {
                return non_empty(&path[..pos]);
            }
            non_empty(path) // `use serde`
        }
        "python" => {
            if let Some(rest) = line.strip_prefix("from ") {
                return non_empty(rest.split(" import ").next()?);
            }
            if let Some(rest) = line.strip_prefix("import ") {
                let first = rest.split(',').next()?.trim();
                return non_empty(first.split(" as ").next().unwrap_or(first));
            }
            None
        }
        "javascript" | "typescript" | "tsx" | "jsx" => {
            let l = line.trim_end_matches(';');
            let after = &l[l.find(" from ")? + 6..];
            non_empty(
                after
                    .trim()
                    .trim_matches(|c| c == '\'' || c == '"' || c == '`'),
            )
        }
        "go" => {
            let start = line.find('"')?;
            let end = line[start + 1..].find('"')?;
            non_empty(&line[start + 1..start + 1 + end])
        }
        "java" | "kotlin" => {
            let l = line.trim_end_matches(';').trim();
            let rest = l
                .strip_prefix("import static ")
                .or_else(|| l.strip_prefix("import "))?
                .trim();
            let rest = rest.split(" as ").next().unwrap_or(rest).trim();
            if rest.ends_with(".*") {
                return non_empty(rest.trim_end_matches(".*"));
            }
            non_empty(rest)
        }
        "dart" => {
            if !(line.starts_with("import ") || line.starts_with("export ")) {
                return None;
            }
            let start = line.find(['\'', '"'])?;
            let quote = line.as_bytes()[start] as char;
            let rel_end = line[start + 1..].find(quote)?;
            non_empty(&line[start + 1..start + 1 + rel_end])
        }
        _ => None,
    }
}

/// Parse a single import/use line and return imported symbol names.
fn parse_import_line(line: &str, language: &str) -> Vec<String> {
    match language {
        "rust" => parse_rust_use(line),
        "python" => parse_python_import(line),
        "javascript" | "typescript" | "tsx" | "jsx" => parse_js_import(line),
        "go" => parse_go_import(line),
        "java" | "kotlin" => parse_java_import(line),
        "dart" => parse_dart_import(line),
        _ => vec![],
    }
}

/// Parse Rust `use` statements.
///
/// Examples:
/// - `use std::collections::HashMap;` -> ["HashMap"]
/// - `use crate::graph::{GraphNode, GraphEdge};` -> ["GraphNode", "GraphEdge"]
/// - `use super::*;` -> [] (wildcard, skip)
pub(crate) fn parse_rust_use(line: &str) -> Vec<String> {
    let line = line.trim().trim_end_matches(';');
    if !line.starts_with("use ") {
        return vec![];
    }
    let path = line[4..].trim();

    // Skip wildcard imports
    if path.ends_with("::*") {
        return vec![];
    }

    // Grouped imports: `use foo::{A, B, C};`
    if let Some(brace_start) = path.find('{') {
        if let Some(brace_end) = path.find('}') {
            let items = &path[brace_start + 1..brace_end];
            return items
                .split(',')
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty() && *s != "self" && *s != "*")
                .collect();
        }
    }

    // Simple import: `use std::collections::HashMap`
    // Take the last path component
    if let Some(pos) = path.rfind("::") {
        let name = path[pos + 2..].trim();
        if !name.is_empty() && name != "self" {
            return vec![name.to_string()];
        }
    }

    // Single-segment use (rare): `use serde;`
    if !path.contains("::") && !path.is_empty() {
        return vec![path.to_string()];
    }

    vec![]
}

/// Parse Python import statements.
///
/// Examples:
/// - `import numpy` -> ["numpy"]
/// - `from pathlib import Path` -> ["Path"]
/// - `from typing import Dict, List, Optional` -> ["Dict", "List", "Optional"]
pub(crate) fn parse_python_import(line: &str) -> Vec<String> {
    let line = line.trim();

    if line.starts_with("from ") {
        // `from X import Y, Z`
        if let Some(import_pos) = line.find(" import ") {
            let items = &line[import_pos + 8..];
            return items
                .split(',')
                .map(|s| {
                    // Handle `as` aliases: `import X as Y` -> X
                    let s = s.trim();
                    if let Some(as_pos) = s.find(" as ") {
                        s[..as_pos].trim().to_string()
                    } else {
                        s.to_string()
                    }
                })
                .filter(|s| !s.is_empty() && *s != "*")
                .collect();
        }
    } else if line.starts_with("import ") {
        // `import X, Y`
        let items = &line[7..];
        return items
            .split(',')
            .map(|s| {
                let s = s.trim();
                if let Some(as_pos) = s.find(" as ") {
                    s[..as_pos].trim().to_string()
                } else {
                    s.to_string()
                }
            })
            .filter(|s| !s.is_empty())
            .collect();
    }

    vec![]
}

/// Parse JavaScript/TypeScript import statements.
///
/// Examples:
/// - `import { Component, useState } from 'react';` -> ["Component", "useState"]
/// - `import React from 'react';` -> ["React"]
/// - `import * as path from 'path';` -> [] (namespace import, skip)
pub(crate) fn parse_js_import(line: &str) -> Vec<String> {
    let line = line.trim().trim_end_matches(';');

    if !line.starts_with("import ") {
        return vec![];
    }
    let rest = &line[7..].trim();

    // Skip `import * as X from ...`
    if rest.starts_with("* as") || rest.starts_with("* ") {
        return vec![];
    }

    // Named imports: `import { A, B } from '...'`
    if let Some(brace_start) = rest.find('{') {
        if let Some(brace_end) = rest.find('}') {
            let items = &rest[brace_start + 1..brace_end];
            return items
                .split(',')
                .map(|s| {
                    let s = s.trim();
                    // Handle `X as Y` -> X
                    if let Some(as_pos) = s.find(" as ") {
                        s[..as_pos].trim().to_string()
                    } else {
                        s.to_string()
                    }
                })
                .filter(|s| !s.is_empty())
                .collect();
        }
    }

    // Default import: `import React from '...'`
    if let Some(from_pos) = rest.find(" from ") {
        let name = rest[..from_pos].trim();
        if !name.is_empty() && !name.contains('{') {
            return vec![name.to_string()];
        }
    }

    vec![]
}

/// Parse Go import statements (single line within import block).
///
/// Examples:
/// - `"fmt"` -> ["fmt"]
/// - `"encoding/json"` -> ["json"]
/// - `alias "some/package"` -> ["package"]
pub(crate) fn parse_go_import(line: &str) -> Vec<String> {
    let line = line.trim();

    // Skip `import (` and `)` lines
    if line.starts_with("import") || line == "(" || line == ")" {
        return vec![];
    }

    // Extract the quoted path
    if let Some(start) = line.find('"') {
        if let Some(end) = line[start + 1..].find('"') {
            let path = &line[start + 1..start + 1 + end];
            // Use last path segment as the import name
            let name = path.rsplit('/').next().unwrap_or(path);
            if !name.is_empty() {
                return vec![name.to_string()];
            }
        }
    }

    vec![]
}

/// Parse Java/Kotlin import statements.
///
/// Examples:
/// - `import com.example.Foo;` -> ["Foo"]
/// - `import static com.example.Bar.baz;` -> ["baz"]
/// - `import com.example.Foo as Bar` (Kotlin alias) -> ["Foo"]
/// - `import com.example.*;` -> [] (wildcard, skip)
pub(crate) fn parse_java_import(line: &str) -> Vec<String> {
    let line = line.trim().trim_end_matches(';').trim();
    let rest = match line
        .strip_prefix("import static ")
        .or_else(|| line.strip_prefix("import "))
    {
        Some(r) => r.trim(),
        None => return vec![],
    };

    // Drop a Kotlin `as <alias>` suffix; the imported symbol is the path tail.
    let rest = rest.split(" as ").next().unwrap_or(rest).trim();
    if rest.is_empty() || rest.ends_with(".*") {
        return vec![];
    }

    let name = rest.rsplit('.').next().unwrap_or(rest).trim();
    if name.is_empty() {
        vec![]
    } else {
        vec![name.to_string()]
    }
}

/// Parse Dart import/export directives.
///
/// Examples:
/// - `import 'package:flutter/material.dart';` -> ["material"]
/// - `import 'dart:async';` -> ["async"]
/// - `import 'widgets/foo.dart' as foo;` -> ["foo"] (alias wins)
/// - `export 'src/bar.dart';` -> ["bar"]
pub(crate) fn parse_dart_import(line: &str) -> Vec<String> {
    let line = line.trim();
    if !(line.starts_with("import ") || line.starts_with("export ")) {
        return vec![];
    }

    // A trailing `as <alias>` names the binding the file uses locally.
    if let Some(as_pos) = line.find(" as ") {
        let after = line[as_pos + 4..].trim_end_matches(';');
        if let Some(alias) = after.split_whitespace().next() {
            if !alias.is_empty() {
                return vec![alias.to_string()];
            }
        }
    }

    // Otherwise derive a name from the quoted URI's final segment.
    let bytes = line.as_bytes();
    if let Some(start) = line.find(['\'', '"']) {
        let quote = bytes[start] as char;
        if let Some(rel_end) = line[start + 1..].find(quote) {
            let uri = &line[start + 1..start + 1 + rel_end];
            let seg = uri.rsplit('/').next().unwrap_or(uri);
            // `dart:async` -> `async`; a path segment has no colon so this is a
            // no-op there.
            let seg = seg.rsplit(':').next().unwrap_or(seg);
            let name = seg.trim_end_matches(".dart").trim();
            if !name.is_empty() {
                return vec![name.to_string()];
            }
        }
    }

    vec![]
}
