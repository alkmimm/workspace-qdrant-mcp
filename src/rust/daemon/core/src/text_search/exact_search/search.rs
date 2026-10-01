//! Main entry point for exact substring search.
//!
//! Orchestrates FTS5 trigram pre-filtering with INSTR verification,
//! scope binding, glob post-filtering, and optional context attachment.

use futures::TryStreamExt;
use sqlx::Row;
use tracing::debug;

use super::super::dedup::MatchDeduper;
use super::super::escaping::{compile_glob_matcher, escape_fts5_pattern, resolve_path_filter};
use super::super::path_exclude::{compile_path_exclude, is_excluded, PathExclude};
use super::super::types::{SearchMatch, SearchOptions, SearchResults};
use super::context::attach_context_lines;
use super::query_builder::build_search_query;
use crate::search_db::{SearchDbError, SearchDbManager};

/// Search code_lines for an exact substring pattern.
///
/// Uses a two-phase approach:
/// 1. FTS5 trigram MATCH for fast candidate selection
/// 2. INSTR verification for exact substring match
///
/// For patterns shorter than 3 characters, falls back to INSTR-only scan.
/// When `path_glob` is set, applies glob filtering in Rust after SQL results.
pub async fn search_exact(
    search_db: &SearchDbManager,
    pattern: &str,
    options: &SearchOptions,
) -> Result<SearchResults, SearchDbError> {
    let start = std::time::Instant::now();

    if pattern.is_empty() {
        return Ok(SearchResults {
            pattern: pattern.to_string(),
            matches: vec![],
            truncated: false,
            query_time_ms: 0,
            search_engine: "fts5".to_string(),
        });
    }

    let (glob_pattern, effective_options) = resolve_path_filter(options);
    let glob_matcher = glob_pattern
        .as_deref()
        .map(compile_glob_matcher)
        .transpose()?;
    let exclude = compile_path_exclude(options.path_exclude.as_deref())?;

    let fts5_pattern = escape_fts5_pattern(pattern);
    let (sql, use_fts) = build_search_query(&fts5_pattern, &effective_options);

    debug!(
        "FTS5 search: pattern={:?}, fts5={:?}, use_fts={}, tenant={:?}, branch={:?}, \
         path_prefix={:?}, path_glob={:?}",
        pattern,
        fts5_pattern,
        use_fts,
        effective_options.tenant_id,
        effective_options.branch,
        effective_options.path_prefix,
        options.path_glob,
    );

    let (matches, truncated) = run_bound_query(
        search_db,
        pattern,
        &effective_options,
        &fts5_pattern,
        use_fts,
        &sql,
        &glob_matcher,
        exclude.as_ref(),
        options,
    )
    .await?;

    let mut matches = matches;
    if options.context_lines > 0 {
        attach_context_lines(search_db, &mut matches, options.context_lines).await?;
    }

    let query_time_ms = start.elapsed().as_millis() as u64;
    debug!(
        "FTS5 search complete: {} matches in {}ms (pattern={:?}, truncated={})",
        matches.len(),
        query_time_ms,
        pattern,
        truncated
    );

    Ok(SearchResults {
        pattern: pattern.to_string(),
        matches,
        truncated,
        query_time_ms,
        search_engine: "fts5".to_string(),
    })
}

/// Bind query parameters and execute the FTS5 search, returning matches + truncation flag.
#[allow(clippy::too_many_arguments)]
async fn run_bound_query(
    search_db: &SearchDbManager,
    pattern: &str,
    effective_options: &SearchOptions,
    fts5_pattern: &Option<String>,
    use_fts: bool,
    sql: &str,
    glob_matcher: &Option<Box<dyn Fn(&str) -> bool + Send + Sync>>,
    exclude: Option<&PathExclude>,
    options: &SearchOptions,
) -> Result<(Vec<SearchMatch>, bool), SearchDbError> {
    let instr_pattern = if options.case_insensitive {
        pattern.to_lowercase()
    } else {
        pattern.to_string()
    };
    let path_prefix_arg = effective_options
        .path_prefix
        .as_ref()
        .map(|p| format!("{}%", p));

    let mut query = sqlx::query(sql);
    if use_fts {
        query = query.bind(fts5_pattern.as_ref().unwrap());
    }
    query = query.bind(&instr_pattern);
    if let Some(ref tid) = effective_options.tenant_id {
        query = query.bind(tid);
    }
    if let Some(ref branch) = effective_options.branch {
        query = query.bind(branch);
    }
    if let Some(ref prefix_arg) = path_prefix_arg {
        query = query.bind(prefix_arg);
    }
    collect_matches(
        search_db.pool(),
        query,
        glob_matcher,
        exclude,
        options.max_results,
    )
    .await
}

/// Stream SQL rows, apply the include glob and the exclude, and collect into
/// `SearchMatch` values. Both filters run before dedup and the cap, so a filtered
/// row never spends any of the `max_results` budget.
async fn collect_matches<'q>(
    pool: &sqlx::Pool<sqlx::Sqlite>,
    query: sqlx::query::Query<'q, sqlx::Sqlite, sqlx::sqlite::SqliteArguments<'q>>,
    glob_matcher: &Option<Box<dyn Fn(&str) -> bool + Send + Sync>>,
    exclude: Option<&PathExclude>,
    max_results: usize,
) -> Result<(Vec<SearchMatch>, bool), SearchDbError> {
    let max = if max_results > 0 {
        max_results
    } else {
        usize::MAX
    };
    let mut stream = query.fetch(pool);
    let mut matches = Vec::new();
    let mut truncated = false;
    // The JOIN streams one row per content GENERATION of a path, so the same
    // served hit can arrive several times. Collapse before the cap so a page
    // holds `max_results` real hits and the pre-cap count the gRPC layer reports
    // as `total_matches` counts hits rather than rows (see `text_search::dedup`).
    let mut deduper = MatchDeduper::new();

    while let Some(row) = stream.try_next().await? {
        let file_path: String = row.get("file_path");
        if let Some(ref matcher) = glob_matcher {
            if !matcher(&file_path) {
                continue;
            }
        }
        if is_excluded(exclude, &file_path) {
            continue;
        }
        let line_number: i64 = row.get("line_number");
        let content: String = row.get("content");
        if !deduper.accept(&file_path, line_number, &content) {
            continue;
        }
        matches.push(SearchMatch {
            line_id: row.get("line_id"),
            file_id: row.get("file_id"),
            line_number,
            content,
            file_path,
            tenant_id: row.get("tenant_id"),
            branch: crate::text_search::display_branch(row.get("branch")),
            context_before: vec![],
            context_after: vec![],
            file_size: row.try_get::<Option<i64>, _>("size_bytes").ok().flatten(),
        });
        if matches.len() >= max {
            truncated = true;
            break;
        }
    }
    drop(stream);
    Ok((matches, truncated))
}
