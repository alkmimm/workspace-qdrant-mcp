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
  /** Glob to EXCLUDE (e.g. "old_project/**") — floated NOT GLOB, opposite of `glob`. */
  excludeGlob?: string;
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

const LANGUAGE_BY_EXTENSION: Record<string, string> = {
  ts: 'typescript',
  tsx: 'typescript',
  'd.ts': 'typescript',
  'd.mts': 'typescript',
  'd.cts': 'typescript',
  js: 'javascript',
  jsx: 'javascript',
  mjs: 'javascript',
  cjs: 'javascript',
  rs: 'rust',
  py: 'python',
  java: 'java',
  go: 'go',
  rb: 'ruby',
  php: 'php',
  cs: 'csharp',
  c: 'c',
  h: 'c',
  cpp: 'cpp',
  cc: 'cpp',
  cxx: 'cpp',
  hpp: 'cpp',
  kt: 'kotlin',
  kts: 'kotlin',
  swift: 'swift',
  scala: 'scala',
  sh: 'shell',
  bash: 'shell',
  zsh: 'shell',
  ps1: 'powershell',
  lua: 'lua',
  dart: 'dart',
  zig: 'zig',
  d: 'd',
  proto: 'protobuf',
  graphql: 'graphql',
  gql: 'graphql',
  html: 'html',
  htm: 'html',
  css: 'css',
  scss: 'scss',
  less: 'less',
};

const FILE_TYPE_EXTENSIONS: Record<string, string[]> = {
  code: [
    'ts',
    'tsx',
    'd.ts',
    'd.mts',
    'd.cts',
    'js',
    'jsx',
    'mjs',
    'cjs',
    'rs',
    'py',
    'java',
    'go',
    'rb',
    'php',
    'cs',
    'c',
    'h',
    'cpp',
    'cc',
    'cxx',
    'hpp',
    'kt',
    'kts',
    'swift',
    'scala',
    'sh',
    'bash',
    'zsh',
    'ps1',
    'lua',
    'dart',
    'zig',
    'd',
    'proto',
    'graphql',
    'gql',
    'vue',
    'svelte',
    'astro',
  ],
  text: ['txt', 'md', 'rst', 'org', 'adoc', 'tex'],
  docs: ['pdf', 'epub', 'docx', 'doc', 'odt', 'rtf', 'pages', 'mobi'],
  web: ['html', 'htm', 'xhtml', 'css', 'scss', 'less', 'xml'],
  slides: ['ppt', 'pptx', 'key', 'odp'],
  config: ['yaml', 'yml', 'toml', 'ini', 'env'],
  data: ['json', 'csv', 'tsv', 'parquet', 'xlsx', 'xls', 'sqlite', 'db', 'npy', 'ipynb'],
  build: [
    'zip',
    'so',
    'dll',
    'dylib',
    'whl',
    'jar',
    'war',
    'ear',
    'tar',
    'gz',
    'bz2',
    'xz',
    'lock',
  ],
};

/**
 * Append a floated `NOT GLOB` exclusion for `excludeGlob` on `column`, mirroring
 * the include-glob float below: it drops the pattern at the repo root AND at any
 * nested depth (negation of `(GLOB ? OR GLOB * /?)`). SQLite GLOB has no `**` —
 * collapse it to `*`, which already crosses `/`.
 */
function pushExcludeGlobClause(
  conditions: string[],
  params: (string | number)[],
  column: string,
  excludeGlob: string
): void {
  const collapsed = excludeGlob.replace(/\*\*/g, '*');
  if (collapsed.startsWith('*') || collapsed.startsWith('/')) {
    conditions.push(`${column} NOT GLOB ?`);
    params.push(collapsed);
  } else {
    conditions.push(`(${column} NOT GLOB ? AND ${column} NOT GLOB ?)`);
    params.push(collapsed, `*/${collapsed}`);
  }
}

