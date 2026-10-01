//! `pathExclude` matching — the same rule the MCP server applies, applied here
//! BEFORE the result cap.
//!
//! The MCP read surfaces accept a hard per-call exclude glob (`pathExclude`).
//! It used to be applied only in TypeScript, on the rows this service had
//! already returned. Because a page fetches just `offset + maxResults` rows, the
//! excluded paths consumed that budget first: measured on the live index,
//! `grep "pub fn" pathExclude:"src/rust/**" maxResults:3` returned an EMPTY page
//! with `truncated: true`, no continuation, and `total_matches: 2021` — for a
//! surface of 13. Filtering here, alongside the include glob and before the cap,
//! keeps every page full of real hits and every count exact.
//!
//! ## Why this is not [`compile_glob_matcher`](super::escaping::compile_glob_matcher)
//!
//! The include matcher is deliberately lenient: its `*` crosses `/` and `[...]`
//! is a character class. Over-matching an INCLUDE only widens a result set. An
//! EXCLUDE that over-matches deletes hits the server would have kept, and they
//! become unreachable. So this is a line-for-line port of the TypeScript
//! `matchesPathExclude` (`utils/path-glob.ts`): `*` and `?` stop at `/`, `**`
//! spans directories, brackets are literal, a wildcard-free literal scopes to
//! its subtree, and every pattern floats to any depth. The shared table in
//! `assets/path_exclude_parity.json` — generated FROM the TypeScript
//! implementation — is asserted by both test suites, so the two cannot drift.

use regex::RegexSet;

use super::escaping::expand_braces;
use crate::search_db::SearchDbError;

/// A compiled `pathExclude` glob.
pub(crate) struct PathExclude {
    patterns: RegexSet,
}

impl PathExclude {
    /// Compile an exclude glob. Brace alternation is expanded first, then each
    /// alternative is directory-shaped and floated, exactly as the MCP server does.
    pub(crate) fn compile(glob: &str) -> Result<Self, SearchDbError> {
        let mut regexes = Vec::new();
        for alternative in expand_braces(glob) {
            for shaped in directory_aware_globs(&alternative) {
                regexes.push(glob_to_regex(&shaped));
                regexes.push(glob_to_regex(&format!("**/{shaped}")));
            }
        }
        let patterns = RegexSet::new(&regexes).map_err(|e| {
            SearchDbError::InvalidPattern(format!("Invalid pathExclude glob {glob:?}: {e}"))
        })?;
        Ok(Self { patterns })
    }

    /// Whether `path` falls under the excluded glob.
    pub(crate) fn excludes(&self, path: &str) -> bool {
        self.patterns.is_match(&path.replace('\\', "/"))
    }
}

/// Compile the request's exclude glob, if it carries one.
pub(crate) fn compile_path_exclude(
    path_exclude: Option<&str>,
) -> Result<Option<PathExclude>, SearchDbError> {
    match path_exclude {
        Some(glob) if !glob.trim().is_empty() => PathExclude::compile(glob).map(Some),
        _ => Ok(None),
    }
}

/// Whether an optional exclude rejects `path` — the one-line check every engine
/// runs on a row before it counts toward the cap.
pub(crate) fn is_excluded(exclude: Option<&PathExclude>, path: &str) -> bool {
    exclude.is_some_and(|e| e.excludes(path))
}

/// A wildcard-free literal names a path the caller wants gone — usually a
/// directory — so it covers the exact path and its subtree; a trailing slash is
/// unambiguously a directory. Port of `directoryAwareGlobs`.
fn directory_aware_globs(glob: &str) -> Vec<String> {
    if glob.contains(['*', '?', '[', '{']) {
        return vec![glob.to_string()];
    }
    let dir = glob.trim_end_matches('/');
    if dir.is_empty() {
        return vec![glob.to_string()];
    }
    if glob.ends_with('/') {
        vec![format!("{dir}/**")]
    } else {
        vec![glob.to_string(), format!("{glob}/**")]
    }
}

/// Port of `globToRegExp`: anchored at both ends; `**/` spans zero or more
/// directories, `**` anything, `*` and `?` never a `/`; every other character —
/// brackets and braces included — is literal.
fn glob_to_regex(glob: &str) -> String {
    let chars: Vec<char> = glob.replace('\\', "/").chars().collect();
    let mut out = String::from("^");
    let mut i = 0;
    while i < chars.len() {
        match chars[i] {
            '*' if chars.get(i + 1) == Some(&'*') => {
                if chars.get(i + 2) == Some(&'/') {
                    out.push_str("(?:.*/)?");
                    i += 3;
                } else {
                    out.push_str(".*");
                    i += 2;
                }
            }
            '*' => {
                out.push_str("[^/]*");
                i += 1;
            }
            '?' => {
                out.push_str("[^/]");
                i += 1;
            }
            c => {
                out.push_str(&regex::escape(&c.to_string()));
                i += 1;
            }
        }
    }
    out.push('$');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The shared table, generated from the TypeScript implementation.
    const PARITY: &str = include_str!("../../../../../../assets/path_exclude_parity.json");

    #[derive(serde::Deserialize)]
    struct Table {
        cases: Vec<Case>,
    }

    #[derive(serde::Deserialize)]
    struct Case {
        path: String,
        exclude: String,
        excluded: bool,
        why: String,
    }

    #[test]
    fn matches_the_typescript_reference_on_every_shared_case() {
        let table: Table = serde_json::from_str(PARITY).expect("parity table parses");
        assert!(
            table.cases.len() >= 30,
            "parity table unexpectedly small ({}) — was it truncated?",
            table.cases.len()
        );
        let mut disagreements = Vec::new();
        for case in &table.cases {
            let got = PathExclude::compile(&case.exclude)
                .expect("every table glob compiles")
                .excludes(&case.path);
            if got != case.excluded {
                disagreements.push(format!(
                    "{:?} vs {:?}: daemon={got} server={} ({})",
                    case.exclude, case.path, case.excluded, case.why
                ));
            }
        }
        assert!(
            disagreements.is_empty(),
            "daemon pathExclude disagrees with the MCP server:\n{}",
            disagreements.join("\n")
        );
    }

    #[test]
    fn single_star_never_crosses_a_directory() {
        // The include matcher would match here; an exclude must not.
        let e = PathExclude::compile("src/*.rs").unwrap();
        assert!(e.excludes("/repo/src/x.rs"));
        assert!(!e.excludes("/repo/src/deep/x.rs"));
    }

    #[test]
    fn brackets_are_literal() {
        let e = PathExclude::compile("src/[x].rs").unwrap();
        assert!(e.excludes("/repo/src/[x].rs"));
        assert!(!e.excludes("/repo/src/x.rs"));
    }

    #[test]
    fn empty_or_blank_exclude_is_no_exclude() {
        assert!(compile_path_exclude(None).unwrap().is_none());
        assert!(compile_path_exclude(Some("")).unwrap().is_none());
        assert!(compile_path_exclude(Some("   ")).unwrap().is_none());
        assert!(!is_excluded(None, "/repo/anything.rs"));
    }

    #[test]
    fn is_excluded_delegates_to_the_compiled_glob() {
        let e = compile_path_exclude(Some("old_project/**")).unwrap();
        assert!(is_excluded(e.as_ref(), "/repo/pkg/old_project/a.ts"));
        assert!(!is_excluded(e.as_ref(), "/repo/src/a.ts"));
    }
}
