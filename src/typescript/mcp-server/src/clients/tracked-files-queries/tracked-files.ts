/**
 * Query operations for the tracked_files table.
 *
 * Reads from the daemon-owned tracked_files table to provide
 * file listing data for the list MCP tool.
 */

import type { Database as DatabaseType } from 'better-sqlite3';
import { existsSync } from 'node:fs';
import type { DegradedQueryResult } from '../sqlite-state-manager.js';
import { handleTableNotFound } from './helpers.js';
import { getSearchDatabasePath } from '../../utils/paths.js';

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
  /** Optional override for the sibling FTS5/file_metadata database. */
  searchDbPath?: string;
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
    options.fallbackBranch && options.fallbackBranch !== branch
      ? options.fallbackBranch
      : undefined;
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
      '(EXISTS (SELECT 1 FROM json_each(branches) WHERE value = ?) OR (EXISTS (SELECT 1 FROM json_each(branches) WHERE value = ?) AND relative_path NOT IN ' +
        '(SELECT relative_path FROM tracked_files WHERE watch_folder_id = ? AND EXISTS (SELECT 1 FROM json_each(branches) WHERE value = ?))))'
    );
    params.push(branch, fallbackBranch, options.watchFolderId, branch);
  } else if (branch) {
    conditions.push('EXISTS (SELECT 1 FROM json_each(branches) WHERE value = ?)');
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

    const mergedRows = mergeTrackedRowsWithSearchMetadata(db, rows, options, limit);
    return { data: mergedRows.map(mapTrackedFileRow), status: 'ok' };
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
    return row.cnt + countSearchMetadataFallbackRows(db, options);
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
        `SELECT je.value AS branch FROM tracked_files tf, json_each(tf.branches) je
         WHERE tf.watch_folder_id = ? AND je.value IS NOT NULL AND je.value != ?
         GROUP BY je.value
         ORDER BY COUNT(*) DESC
         LIMIT 1`
      )
      .get(watchFolderId, excludeBranch) as { branch: string } | undefined;
    return row?.branch ?? null;
  } catch {
    return null;
  }
}

function mergeTrackedRowsWithSearchMetadata(
  db: DatabaseType,
  trackedRows: Array<{
    relative_path: string;
    file_type: string | null;
    language: string | null;
    extension: string | null;
    is_test: number;
  }>,
  options: ListTrackedFilesOptions,
  limit: number
): Array<{
  relative_path: string;
  file_type: string | null;
  language: string | null;
  extension: string | null;
  is_test: number;
}> {
  const fallbackRows = listSearchMetadataFallbackRows(db, options, limit);
  if (fallbackRows.length === 0) return trackedRows;

  const byPath = new Map<string, (typeof trackedRows)[number]>();
  for (const row of trackedRows) byPath.set(row.relative_path, row);
  for (const row of fallbackRows) {
    if (!byPath.has(row.relative_path)) byPath.set(row.relative_path, row);
  }
  return [...byPath.values()]
    .sort((a, b) => a.relative_path.localeCompare(b.relative_path))
    .slice(0, limit);
}

function listSearchMetadataFallbackRows(
  db: DatabaseType,
  options: ListTrackedFilesOptions,
  limit: number
): Array<{
  relative_path: string;
  file_type: string | null;
  language: string | null;
  extension: string | null;
  is_test: number;
}> {
  if (options.fileType || options.language || options.includeTests === false) return [];
  if (!ensureSearchDbAttached(db, options.searchDbPath)) return [];

  const { conditions, params } = buildSearchMetadataFilterClause(options);
  params.push(limit);
  const sql = `
    WITH metadata AS (
      SELECT
        COALESCE(NULLIF(fm.relative_path, ''),
          CASE
            WHEN wf.path IS NOT NULL AND fm.file_path LIKE wf.path || '/%'
              THEN substr(fm.file_path, length(wf.path) + 2)
            ELSE fm.file_path
          END
        ) AS relative_path,
        fm.branches AS branches
      FROM searchdb.file_metadata fm
      JOIN watch_folders wf ON wf.tenant_id = fm.tenant_id AND wf.watch_id = ?
    )
    SELECT m.relative_path
    FROM metadata m
    WHERE ${conditions.join(' AND ')}
    ORDER BY m.relative_path ASC
    LIMIT ?
  `;

  try {
    const rows = db.prepare(sql).all(options.watchFolderId, ...params) as Array<{
      relative_path: string;
    }>;
    return rows.map((row) => ({
      relative_path: row.relative_path,
      file_type: null,
      language: null,
      extension: inferExtension(row.relative_path),
      is_test: 0,
    }));
  } catch {
    return [];
  }
}

