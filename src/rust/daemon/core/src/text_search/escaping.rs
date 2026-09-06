//! FTS5 pattern escaping, LIKE pattern escaping, and glob handling.

use super::types::SearchOptions;
use crate::search_db::SearchDbError;

// ---------------------------------------------------------------------------
// FTS5 pattern escaping
// ---------------------------------------------------------------------------

/// Escape a search pattern for FTS5 trigram MATCH.
///
/// FTS5 trigram tokenizer requires patterns to be double-quote wrapped.
/// Internal double quotes are escaped as `""`.
///
/// Returns `None` if the pattern is shorter than 3 characters (trigram minimum).
pub fn escape_fts5_pattern(pattern: &str) -> Option<String> {
    if pattern.len() < 3 {
        return None;
    }
    let escaped = pattern.replace('"', "\"\"");
    Some(format!("\"{}\"", escaped))
}

/// Escape a LIKE pattern — escape `%`, `_`, and `\` for exact substring match.
pub fn escape_like_pattern(pattern: &str) -> String {
    pattern
        .replace('\\', "\\\\")
        .replace('%', "\\%")
        .replace('_', "\\_")
}

// ---------------------------------------------------------------------------
// Path glob filtering (Task 55)
// ---------------------------------------------------------------------------

/// Extract a deterministic prefix from a glob pattern for SQL pre-filtering.
///
/// Returns the longest prefix before any glob metacharacter (`*`, `?`, `[`).
/// For example, `src/**/*.rs` -> `Some("src/")`, `**/*.rs` -> `None`.
pub(crate) fn extract_glob_prefix(glob: &str) -> Option<String> {
    let end = glob.find(|c: char| c == '*' || c == '?' || c == '[');
    match end {
        Some(0) | None if glob.contains('*') || glob.contains('?') || glob.contains('[') => None,
        Some(pos) => {
            let prefix = &glob[..pos];
            if prefix.is_empty() {
                None
            } else {
                Some(prefix.to_string())
            }
        }
        None => {
            // No metacharacters — treat as exact path match prefix
            Some(glob.to_string())
        }
    }
}

/// Ceiling on how many patterns one glob may expand into. Real patterns
/// (`*.{rs,ts}`, `{src,tests}/**`) are far below it; a pathological nest is not
/// worth compiling. On overflow the pattern is returned verbatim — the braces
/// then match literally, which is a MISS, never a wrong match.
const MAX_BRACE_EXPANSIONS: usize = 256;

/// Locate the first brace group, honouring nesting. `None` when there is no
/// `{`, or when it is unbalanced — an unbalanced brace is a literal.
fn find_brace_group(glob: &str) -> Option<(usize, usize)> {
    let open = glob.find('{')?;
    let mut depth = 0usize;
    for (offset, ch) in glob[open..].char_indices() {
        match ch {
            '{' => depth += 1,
            '}' => {
                depth -= 1;
                if depth == 0 {
                    return Some((open, open + offset));
                }
            }
            _ => {}
        }
    }
    None
}

/// Split a brace body on its TOP-LEVEL commas, so `{a,{b,c}}` yields
/// `["a", "{b,c}"]` instead of splitting the nested group open.
fn split_top_level_commas(body: &str) -> Vec<&str> {
    let mut parts = Vec::new();
    let mut depth = 0usize;
    let mut start = 0usize;
    for (offset, ch) in body.char_indices() {
        match ch {
            '{' => depth += 1,
            '}' => depth = depth.saturating_sub(1),
            ',' if depth == 0 => {
                parts.push(&body[start..offset]);
                start = offset + 1;
            }
            _ => {}
        }
    }
    parts.push(&body[start..]);
    parts
}

fn expand_braces_into(glob: &str, out: &mut Vec<String>) -> bool {
    if out.len() >= MAX_BRACE_EXPANSIONS {
        return false;
    }
    let Some((open, close)) = find_brace_group(glob) else {
        out.push(glob.to_string());
        return true;
    };
    let prefix = &glob[..open];
    let suffix = &glob[close + 1..];
    for alternative in split_top_level_commas(&glob[open + 1..close]) {
        let candidate = format!("{}{}{}", prefix, alternative.trim(), suffix);
        if !expand_braces_into(&candidate, out) {
            return false;
        }
    }
    true
}