/** Build WHERE conditions and params from filter options. */
function buildFilterClause(options: Omit<ListTrackedFilesOptions, 'limit'>): FilterClause {
  const conditions: string[] = ['watch_folder_id = ?'];
  const params: (string | number)[] = [options.watchFolderId];
  const {
    path,
    fileType,
    language,
    extension,
    branch,
    glob,
    excludeGlob,
    componentBasePaths,
    afterPath,
  } = options;
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
    addNullableMetadataCondition(
      conditions,
      params,
      'file_type',
      fileType,
      extensionsForFileType(fileType)
    );
  }
  if (language) {
    addNullableMetadataCondition(
      conditions,
      params,
      'language',
      language,
      extensionsForLanguage(language)
    );
  }
  if (extension) {
    addNullableMetadataCondition(conditions, params, 'extension', normalizeExtension(extension), [
      extension,
    ]);
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
    // SQLite GLOB matches the ENTIRE relative_path (anchored at both ends) and its
    // `*` crosses `/`. `**` is collapsed to `*` since SQLite GLOB has no `**`.
    //
    // A *relative* pattern ("V*.sql", "src/main.rs", "db/migration/V*.sql") is thus
    // anchored at the repo root and silently matches nothing when the file is nested
    // — the same false-empty trap the daemon's grep path already fixed via
    // `normalize_path_glob` (see src/rust/.../text_search/escaping.rs). Mirror that
    // here: float a relative pattern so it matches at the repo root AND at any nested
    // depth. Patterns that are already floating (leading `*`) or absolute (leading
    // `/`) are matched verbatim.
    const collapsed = glob.replace(/\*\*/g, '*');
    if (collapsed.startsWith('*') || collapsed.startsWith('/')) {
      conditions.push('relative_path GLOB ?');
      params.push(collapsed);
    } else {
      conditions.push('(relative_path GLOB ? OR relative_path GLOB ?)');
      params.push(collapsed, `*/${collapsed}`);
    }
  }
  if (excludeGlob) {
    pushExcludeGlobClause(conditions, params, 'relative_path', excludeGlob);
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
 * Map absolute file paths to the daemon's `is_test` classification for one
 * watch folder. Best-effort annotation source for the FTS-backed read
 * surfaces (grep matches, exact-search hits), whose rows carry no ingest
 * tags — reading the verdict back from `tracked_files.is_test` keeps the
 * daemon's `is_test_file()` classifier the single source of truth instead of
 * re-deriving it from client-side path heuristics that could drift.
 *
 * `tracked_files` stores only `relative_path` (no absolute-path column in the
 * live schema — the first cut of this query assumed one and silently returned
 * nothing in production), so the callers' absolute FTS paths are relativized
 * against the watch folder's root (`watch_folders.path`) before the lookup and
 * mapped back to absolute keys after. `MAX(is_test)` collapses the multiple
 * generations a path can have. Chunked to stay under SQLite's bound-parameter
 * limit. Paths with no row (or outside the root) are simply absent from the
 * map (absent = unknown, never false).
 */
export function getIsTestByFilePaths(
  db: DatabaseType | null,
  watchFolderId: string,
  filePaths: readonly string[]
): Map<string, boolean> {
  const out = new Map<string, boolean>();
  if (!db || filePaths.length === 0) return out;
  try {
    const wf = db
      .prepare('SELECT path FROM watch_folders WHERE watch_id = ?')
      .get(watchFolderId) as { path?: string } | undefined;
    const root = wf?.path;
    if (!root) return out;
    const prefix = root.endsWith('/') ? root : `${root}/`;
    const absByRel = new Map<string, string[]>();
    for (const abs of filePaths) {
      if (!abs.startsWith(prefix)) continue;
      const rel = abs.slice(prefix.length);
      const list = absByRel.get(rel);
      if (list) list.push(abs);
      else absByRel.set(rel, [abs]);
    }
    const relPaths = [...absByRel.keys()];
    const CHUNK = 400; // SQLite's default max bound parameters is 999
    for (let i = 0; i < relPaths.length; i += CHUNK) {
      const chunk = relPaths.slice(i, i + CHUNK);
      const placeholders = chunk.map(() => '?').join(',');
      const rows = db
        .prepare(
          `SELECT relative_path, MAX(is_test) AS is_test FROM tracked_files
           WHERE watch_folder_id = ? AND relative_path IN (${placeholders})
           GROUP BY relative_path`
        )
        .all(watchFolderId, ...chunk) as Array<{
        relative_path: string;
        is_test: number | null;
      }>;
      for (const row of rows) {
        for (const abs of absByRel.get(row.relative_path) ?? []) {
          out.set(abs, row.is_test === 1);
        }
      }
    }
  } catch {
    out.clear(); // annotation is best-effort — never fail the read
  }
  return out;
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
    return rows.map((row) => {
      const inferred = inferFallbackMetadata(row.relative_path);
      return {
        relative_path: row.relative_path,
        file_type: inferred.fileType,
        language: inferred.language,
        extension: inferred.extension,
        is_test: inferred.isTest ? 1 : 0,
      };
    });
  } catch {
    return [];
  }
}

