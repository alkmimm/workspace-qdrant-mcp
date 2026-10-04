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
import { expandBraces } from '../../utils/path-glob.js';
import { getDefaultBranch } from '../../utils/git-utils.js';

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
  /**
   * Paths `fallbackBranch` must never fill: those git reports as changed
   * between the two tips (deleted or modified on the branch — the trunk's copy
   * is not the branch's). See `tools/fallback-guard.ts`.
   */
  fallbackRefusedPaths?: readonly string[];
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

/** Ceiling on the `**\/` variants one pattern may produce (2^k for k segments).
 *  Real patterns carry one or two; past the cap only the "one or more
 *  directories" reading is kept, which is what this engine did for all of them
 *  before. */
const MAX_DOUBLE_STAR_VARIANTS = 16;

/**
 * Translate `**\/` for an engine that does not have it.
 *
 * `**\/` means "any number of directories, INCLUDING ZERO". SQLite GLOB has no
 * `**`, and this builder used to collapse it to `*\/` — which forces a literal
 * slash, so `**\/*.md` skipped every root-level file, `src/**\/*.rs` skipped
 * `src/main.rs`, and a caller comparing `list` against `grep` or semantic
 * `search` (where `**\/` compiles to an OPTIONAL `(?:.*\/)?`) saw the same glob
 * return different sets. Emitting BOTH readings per segment — dropped and
 * `*\/` — covers zero and non-zero depth, and the `OR` in
 * {@link pushGlobClause} unions them (its `AND` of `NOT GLOB` complements them
 * for an exclude). Any `**` not followed by `/` collapses to `*` as before.
 */
function doubleStarVariants(glob: string): string[] {
  const index = glob.indexOf('**/');
  if (index === -1) return [glob.replace(/\*\*/g, '*')];
  const prefix = glob.slice(0, index).replace(/\*\*/g, '*');
  const variants: string[] = [];
  for (const tail of doubleStarVariants(glob.slice(index + 3))) {
    variants.push(`${prefix}${tail}`, `${prefix}*/${tail}`);
    if (variants.length >= MAX_DOUBLE_STAR_VARIANTS) {
      return [`${prefix}*/${tail}`];
    }
  }
  return variants;
}

/**
 * The floating SQLite GLOB patterns ONE brace-free caller pattern expands to —
 * the single source of truth for both the include (`glob`) and the exclude
 * (`excludeGlob`), which apply it through {@link pushGlobClause} so their
 * semantics can never drift. Mirrors the daemon's `normalize_path_glob`
 * (src/rust/.../text_search/escaping.rs) and the TS `matchesFloatingGlob`
 * (src/.../utils/path-glob.ts):
 *
 *   - Every `**\/` segment is first expanded to its zero-directory AND
 *     one-or-more-directory readings (see {@link doubleStarVariants}).
 *   - Already-floating (leading `*`) or absolute (leading `/`) → matched verbatim.
 *   - A pattern that carries a wildcard (`V*.sql`, `src/*.rs`) floats at the repo
 *     root AND at any nested depth.
 *   - A WILDCARD-FREE literal (`tool-builders`, `src/main.rs`) is a PATH the
 *     caller wants to scope to: match the exact path OR its whole subtree at any
 *     depth. A trailing slash is unambiguously a directory (subtree only).
 *     Without this a bare directory name floated only as a same-named FILE at
 *     any depth, so `glob:"pkg"` selected nothing and `excludeGlob:"node_modules"`
 *     dropped nothing — the files live UNDER the directory, not AT it.
 *
 * SQLite GLOB anchors both ends and its `*` crosses `/`, so `dir/*` already spans
 * an arbitrarily deep subtree.
 */