/// Expand brace expressions in a glob pattern, left to right.
///
/// `*.{rs,toml}` -> `["*.rs", "*.toml"]`. Multiple groups and nesting are
/// handled (`{src,tests}/*.{rs,ts}`, `{a,{b,c}}`); previously only the first
/// group expanded and its closing brace was taken as the first `}` found,
/// which splits a nested group in the wrong place. If no braces are present,
/// returns the original pattern as a single-element vec.
///
/// The TypeScript side mirrors this exactly (`expandBraces` in
/// `utils/path-glob.ts`), which is the point: this daemon-side expansion has
/// always worked, so `grep pathGlob:"**/*.{rs,ts}"` answered correctly while
/// the same glob returned zero from `list` and semantic `search`. Keep the two
/// implementations in step — alternatives are trimmed on both sides, so a
/// human-typed `{rs, toml}` behaves like `{rs,toml}`.
pub(crate) fn expand_braces(glob: &str) -> Vec<String> {
    let mut expanded = Vec::new();
    if expand_braces_into(glob, &mut expanded) && !expanded.is_empty() {
        expanded
    } else {
        vec![glob.to_string()]
    }
}

/// Compile a glob pattern (with optional brace expansion) into a matcher.
///
/// Returns a closure that tests whether a file path matches the glob.
pub(crate) fn compile_glob_matcher(
    glob_pattern: &str,
) -> Result<Box<dyn Fn(&str) -> bool + Send + Sync>, SearchDbError> {
    let patterns = expand_braces(glob_pattern);
    let compiled: Vec<glob::Pattern> = patterns
        .iter()
        .map(|p| glob::Pattern::new(p))
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| SearchDbError::InvalidPattern(format!("Invalid glob pattern: {}", e)))?;

    // `require_literal_separator: false` is deliberate and load-bearing: it lets a
    // leading `**/` (added by `normalize_path_glob`) absorb the absolute-path
    // prefix (`/home/u/repo/...`) so a project-relative glob still matches the
    // absolute `file_path` stored in the index. A side effect is that a lone `*`
    // also crosses `/` — so `src/*.rs` matches `src/deep/nested/x.rs`, not just
    // `src/x.rs`. This is intentionally lenient (over-match, never under-match);
    // callers that need a single directory level should use an explicit path.
    let opts = glob::MatchOptions {
        case_sensitive: true,
        require_literal_separator: false,
        require_literal_leading_dot: false,
    };

    Ok(Box::new(move |path: &str| {
        compiled.iter().any(|p| p.matches_with(path, opts))
    }))
}

/// Anchor a project-relative glob against the ABSOLUTE `file_path` stored in
/// the index.
///
/// Globs are matched against the full absolute path
/// (e.g. `/home/u/repo/doc-backend/domain/Foo.java`). A pattern whose first
/// segment is a literal — like `doc-backend/domain/**/*.java` — can therefore
/// never match (the path starts with `/home/...`, not `doc-backend`), so grep
/// silently returned zero. Prefix such "relative" patterns with `**/` so they
/// match at any directory boundary. Patterns that are already absolute (`/…`)
/// or already floating (`**…`) are returned unchanged.
///
/// This also fixes the false-empty SQL pre-filter: a relative glob's literal
/// prefix (`doc-backend/domain/`) used to become a `file_path LIKE 'doc-backend/domain/%'`
/// condition that matched nothing against absolute paths. After prefixing with
/// `**/`, `extract_glob_prefix` returns `None`, so no bogus SQL prefix is applied.
///
/// A wildcard-free literal (`integrator-events`, `src/tools`, `tool-builders/`)
/// is a PATH the caller wants to scope to — a directory far more often than an
/// exact file. Anchored as `**/<lit>` it can only match a path ENDING in `<lit>`
/// (a file of that name), so a bare directory name silently matched nothing —
/// the exact field-reported `pathGlob` friction. Such a literal is expanded to
/// match the exact path OR its whole subtree: a trailing slash is unambiguously
/// a directory (`<lit>/**`); otherwise a brace form (`**/<lit>{,/**}`) that
/// [`compile_glob_matcher`] ORs and [`extract_glob_prefix`] still collapses to
/// no SQL prefix. Patterns that already carry a wildcard keep their exact meaning.
pub(crate) fn normalize_path_glob(glob: &str) -> String {
    // Absolute or already-floating patterns: the caller was explicit — leave as-is.
    if glob.starts_with('/') || glob.starts_with("**") {
        return glob.to_string();
    }
    // A relative pattern that carries a wildcard is anchored to any directory
    // boundary with a leading `**/` (unchanged behavior).
    let has_wildcard =
        glob.contains('*') || glob.contains('?') || glob.contains('[') || glob.contains('{');
    if has_wildcard {
        return format!("**/{glob}");
    }
    // Wildcard-free literal → match the exact path OR its subtree at any depth.
    match glob.strip_suffix('/') {
        Some(dir) => format!("**/{dir}/**"),
        None => format!("**/{glob}{{,/**}}"),
    }
}

