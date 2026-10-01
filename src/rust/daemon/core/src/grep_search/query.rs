/// Database query helpers for resolving file paths before grep scanning.
use sqlx::Row;

use crate::search_db::{SearchDbError, SearchDbManager};
use crate::text_search::{compile_glob_matcher, resolve_path_filter, SearchOptions};

use super::types::FileInfo;

/// Query file_metadata for file paths matching scope filters.
pub(super) async fn query_file_paths(
    search_db: &SearchDbManager,
    options: &SearchOptions,
    glob_matcher: Option<&Box<dyn Fn(&str) -> bool + Send + Sync>>,
) -> Result<Vec<FileInfo>, SearchDbError> {
    let mut sql = String::from(
        "SELECT file_path, tenant_id, branches AS branch FROM file_metadata WHERE 1=1",
    );
    let mut next_param = 1;

    if options.tenant_id.is_some() {
        sql.push_str(&format!(" AND tenant_id = ?{}", next_param));
        next_param += 1;
    }
    if options.branch.is_some() {
        sql.push_str(&format!(
            " AND EXISTS (SELECT 1 FROM json_each(branches) WHERE value = ?{})",
            next_param
        ));
        next_param += 1;
    }
    if options.path_prefix.is_some() {
        sql.push_str(&format!(" AND file_path LIKE ?{} ESCAPE '\\'", next_param));
    }

    sql.push_str(" ORDER BY file_path");

    let pool = search_db.pool();
    let mut query = sqlx::query(&sql);

    if let Some(ref tid) = options.tenant_id {
        query = query.bind(tid);
    }
    if let Some(ref branch) = options.branch {
        query = query.bind(branch);
    }
    if let Some(ref prefix) = options.path_prefix {
        query = query.bind(format!("{}%", prefix));
    }

    let rows = query.fetch_all(pool).await?;
    // Same exclude rule as the FTS engines: an excluded file is never scanned,
    // so none of its matches can spend the result budget.
    let exclude = crate::text_search::compile_path_exclude(options.path_exclude.as_deref())?;

    let mut files = Vec::with_capacity(rows.len());
    for row in rows {
        let file_path: String = row.get("file_path");
        // Apply glob filter
        if let Some(matcher) = glob_matcher {
            if !matcher(&file_path) {
                continue;
            }
        }
        if crate::text_search::is_excluded(exclude.as_ref(), &file_path) {
            continue;
        }
        files.push(FileInfo {
            file_path,
            tenant_id: row.get("tenant_id"),
            branch: crate::text_search::display_branch(row.get("branch")),
        });
    }

    // One row per content GENERATION of a path: without this, the scanner opens
    // the same file once per generation and reports every match in it that many
    // times. Dedupe on the path — the generations differ only in what `search.db`
    // remembers, while the scan reads the single file on disk. Same identity rule
    // the FTS engines apply to their rows (`text_search::dedup`).
    crate::text_search::retain_first_by_path(&mut files, |f| f.file_path.as_str());

    Ok(files)
}

/// Resolve path glob filter options and return a compiled matcher if needed.
pub(super) fn resolve_and_compile(
    options: &SearchOptions,
) -> Result<
    (
        SearchOptions,
        Option<Box<dyn Fn(&str) -> bool + Send + Sync>>,
    ),
    SearchDbError,
> {
    let (glob_pattern, effective_options) = resolve_path_filter(options);
    let glob_matcher = glob_pattern
        .as_deref()
        .map(compile_glob_matcher)
        .transpose()?;
    Ok((effective_options, glob_matcher))
}