function sqlGlobPatternsFor(glob: string): string[] {
  // Classified from the ORIGINAL pattern: the `**/` expansion below can strip
  // the only wildcard out of a variant (`a/**/b` → `a/b`), and treating that
  // variant as a wildcard-free literal would hand it directory-subtree
  // semantics the caller never asked for.
  const hadWildcard = /[*?[]/.test(glob);
  const patterns: string[] = [];

  for (const collapsed of doubleStarVariants(glob)) {
    // Explicit floating (leading `*`) or absolute (leading `/`) → matched verbatim.
    if (collapsed.startsWith('*') || collapsed.startsWith('/')) {
      patterns.push(collapsed);
      continue;
    }

    // A relative pattern that still carries a wildcard floats at root + any depth.
    if (hadWildcard) {
      patterns.push(collapsed, `*/${collapsed}`);
      continue;
    }

    // Wildcard-free literal → exact path OR whole subtree at any depth; a trailing
    // slash is directory-only.
    const dir = collapsed.replace(/\/+$/, '');
    if (!collapsed.endsWith('/')) patterns.push(dir, `*/${dir}`); // exact file, root or nested
    patterns.push(`${dir}/*`, `*/${dir}/*`); // its subtree, root or nested
  }

  return patterns;
}

/**
 * Append the clause for one caller glob. Brace alternation is expanded first
 * (`expandBraces`, the same helper the semantic matcher uses) and every
 * alternative contributes its own patterns to the SAME clause.
 *
 * SQLite's `GLOB` operator implements `*`, `?` and `[…]` but NOT `{a,b}`, so
 * before this a braced pattern reached the database as a literal and selected
 * nothing — `list pattern:"**\/*.{rs,ts}"` returned zero files while the
 * daemon-side FTS glob behind `grep` answered the identical pattern with 28
 * matches (measured 2026-09-05). Union semantics hold under negation too:
 * `negate` turns each `GLOB` into `NOT GLOB` and every `OR` into `AND`, so the
 * exclude stays the exact complement of the include across every alternative.
 */
function pushGlobClause(
  conditions: string[],
  params: (string | number)[],
  column: string,
  glob: string,
  negate = false
): void {
  const op = negate ? 'NOT GLOB' : 'GLOB';
  const join = negate ? ' AND ' : ' OR ';
  const patterns = [...new Set(expandBraces(glob).flatMap(sqlGlobPatternsFor))];
  conditions.push(`(${patterns.map(() => `${column} ${op} ?`).join(join)})`);
  params.push(...patterns);
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
    // whose path is NOT overridden by a same-path entry on `branch` and that
    // git does not report as changed between the tips (a file the branch
    // deleted must not reappear from the trunk).
    conditions.push(
      '(EXISTS (SELECT 1 FROM json_each(branches) WHERE value = ?) OR (EXISTS (SELECT 1 FROM json_each(branches) WHERE value = ?) AND relative_path NOT IN ' +
        '(SELECT relative_path FROM tracked_files WHERE watch_folder_id = ? AND EXISTS (SELECT 1 FROM json_each(branches) WHERE value = ?))' +
        ' AND relative_path NOT IN (SELECT value FROM json_each(?))))'
    );
    params.push(
      branch,
      fallbackBranch,
      options.watchFolderId,
      branch,
      JSON.stringify(options.fallbackRefusedPaths ?? [])
    );
  } else if (branch) {
    conditions.push('EXISTS (SELECT 1 FROM json_each(branches) WHERE value = ?)');
    params.push(branch);
  }
  if (glob) {
    pushGlobClause(conditions, params, 'relative_path', glob);
  }
  if (excludeGlob) {
    pushGlobClause(conditions, params, 'relative_path', excludeGlob, true);
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

    // GROUP BY path: `tracked_files` holds one row per content GENERATION, so a
    // path tracked on several branches is several rows. Under the default
    // concrete-branch filter that is exactly one row per path (measured: 1975
    // rows / 1975 distinct paths), but a `branch:"*"` sweep returned the same
    // file once per branch — 2230 rows for 1976 files on this repo — which both
    // duplicated entries and shortened every page by the repeats it spent. One
    // entry per file is what a caller means by "list the files".
    // MAX() keeps the pick deterministic; for `is_test` it also means "a test if
    // any generation says so", the same rule getIsTestByFilePaths applies.
    const sql = `
      SELECT relative_path,
             MAX(file_type) AS file_type,
             MAX(language) AS language,
             MAX(extension) AS extension,
             MAX(is_test) AS is_test
      FROM tracked_files
      WHERE ${conditions.join(' AND ')}
      GROUP BY relative_path
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
    // DISTINCT for the same reason the listing groups by path: one entry per
    // file, so the total describes what paging can actually return.
    const sql = `
      SELECT COUNT(DISTINCT relative_path) as cnt
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
  for (const [abs, annotation] of getFileAnnotationsByFilePaths(db, watchFolderId, filePaths)) {
    if (annotation.isTest !== undefined) out.set(abs, annotation.isTest);
  }
  return out;
}

/** What `tracked_files` can say about a file the FTS surfaces found on disk. */
export interface TrackedFileAnnotation {
  /** Repo-relative path — always present (derived from the watch-folder root). */
  relativePath: string;
  /** Daemon `is_test` verdict; `undefined` when no row covers the path. */
  isTest: boolean | undefined;
  /** Detected language; `null` when unknown or no row covers the path. */
  language: string | null;
}

/**
 * Absolute path → what the daemon knows about that file: its repo-relative path,
 * `is_test`, and `language`.
 *
 * One query for all three because the FTS read surfaces need all three and
 * already paid for this lookup to get `is_test` alone. A keyword/exact hit used
 * to carry only `file_path` while a semantic hit carried `relative_path`,
 * `language` and `branch` — so an agent reading `relative_path` silently got
 * nothing the moment it set `exact: true`. The fields now match across modes.
 *
 * `relativePath` is computed from the watch-folder root rather than read from the
 * row, so it is available even for a file the index has not tracked yet; the row
 * supplies only what the daemon alone can know. `MAX(is_test)` collapses the
 * multiple generations a path can have. Chunked to stay under SQLite's
 * bound-parameter limit. Best-effort throughout: any failure yields an empty map.
 */
export function getFileAnnotationsByFilePaths(
  db: DatabaseType | null,
  watchFolderId: string,
  filePaths: readonly string[]
): Map<string, TrackedFileAnnotation> {
  const out = new Map<string, TrackedFileAnnotation>();
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
    for (const [rel, absList] of absByRel) {
      for (const abs of absList) {
        out.set(abs, { relativePath: rel, isTest: undefined, language: null });
      }
    }
    const relPaths = [...absByRel.keys()];
    const CHUNK = 400; // SQLite's default max bound parameters is 999
    for (let i = 0; i < relPaths.length; i += CHUNK) {
      const chunk = relPaths.slice(i, i + CHUNK);
      const placeholders = chunk.map(() => '?').join(',');
      const rows = db
        .prepare(
          `SELECT relative_path, MAX(is_test) AS is_test, MAX(language) AS language
             FROM tracked_files
           WHERE watch_folder_id = ? AND relative_path IN (${placeholders})
           GROUP BY relative_path`
        )
        .all(watchFolderId, ...chunk) as Array<{
        relative_path: string;
        is_test: number | null;
        language: string | null;
      }>;
      for (const row of rows) {
        for (const abs of absByRel.get(row.relative_path) ?? []) {
          out.set(abs, {
            relativePath: row.relative_path,
            isTest: row.is_test === 1,
            language: row.language,
          });
        }
      }
    }
  } catch {
    out.clear(); // annotation is best-effort — never fail the read
  }
  return out;
}