function countSearchMetadataFallbackRows(
  db: DatabaseType,
  options: Omit<ListTrackedFilesOptions, 'limit'>
): number {
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
    addExtensionCondition(conditions, params, [normalizeExtension(options.extension)]);
  }
  if (options.fileType) {
    addExtensionCondition(conditions, params, extensionsForFileType(options.fileType));
  }
  if (options.language) {
    addExtensionCondition(conditions, params, extensionsForLanguage(options.language));
  }
  if (options.includeTests === false) {
    conditions.push(
      "LOWER(m.relative_path) NOT LIKE '%/test/%'",
      "LOWER(m.relative_path) NOT LIKE '%/tests/%'",
      "LOWER(m.relative_path) NOT LIKE 'test/%'",
      "LOWER(m.relative_path) NOT LIKE 'tests/%'",
      "LOWER(m.relative_path) NOT LIKE '%.test.%'",
      "LOWER(m.relative_path) NOT LIKE '%.spec.%'",
      "LOWER(m.relative_path) NOT LIKE '%_test.%'"
    );
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
  if (options.excludeGlob) {
    pushExcludeGlobClause(conditions, params, 'm.relative_path', options.excludeGlob);
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
  try {
    const attached = db.prepare('PRAGMA database_list').all() as Array<{ name: string }>;
    if (attached.some((entry) => entry.name === 'searchdb')) return true;
  } catch {
    return false;
  }

  const dbPath = explicitPath ?? getSearchDatabasePath();
  if (!existsSync(dbPath)) return false;
  try {
    db.prepare('ATTACH DATABASE ? AS searchdb').run(dbPath);
    return true;
  } catch {
    return false;
  }
}

function addExtensionCondition(
  conditions: string[],
  params: (string | number)[],
  extensions: string[]
): void {
  const normalized = extensions.map(normalizeExtension).filter((ext) => ext.length > 0);
  if (normalized.length === 0) {
    conditions.push('0 = 1');
    return;
  }
  conditions.push(`(${buildRelativePathExtensionPredicate('m.relative_path', normalized)})`);
  for (const ext of normalized) params.push(`%.${ext}`);
}

function addNullableMetadataCondition(
  conditions: string[],
  params: (string | number)[],
  column: 'file_type' | 'language' | 'extension',
  value: string,
  extensions: string[]
): void {
  const normalized = extensions.map(normalizeExtension).filter((ext) => ext.length > 0);
  if (normalized.length === 0) {
    conditions.push(`${column} = ?`);
    params.push(value);
    return;
  }
  conditions.push(
    `(${column} = ? OR ((${column} IS NULL OR ${column} = '') AND ${buildRelativePathExtensionPredicate('relative_path', normalized)}))`
  );
  params.push(value);
  for (const ext of normalized) params.push(`%.${ext}`);
}

function buildRelativePathExtensionPredicate(column: string, extensions: string[]): string {
  return extensions.map(() => `LOWER(${column}) LIKE ?`).join(' OR ');
}

function extensionsForFileType(fileType: string): string[] {
  return FILE_TYPE_EXTENSIONS[fileType.toLowerCase()] ?? [];
}

function extensionsForLanguage(language: string): string[] {
  const normalized = language.toLowerCase();
  return Object.entries(LANGUAGE_BY_EXTENSION)
    .filter(([, lang]) => lang === normalized)
    .map(([ext]) => ext);
}

function normalizeExtension(extension: string): string {
  return extension.replace(/^\.+/, '').toLowerCase();
}

function inferExtension(relativePath: string): string | null {
  const fileName = relativePath.split('/').pop() ?? relativePath;
  const lower = fileName.toLowerCase();
  for (const compound of ['d.mts', 'd.cts', 'd.ts']) {
    if (lower.endsWith(`.${compound}`)) return compound;
  }
  const dot = lower.lastIndexOf('.');
  return dot > 0 ? lower.slice(dot + 1) : null;
}

function inferFallbackMetadata(relativePath: string): {
  fileType: string | null;
  language: string | null;
  extension: string | null;
  isTest: boolean;
} {
  const extension = inferExtension(relativePath);
  const language = extension ? (LANGUAGE_BY_EXTENSION[extension] ?? null) : null;
  const fileType = extension
    ? (Object.entries(FILE_TYPE_EXTENSIONS).find(([, exts]) => exts.includes(extension))?.[0] ??
      null)
    : null;
  return {
    fileType,
    language,
    extension,
    isTest: isLikelyTestPath(relativePath),
  };
}

function isLikelyTestPath(relativePath: string): boolean {
  const path = relativePath.toLowerCase();
  return (
    path.startsWith('test/') ||
    path.startsWith('tests/') ||
    path.includes('/test/') ||
    path.includes('/tests/') ||
    path.includes('.test.') ||
    path.includes('.spec.') ||
    path.includes('_test.')
  );
}
// ── Helpers ──────────────────────────────────────────────────────────────

function mapTrackedFileRow(row: {
  relative_path: string;
  file_type: string | null;
  language: string | null;
  extension: string | null;
  is_test: number;
}): TrackedFileEntry {
  const inferred = inferFallbackMetadata(row.relative_path);
  return {
    relativePath: row.relative_path,
    fileType: row.file_type || inferred.fileType,
    language: row.language || inferred.language,
    extension: row.extension || inferred.extension,
    isTest: row.is_test === 1 || inferred.isTest,
  };
}