/// Resolve the effective path prefix for SQL pre-filtering.
///
/// If `path_glob` is set, normalizes it (see [`normalize_path_glob`]) and
/// extracts a prefix from the result. Otherwise uses `path_prefix`. The glob
/// matcher (if any) is applied in Rust after SQL results are fetched, using the
/// SAME normalized pattern returned here.
pub(crate) fn resolve_path_filter(options: &SearchOptions) -> (Option<String>, SearchOptions) {
    if let Some(ref glob) = options.path_glob {
        let normalized = normalize_path_glob(glob);
        let prefix = extract_glob_prefix(&normalized);
        let mut effective = options.clone();
        // Replace path_prefix with the extracted glob prefix for SQL pre-filtering
        effective.path_prefix = prefix;
        // Clear path_glob in effective options so query builder uses path_prefix
        effective.path_glob = None;
        (Some(normalized), effective)
    } else {
        (None, options.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── Pattern escaping tests ──

    #[test]
    fn test_escape_fts5_pattern_basic() {
        assert_eq!(
            escape_fts5_pattern("println"),
            Some("\"println\"".to_string())
        );
    }

    #[test]
    fn test_escape_fts5_pattern_with_quotes() {
        assert_eq!(
            escape_fts5_pattern("say \"hello\""),
            Some("\"say \"\"hello\"\"\"".to_string())
        );
    }

    #[test]
    fn test_escape_fts5_pattern_short() {
        assert_eq!(escape_fts5_pattern("fn"), None);
        assert_eq!(escape_fts5_pattern("a"), None);
        assert_eq!(escape_fts5_pattern(""), None);
    }

    #[test]
    fn test_escape_fts5_pattern_exactly_3() {
        assert_eq!(escape_fts5_pattern("abc"), Some("\"abc\"".to_string()));
    }

    #[test]
    fn test_escape_like_pattern() {
        assert_eq!(escape_like_pattern("hello"), "hello");
        assert_eq!(escape_like_pattern("100%"), "100\\%");
        assert_eq!(escape_like_pattern("under_score"), "under\\_score");
        assert_eq!(escape_like_pattern("back\\slash"), "back\\\\slash");
    }

    // ── Glob utility tests (Task 55) ──

    #[test]
    fn test_extract_glob_prefix_with_prefix() {
        assert_eq!(extract_glob_prefix("src/**/*.rs"), Some("src/".to_string()));
        assert_eq!(
            extract_glob_prefix("src/rust/*.rs"),
            Some("src/rust/".to_string())
        );
    }

    #[test]
    fn test_extract_glob_prefix_no_prefix() {
        assert_eq!(extract_glob_prefix("**/*.rs"), None);
        assert_eq!(extract_glob_prefix("*.rs"), None);
        assert_eq!(extract_glob_prefix("?abc"), None);
    }

    #[test]
    fn test_extract_glob_prefix_no_metacharacters() {
        assert_eq!(
            extract_glob_prefix("src/main.rs"),
            Some("src/main.rs".to_string())
        );
    }

    #[test]
    fn test_normalize_path_glob_relative_gets_anchored() {
        // Relative patterns WITH a wildcard get a plain `**/` prefix.
        assert_eq!(
            normalize_path_glob("doc-backend/domain/**/*.java"),
            "**/doc-backend/domain/**/*.java"
        );
        assert_eq!(normalize_path_glob("dir/**"), "**/dir/**");
    }

    #[test]
    fn test_normalize_path_glob_directory_literal_matches_subtree() {
        // A wildcard-free literal expands to match the exact path OR its subtree,
        // so a bare directory name is no longer end-anchored to nothing.
        assert_eq!(
            normalize_path_glob("integrator-events"),
            "**/integrator-events{,/**}"
        );
        assert_eq!(normalize_path_glob("src/main.rs"), "**/src/main.rs{,/**}");
        assert_eq!(normalize_path_glob("src/tools"), "**/src/tools{,/**}");
        // A trailing slash is unambiguously a directory → subtree only.
        assert_eq!(normalize_path_glob("tool-builders/"), "**/tool-builders/**");
    }

    #[test]
    fn test_directory_literal_glob_matches_files_under_it() {
        // Regression for the field-reported friction: a bare directory `pathGlob`
        // must match the files UNDER it (absolute paths), not just a same-named file.
        let under = "/home/u/repo/src/typescript/tool-builders/search.ts";
        let exact_file = "/home/u/repo/scripts/tool-builders";
        let unrelated = "/home/u/repo/src/typescript/other/search.ts";

        // Un-normalized bare literal used to match nothing under the directory.
        let raw = compile_glob_matcher("tool-builders").unwrap();
        assert!(
            !raw(under),
            "un-normalized bare literal should not match subtree"
        );

        let bare = compile_glob_matcher(&normalize_path_glob("tool-builders")).unwrap();
        assert!(
            bare(under),
            "normalized literal matches files under the directory"
        );
        assert!(
            bare(exact_file),
            "normalized literal still matches an exact file of that name"
        );
        assert!(
            !bare(unrelated),
            "and does not over-match a sibling directory"
        );

        // Trailing slash: subtree only (a file literally named `tool-builders` is a dir miss).
        let dir = compile_glob_matcher(&normalize_path_glob("tool-builders/")).unwrap();
        assert!(dir(under), "trailing-slash directory matches its subtree");
        assert!(
            !dir(exact_file),
            "trailing-slash form does not match a same-named file"
        );
    }

    #[test]
    fn test_directory_literal_glob_has_no_sql_prefix() {
        // The brace/subtree forms must not yield a bogus absolute SQL prefix
        // (they start with `**`, so `extract_glob_prefix` returns None).
        assert_eq!(
            extract_glob_prefix(&normalize_path_glob("integrator-events")),
            None
        );
        assert_eq!(
            extract_glob_prefix(&normalize_path_glob("tool-builders/")),
            None
        );
    }

    #[test]
    fn test_normalize_path_glob_already_floating_or_absolute() {
        // Already-floating (`**…`) and absolute (`/…`) patterns are unchanged.
        assert_eq!(normalize_path_glob("**/*.rs"), "**/*.rs");
        assert_eq!(
            normalize_path_glob("**/domain/**/*.java"),
            "**/domain/**/*.java"
        );
        assert_eq!(
            normalize_path_glob("/home/u/repo/src/*.rs"),
            "/home/u/repo/src/*.rs"
        );
    }

    #[test]
    fn test_normalized_relative_glob_matches_absolute_path() {
        // Regression: a project-relative glob must match the ABSOLUTE file_path
        // stored in the index. Before normalization it silently matched nothing.
        let abs = "/home/u/repos/example-monorepo/doc-backend/domain/Order.java";

        let raw = compile_glob_matcher("doc-backend/domain/**/*.java").unwrap();
        assert!(
            !raw(abs),
            "un-anchored relative glob should NOT match abs path"
        );

        let normalized =
            compile_glob_matcher(&normalize_path_glob("doc-backend/domain/**/*.java")).unwrap();
        assert!(
            normalized(abs),
            "normalized relative glob must match abs path"
        );
        // And it must not over-match a different directory.
        assert!(!normalized(
            "/home/u/repos/example-monorepo/doc-frontend/app/Order.java"
        ));
    }

    #[test]
    fn test_normalized_relative_glob_has_no_sql_prefix() {
        // The anchored glob must not yield a bogus absolute SQL prefix
        // (which caused the false-empty pre-filter).
        assert_eq!(
            extract_glob_prefix(&normalize_path_glob("doc-backend/domain/**/*.java")),
            None
        );
    }

    #[test]
    fn test_expand_braces_basic() {
        let expanded = expand_braces("*.{rs,toml}");
        assert_eq!(expanded, vec!["*.rs", "*.toml"]);
    }

    #[test]
    fn test_expand_braces_three_alternatives() {
        let expanded = expand_braces("src/**/*.{rs,ts,js}");
        assert_eq!(expanded, vec!["src/**/*.rs", "src/**/*.ts", "src/**/*.js"]);
    }

    #[test]
    fn test_expand_braces_no_braces() {
        let expanded = expand_braces("**/*.rs");
        assert_eq!(expanded, vec!["**/*.rs"]);
    }

    /// Only the FIRST group used to expand, so a second group survived as a
    /// literal and matched nothing.
    #[test]
    fn test_expand_braces_multiple_groups() {
        let expanded = expand_braces("{src,tests}/*.{rs,ts}");
        assert_eq!(
            expanded,
            vec!["src/*.rs", "src/*.ts", "tests/*.rs", "tests/*.ts"]
        );
    }

    /// The closing brace used to be the first `}` found, which cuts a nested
    /// group in the wrong place.
    #[test]
    fn test_expand_braces_nested() {
        assert_eq!(expand_braces("{a,{b,c}}"), vec!["a", "b", "c"]);
    }

    /// The shape `normalize_path_glob` itself emits for a wildcard-free
    /// literal: exact path OR whole subtree.
    #[test]
    fn test_expand_braces_empty_alternative() {
        assert_eq!(
            expand_braces("**/src/tools{,/**}"),
            vec!["**/src/tools", "**/src/tools/**"]
        );
    }

    /// An unbalanced brace is a literal — never an accidental match-all.
    #[test]
    fn test_expand_braces_unbalanced_is_literal() {
        assert_eq!(expand_braces("*.{rs"), vec!["*.{rs"]);
    }

    /// Past the ceiling the pattern is returned verbatim: the braces then match
    /// literally (a miss), rather than compiling thousands of patterns.
    #[test]
    fn test_expand_braces_ceiling_falls_back_to_verbatim() {
        let pathological = "{a,b}".repeat(9); // 2^9 = 512 > MAX_BRACE_EXPANSIONS
        assert_eq!(expand_braces(&pathological), vec![pathological]);
    }

    /// Parity with the TypeScript `expandBraces`: a human-typed space after the
    /// comma must not become part of the pattern.
    #[test]
    fn test_expand_braces_trims_alternatives() {
        assert_eq!(expand_braces("*.{rs, ts}"), vec!["*.rs", "*.ts"]);
    }

    #[test]
    fn test_compile_glob_matcher_star_star() {
        let matcher = compile_glob_matcher("**/*.rs").unwrap();
        assert!(matcher("src/main.rs"));
        assert!(matcher("src/deep/nested/lib.rs"));
        assert!(!matcher("src/main.ts"));
        assert!(matcher("lib.rs"));
    }

    #[test]
    fn test_compile_glob_matcher_with_prefix() {
        let matcher = compile_glob_matcher("src/**/*.rs").unwrap();
        assert!(matcher("src/main.rs"));
        assert!(matcher("src/deep/lib.rs"));
        assert!(!matcher("tests/test.rs"));
    }

    #[test]
    fn test_compile_glob_matcher_braces() {
        let matcher = compile_glob_matcher("**/*.{rs,toml}").unwrap();
        assert!(matcher("src/main.rs"));
        assert!(matcher("Cargo.toml"));
        assert!(!matcher("src/main.ts"));
    }

    #[test]
    fn test_compile_glob_matcher_single_star_crosses_separators() {
        // Documented lenient behavior: with require_literal_separator=false a lone
        // `*` also crosses `/`, so `src/*.rs` matches deeply nested paths too. This
        // pins the over-match so it isn't silently changed (it is load-bearing for
        // the `**/`-anchored absolute-path matching in normalize_path_glob).
        let matcher = compile_glob_matcher("src/*.rs").unwrap();
        assert!(matcher("src/main.rs"), "direct child matches");
        assert!(
            matcher("src/deep/nested/x.rs"),
            "single `*` crosses `/` (intentional over-match)"
        );
        assert!(
            !matcher("tests/x.rs"),
            "different top-level dir does not match"
        );
        assert!(!matcher("src/main.ts"), "extension still constrains");
    }

    #[test]
    fn test_compile_glob_matcher_invalid() {
        let result = compile_glob_matcher("[invalid");
        assert!(result.is_err());
    }
}