function countSearchMetadataFallbackRows(
  db: DatabaseType,
  options: Omit<ListTrackedFilesOptions, 'limit'>
): number {
  if (options.fileType || options.language || options.includeTests === false) return 0;
  if (!ensureSearchDbAttached(db, options.searchDbPath)) return 0;

  const { conditions, params } = buildSearchMetadataFilterClause(options);
  const sql = `
    WITH metadata AS (
      SELECT
        COALESCE(NULLIF(fm.relative_path, ''),
          CASE
            WHEN wf.path IS NOT NULL AND fm.file_path LIKE wf.path || '/%'
              THEN substr(fm.file_path, length(wf.path) + 2)
            ELSE fm.file_path
          END
        ) AS relative_path,
        fm.branches AS branches
      FROM searchdb.file_metadata fm
      JOIN watch_folders wf ON wf.tenant_id = fm.tenant_id AND wf.watch_id = ?
    )
    SELECT COUNT(*) AS cnt
    FROM metadata m
    WHERE ${conditions.join(' AND ')}
  `;

  try {
    const row = db.prepare(sql).get(options.watchFolderId, ...params) as
      | { cnt: number }
      | undefined;
    return row?.cnt ?? 0;
  } catch {
    return 0;
  }
}

function buildSearchMetadataFilterClause(
  options: Omit<ListTrackedFilesOptions, 'limit'>
): FilterClause {
  const conditions: string[] = [
    'm.relative_path IS NOT NULL',
    "m.relative_path != ''",
    'NOT EXISTS (SELECT 1 FROM tracked_files tf WHERE tf.watch_folder_id = ? AND tf.relative_path = m.relative_path)',
  ];
  const params: (string | number)[] = [options.watchFolderId];
  const fallbackBranch =
    options.fallbackBranch && options.fallbackBranch !== options.branch
      ? options.fallbackBranch
      : undefined;

  if (options.path) {
    conditions.push('m.relative_path LIKE ?');
    params.push(`${options.path}/%`);
  }
  if (options.extension) {
    const extension = options.extension.startsWith('.')
      ? options.extension
      : `.${options.extension}`;
    conditions.push('m.relative_path LIKE ?');
    params.push(`%${extension}`);
  }
  if (options.branch && fallbackBranch) {
    conditions.push(
      '(EXISTS (SELECT 1 FROM json_each(m.branches) WHERE value = ?) OR EXISTS (SELECT 1 FROM json_each(m.branches) WHERE value = ?))'
    );
    params.push(options.branch, fallbackBranch);
  } else if (options.branch) {
    conditions.push('EXISTS (SELECT 1 FROM json_each(m.branches) WHERE value = ?)');
    params.push(options.branch);
  }
  if (options.glob) {
    conditions.push('m.relative_path GLOB ?');
    params.push(options.glob.replace(/\*\*/g, '*'));
  }
  if (options.componentBasePaths && options.componentBasePaths.length > 0) {
    const clauses = options.componentBasePaths.map(
      () => '(m.relative_path = ? OR m.relative_path LIKE ?)'
    );
    conditions.push(`(${clauses.join(' OR ')})`);
    for (const bp of options.componentBasePaths) params.push(bp, `${bp}/%`);
  }
  if (options.afterPath) {
    conditions.push('m.relative_path > ?');
    params.push(options.afterPath);
  }

  return { conditions, params };
}

function ensureSearchDbAttached(db: DatabaseType, explicitPath?: string): boolean {
  const dbPath = explicitPath ?? getSearchDatabasePath();
  if (!existsSync(dbPath)) return false;
  try {
    const attached = db.prepare('PRAGMA database_list').all() as Array<{ name: string }>;
    if (attached.some((entry) => entry.name === 'searchdb')) return true;
    db.prepare('ATTACH DATABASE ? AS searchdb').run(dbPath);
    return true;
  } catch {
    return false;
  }
}

function inferExtension(relativePath: string): string | null {
  const fileName = relativePath.split('/').pop() ?? relativePath;
  const dot = fileName.lastIndexOf('.');
  return dot > 0 ? fileName.slice(dot) : null;
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