/**
 * Resolve the branch a read on `effectiveBranch` should widen to for files that
 * branch does not carry — or `null` when it should not widen at all.
 *
 * The daemon tags only CHANGED files under a feature branch, so a file
 * unchanged on that branch stays indexed under the trunk and is invisible to a
 * strict branch filter. Widening to the trunk repairs that. Widening on the
 * TRUNK itself repairs nothing and actively harms: there is no "unchanged
 * elsewhere" set to recover, so every extra branch admitted can only contribute
 * a stale content generation of a path the trunk already has.
 *
 * That is exactly what used to happen. This function took an `excludeBranch`
 * argument — always the caller's own branch — and returned "the branch with the
 * most tracked files that isn't you". On the trunk it could therefore never
 * return the trunk, which made the identical guard in `resolveFallbackBranch`
 * ("the effective branch already IS the base branch") unreachable. Measured on
 * this repo: a read on `main` widened into an abandoned `fix/...` branch holding
 * 1952 files, which re-served pre-edit generations of files changed on `main`
 * and inflated `grep`'s `total_matches` from 2033 to 4033 by counting the
 * overlapping set twice. Every read surface — grep, search (both lanes), list,
 * retrieve — calls this one function, so all of them were affected.
 *
 * The trunk is now resolved from GIT (see {@link getDefaultBranch}), which is
 * the authority for it; the index is only consulted when git cannot answer.
 * Returns `null` when the caller is already on the trunk, or when neither
 * source can name one.
 */
