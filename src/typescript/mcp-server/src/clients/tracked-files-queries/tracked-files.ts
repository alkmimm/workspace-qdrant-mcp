/**
 * Query operations for the tracked_files table.
 *
 * Reads from the daemon-owned tracked_files table to provide
 * file listing data for the list MCP tool.
 */

import type { Database as DatabaseType } from 'better-sqlite3';
import type { DegradedQueryResult } from '../sqlite-state-manager.js';
import { handleTableNotFound } from './helpers.js';

// ── Types ────────────────────────────────────────────────────────────────

export interface TrackedFileEntry {
  relativePath: string;
  fileType: string | null;
  language: string | null;
  extension: string | null;
  isTest: boolean;
}

export interface ListTrackedFilesOptions {
  watchFolderId: string;
  path?: string;
  fileType?: string;
  language?: string;
  extension?: string;
  includeTests?: boolean;
  branch?: string;
  /**
   * Base/default branch to fall back to for files unchanged on `branch`.
   * When set (and different from `branch`), the query returns rows on `branch`
   * PLUS rows on `fallbackBranch` whose `relative_path` is not already present
   * on `branch` — i.e. the project as it appears on the feature branch, without
   * surfacing the stale default-branch copy of a file changed on `branch`.
   */
  fallbackBranch?: string;
  limit?: number;
  /** Glob pattern (e.g. "*.rs") — translated to SQLite GLOB */
  glob?: string;
  /** Component base-path prefixes (OR logic) — each entry is a basePath like "src/rust/daemon" */
  componentBasePaths?: string[];
  /** Keyset pagination cursor: return rows with relative_path > cursor */
  afterPath?: string;
}

// ── Query Building ───────────────────────────────────────────────────────

interface FilterClause {
  conditions: string[];
  params: (string | number)[];
}

/** Build WHERE conditions and params from filter options. */
function buildFilterClause(options: Omit<ListTrackedFilesOptions, 'limit'>): FilterClause {
  const conditions: string[] = ['watch_folder_id = ?'];
  const params: (string | number)[] = [options.watchFolderId];
  const { path, fileType, language, extension, branch, glob, componentBasePaths, afterPath } =
    options;
  const fallbackBranch =
    options.fallbackBranch && options.fallbackBranch !== branch ? options.fallbackBranch : undefined;
  const includeTests = options.includeTests ?? true;

  if (path) {
    conditions.push('relative_path LIKE ?');
    params.push(`${path}/%`);
  }
  if (fileType) {
    conditions.push('file_type = ?');
    params.push(fileType);
  }
  if (language) {
    conditions.push('language = ?');
    params.push(language);
  }
  if (extension) {
    conditions.push('extension = ?');
    params.push(extension);
  }
  if (!includeTests) {
    conditions.push('is_test = 0');
  }
  if (branch && fallbackBranch) {
    // Feature-branch view: rows on `branch`, plus rows on the default branch
    // whose path is NOT overridden by a same-path entry on `branch`.
    conditions.push(
      '(branch = ? OR (branch = ? AND relative_path NOT IN ' +
        '(SELECT relative_path FROM tracked_files WHERE watch_folder_id = ? AND branch = ?)))'
    );
    params.push(branch, fallbackBranch, options.watchFolderId, branch);
  } else if (branch) {
    conditions.push('branch = ?');
    params.push(branch);
  }
  if (glob) {
    // SQLite GLOB uses * for multi-char and ? for single-char, same as shell globs.
    // The caller passes a pattern like "*.rs" or "src/**/*.ts"; translate ** → * for SQLite.
    const sqliteGlob = glob.replace(/\*\*/g, '*');
    conditions.push('relative_path GLOB ?');
    params.push(sqliteGlob);
  }
  if (componentBasePaths && componentBasePaths.length > 0) {
    // Build OR clause: each base path matches exact or prefix (with /)
    const clauses = componentBasePaths.map(() => '(relative_path = ? OR relative_path LIKE ?)');
    conditions.push(`(${clauses.join(' OR ')})`);
    for (const bp of componentBasePaths) {
      params.push(bp, `${bp}/%`);
    }
  }
  if (afterPath) {
    conditions.push('relative_path > ?');
    params.push(afterPath);
  }

  return { conditions, params };
}

// ── Queries ──────────────────────────────────────────────────────────────

/**
 * List tracked files for a project, with optional filtering.
 *
 * Returns minimal fields needed for tree construction.
 */
export function listTrackedFiles(
  db: DatabaseType | null,
  options: ListTrackedFilesOptions
): DegradedQueryResult<TrackedFileEntry[]> {
  if (!db) {
    return {
      data: [],
      status: 'degraded',
      reason: 'database_not_found',
      message: 'Database not initialized',
    };
  }

  try {
    const { conditions, params } = buildFilterClause(options);
    const limit = options.limit ?? 500;
    params.push(limit);

    const sql = `
      SELECT relative_path, file_type, language, extension, is_test
      FROM tracked_files
      WHERE ${conditions.join(' AND ')}
      ORDER BY relative_path ASC
      LIMIT ?
    `;

    const rows = db.prepare(sql).all(...params) as Array<{
      relative_path: string;
      file_type: string | null;
      language: string | null;
      extension: string | null;
      is_test: number;
    }>;

    return { data: rows.map(mapTrackedFileRow), status: 'ok' };
  } catch (error) {
    return handleTableNotFound(error, [], 'tracked_files');
  }
}

/**
 * Count total tracked files matching the same filters (ignoring limit).
 *
 * Used to report accurate totals when results are truncated.
 */
export function countTrackedFiles(
  db: DatabaseType | null,
  options: Omit<ListTrackedFilesOptions, 'limit'>
): number {
  if (!db) return 0;

  try {
    const { conditions, params } = buildFilterClause(options);
    const sql = `
      SELECT COUNT(*) as cnt
      FROM tracked_files
      WHERE ${conditions.join(' AND ')}
    `;
    const row = db.prepare(sql).get(...params) as { cnt: number };
    return row.cnt;
  } catch {
    return 0;
  }
}

/**
 * Resolve the de-facto base branch for a project: the branch under which the
 * most files are tracked, excluding `excludeBranch`. This matches whatever
 * branch the daemon tagged the bulk of (unchanged) files under — the daemon
 * defaults unchanged files to the project's base branch regardless of the
 * repo's local git naming (e.g. files end up under "main" even when the git
 * default is "master") — so it is the correct fallback target for a
 * feature-branch view. Returns `null` when no other branch has tracked files.
 */
export function getBaseBranch(
  db: DatabaseType | null,
  watchFolderId: string,
  excludeBranch: string
): string | null {
  if (!db) return null;
  try {
    const row = db
      .prepare(
        `SELECT branch FROM tracked_files
         WHERE watch_folder_id = ? AND branch IS NOT NULL AND branch != ?
         GROUP BY branch
         ORDER BY COUNT(*) DESC
         LIMIT 1`
      )
      .get(watchFolderId, excludeBranch) as { branch: string } | undefined;
    return row?.branch ?? null;
  } catch {
    return null;
  }
}

// ── Helpers ──────────────────────────────────────────────────────────────

function mapTrackedFileRow(row: {
  relative_path: string;
  file_type: string | null;
  language: string | null;
  extension: string | null;
  is_test: number;
}): TrackedFileEntry {
  return {
    relativePath: row.relative_path,
    fileType: row.file_type,
    language: row.language,
    extension: row.extension,
    isTest: row.is_test === 1,
  };
}