export function getBaseBranch(
  db: DatabaseType | null,
  watchFolderId: string,
  effectiveBranch: string
): string | null {
  if (!db) return null;
  try {
    const trunk = resolveTrunkBranch(db, watchFolderId, effectiveBranch);
    return trunk && trunk !== effectiveBranch ? trunk : null;
  } catch {
    return null;
  }
}

/**
 * The project's trunk, resolved from git and the index TOGETHER — neither is
 * sufficient alone.
 *
 * Git names the trunk correctly but can name one the index has never heard of:
 * the write path defaults a branch tag to `main`, so a repo whose git default is
 * `master` can hold its files under `main` (both names appear in this
 * deployment's data, in the same project). Trusting git blindly there would
 * point the fallback at an empty branch and, worse, report "already on the
 * trunk" for a caller on `master` whose files are all tagged `main`.
 *
 * The index names a branch that exists but cannot tell a trunk from a long-lived
 * feature branch: measured here, one project's most-tracked branch was
 * `fix/schedule-export-sheet-titles` (98.7%) against `main` (97.3%).
 *
 * So: take git's answer when the index actually has files under it, else the
 * most-tracked branch. The index fallback does NOT exclude `effectiveBranch` —
 * the dominant branch must be allowed to resolve to itself, which is precisely
 * the signal "already on the trunk, do not widen". `effectiveBranch` only breaks
 * an exact tie, where preferring the caller's own branch is the conservative
 * outcome (no widening).
 */
function resolveTrunkBranch(
  db: DatabaseType,
  watchFolderId: string,
  effectiveBranch: string
): string | null {
  const folder = db
    .prepare('SELECT path FROM watch_folders WHERE watch_id = ?')
    .get(watchFolderId) as { path: string } | undefined;
  if (folder?.path) {
    const fromGit = getDefaultBranch(folder.path);
    if (fromGit && branchHasTrackedFiles(db, watchFolderId, fromGit)) return fromGit;
  }
  const dominant = db
    .prepare(
      `SELECT je.value AS branch FROM tracked_files tf, json_each(tf.branches) je
         WHERE tf.watch_folder_id = ? AND je.value IS NOT NULL
         GROUP BY je.value
         ORDER BY COUNT(*) DESC, (je.value = ?) DESC, je.value ASC
         LIMIT 1`
    )
    .get(watchFolderId, effectiveBranch) as { branch: string } | undefined;
  return dominant?.branch ?? null;
}

/** Whether the index holds any file tagged under `branch` for this watch folder. */
function branchHasTrackedFiles(
  db: DatabaseType,
  watchFolderId: string,
  branch: string
): boolean {
  const row = db
    .prepare(
      `SELECT 1 AS present FROM tracked_files tf
         WHERE tf.watch_folder_id = ?
           AND EXISTS (SELECT 1 FROM json_each(tf.branches) WHERE value = ?)
         LIMIT 1`
    )
    .get(watchFolderId, branch) as { present: number } | undefined;
  return row !== undefined;
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
    SELECT DISTINCT m.relative_path
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
    -- DISTINCT because \`file_metadata\` holds one row per content GENERATION of a
    -- path, while the listing collapses to one entry per path. COUNT(*) made the
    -- total exceed what any amount of paging could return — the same
    -- "count rows, serve hits" mismatch that inflated grep's \`total_matches\`.
    SELECT COUNT(DISTINCT m.relative_path) AS cnt
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
      '(EXISTS (SELECT 1 FROM json_each(m.branches) WHERE value = ?) OR (EXISTS (SELECT 1 FROM json_each(m.branches) WHERE value = ?)' +
        ' AND m.relative_path NOT IN (SELECT value FROM json_each(?))))'
    );
    params.push(options.branch, fallbackBranch, JSON.stringify(options.fallbackRefusedPaths ?? []));
  } else if (options.branch) {
    conditions.push('EXISTS (SELECT 1 FROM json_each(m.branches) WHERE value = ?)');
    params.push(options.branch);
  }
  if (options.glob) {
    pushGlobClause(conditions, params, 'm.relative_path', options.glob);
  }
  if (options.excludeGlob) {
    pushGlobClause(conditions, params, 'm.relative_path', options.excludeGlob, true);
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
