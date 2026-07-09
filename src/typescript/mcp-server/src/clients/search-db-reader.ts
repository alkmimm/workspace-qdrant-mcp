/**
 * Read-only client for the FTS5 / file_metadata search database.
 *
 * Backs the admin UI's "largest files" view (Task #4 of the FTS5
 * size-guard series). The Rust daemon owns search.db writes — this
 * client opens it `readonly` so accidental schema drift / mutation
 * surfaces immediately. The daemon writes via WAL, so concurrent
 * reads from this handle don't interfere.
 *
 * NOT a `SqliteStateManager` extension because that class is wired
 * for state.db and a single dbPath; mixing the two would muddy ADR-003
 * ownership semantics. Both stay separate and read-only.
 */

import Database, { type Database as DatabaseType } from 'better-sqlite3';
import { existsSync } from 'node:fs';

import { getSearchDatabasePath } from '../utils/paths.js';
import { matchesPathInclude, matchesPathExclude } from '../utils/path-glob.js';

export interface SearchDbReaderConfig {
  dbPath?: string;
}

export interface LargeFileRow {
  file_id: number;
  tenant_id: string;
  /**
   * A file's `branches` (JSON array — one row is shared across every branch it's
   * indexed on, see layer-2 branch dedup) joined as `"main, feature/x"`; `"(none)"`
   * when the array is empty. On a not-yet-migrated search.db (bare `branch`
   * column) this is just that column's value, `"(none)"` when NULL.
   */
  branch: string;
  file_path: string;
  /** May be null for rows ingested before search.db v7. */
  size_bytes: number | null;
  /** Always 0 or 1 (search.db v8 `INTEGER NOT NULL DEFAULT 0`). */
  fts5_skipped: number;
}

export interface ListLargeFilesOptions {
  limit?: number;
  tenantId?: string;
  branch?: string;
  /** When true, return only rows where `fts5_skipped = 1`. */
  skippedOnly?: boolean;
}

export interface ChurnFileRow {
  file_id: number;
  tenant_id: string;
  /** Same projection as {@link LargeFileRow.branch} — joined `branches` array, or the legacy column. */
  branch: string;
  file_path: string;
  /** Number of times the daemon has (re)indexed this file's content (search.db v9). */
  reindex_count: number;
  /** RFC3339 UTC of first index; null for rows written before search.db v9. */
  first_indexed_at: string | null;
  /** May be null for rows ingested before search.db v7. */
  size_bytes: number | null;
}

export interface ListChurnFilesOptions {
  limit?: number;
  tenantId?: string;
  branch?: string;
  /** Only return files re-indexed at least this many times. Default 2 (≥1 re-index). */
  minReindexCount?: number;
}

export interface SearchBranchCountRow {
  tenant_id: string;
  branch: string;
  files: number;
  total_bytes: number | null;
}

export interface CountFilesMatchingPathFiltersOptions {
  tenantId: string;
  /** Include glob; floats exactly like the daemon-side FTS glob. */
  pathGlob?: string | undefined;
  /** Exclude glob; floats exactly like the daemon-side FTS glob. */
  pathExclude?: string | undefined;
  /** Stop counting once this many matches are found (caller only needs "> 0"). */
  limit?: number | undefined;
}

export type ReaderStatus =
  | { status: 'ok' }
  | { status: 'degraded'; reason: 'database_not_found' | 'database_error'; message: string };

const DEFAULT_LIMIT = 50;
const MAX_LIMIT = 500;

/**
 * Append a branch filter for the CURRENT schema (`branches` JSON array — one
 * row shared across every branch it's indexed on). `"(none)"` matches an empty
 * array; a concrete name tests array membership via `json_each`. Shared by
 * `listLargestFiles`/`listChurnFiles` so their branch semantics can't drift
 * from `listBranchCounts`, which established this pattern first.
 */
function pushBranchWhereCurrent(
  branch: string | undefined,
  where: string[],
  params: Record<string, string | number>
): void {
  if (branch === '(none)') {
    where.push('json_array_length(branches) = 0');
  } else if (branch) {
    where.push('EXISTS (SELECT 1 FROM json_each(branches) j WHERE j.value = @branch)');
    params['branch'] = branch;
  }
}

/**
 * Append a branch filter for the LEGACY schema (bare `branch` column, pre
 * branch-array migration) — the fallback path when the current-schema query
 * throws on a not-yet-migrated search.db.
 */
function pushBranchWhereLegacy(
  branch: string | undefined,
  where: string[],
  params: Record<string, string | number>
): void {
  if (branch === '(none)') {
    where.push('branch IS NULL');
  } else if (branch) {
    where.push('branch = @branch');
    params['branch'] = branch;
  }
}

export class SearchDbReader {
  private db: DatabaseType | null = null;
  private readonly dbPath: string;
  private initialized = false;

  constructor(config: SearchDbReaderConfig = {}) {
    this.dbPath = config.dbPath ?? getSearchDatabasePath();
  }

  /** Lazy open; returns degraded status if search.db is missing or won't open. */
  initialize(): ReaderStatus {
    if (this.initialized) return { status: 'ok' };
    if (!existsSync(this.dbPath)) {
      return {
        status: 'degraded',
        reason: 'database_not_found',
        message: `search.db not found at ${this.dbPath}. Daemon has not initialized yet.`,
      };
    }
    try {
      this.db = new Database(this.dbPath, { readonly: true, fileMustExist: true });
      this.initialized = true;
      return { status: 'ok' };
    } catch (error) {
      return {
        status: 'degraded',
        reason: 'database_error',
        message: `Failed to open search.db: ${error instanceof Error ? error.message : String(error)}`,
      };
    }
  }

  close(): void {
    if (this.db) {
      this.db.close();
      this.db = null;
      this.initialized = false;
    }
  }

  isConnected(): boolean {
    return this.initialized && this.db !== null;
  }

  getDatabasePath(): string {
    return this.dbPath;
  }

  /**
   * Return the N largest files in `file_metadata`, optionally filtered by
   * tenant_id and/or branch. Sort: `size_bytes DESC NULLS LAST`.
   *
   * Bounded by MAX_LIMIT to keep the admin UI responsive (and discourage
   * "give me everything" queries that the Prometheus gauges already cover
   * in aggregate form).
   */
  listLargestFiles(options: ListLargeFilesOptions = {}): LargeFileRow[] {
    const status = this.initialize();
    if (status.status !== 'ok' || !this.db) {
      return [];
    }
    const limit = Math.min(Math.max(options.limit ?? DEFAULT_LIMIT, 1), MAX_LIMIT);

    // Build WHERE clause dynamically — better-sqlite3 binds named params
    // safely. Branch=null is filterable via the special "(none)" sentinel
    // to match the Prometheus / Rust convention.
    const where: string[] = [];
    const params: Record<string, string | number> = { limit };
    if (options.tenantId) {
      where.push('tenant_id = @tenantId');
      params['tenantId'] = options.tenantId;
    }
    pushBranchWhereCurrent(options.branch, where, params);
    if (options.skippedOnly) {
      where.push('fts5_skipped = 1');
    }
    const whereSql = where.length > 0 ? `WHERE ${where.join(' AND ')}` : '';

    const sql = `
      SELECT
        file_id,
        tenant_id,
        CASE WHEN json_array_length(branches) = 0 THEN '(none)'
             ELSE (SELECT group_concat(value, ', ') FROM json_each(branches)) END AS branch,
        file_path,
        size_bytes,
        fts5_skipped
      FROM file_metadata
      ${whereSql}
      ORDER BY size_bytes DESC NULLS LAST, file_id DESC
      LIMIT @limit
    `;

    try {
      return this.db.prepare(sql).all(params) as LargeFileRow[];
    } catch {
      return this.listLargestFilesLegacy(options, limit);
    }
  }

  /**
   * Fallback for a search.db predating the branch-array migration (bare
   * `branch` column instead of `branches` JSON array) — mirrors
   * {@link listBranchCounts}'s two-tier fallback so a not-yet-migrated dev
   * database still works. Returns `[]` on any error, e.g. `file_metadata`
   * itself missing on a fresh, pre-v4 search.db — friendlier than throwing
   * since the admin UI polls this on every snapshot refresh.
   */
  private listLargestFilesLegacy(options: ListLargeFilesOptions, limit: number): LargeFileRow[] {
    if (!this.db) return [];
    const where: string[] = [];
    const params: Record<string, string | number> = { limit };
    if (options.tenantId) {
      where.push('tenant_id = @tenantId');
      params['tenantId'] = options.tenantId;
    }
    pushBranchWhereLegacy(options.branch, where, params);
    if (options.skippedOnly) {
      where.push('fts5_skipped = 1');
    }
    const whereSql = where.length > 0 ? `WHERE ${where.join(' AND ')}` : '';
    const sql = `
      SELECT file_id, tenant_id, COALESCE(branch, '(none)') AS branch, file_path, size_bytes, fts5_skipped
      FROM file_metadata
      ${whereSql}
      ORDER BY size_bytes DESC NULLS LAST, file_id DESC
      LIMIT @limit
    `;
    try {
      return this.db.prepare(sql).all(params) as LargeFileRow[];
    } catch {
      return [];
    }
  }

  /**
   * Return the N most-churned files in `file_metadata`, ranked by
   * `reindex_count DESC` (search.db v9). High counts flag files whose
   * content changes constantly — typically IDE/build-generated artifacts
   * (`.idea/`, `target/`, lockfiles, codegen output) that are good ignore
   * candidates. The caller pairs `reindex_count` with `first_indexed_at`
   * to derive a churn rate.
   *
   * Returns [] on any error (e.g. a pre-v9 search.db lacking the column),
   * matching `listLargestFiles` so the admin UI degrades gracefully.
   */
  listChurnFiles(options: ListChurnFilesOptions = {}): ChurnFileRow[] {
    const status = this.initialize();
    if (status.status !== 'ok' || !this.db) {
      return [];
    }
    const limit = Math.min(Math.max(options.limit ?? DEFAULT_LIMIT, 1), MAX_LIMIT);
    const minCount = Math.max(options.minReindexCount ?? 2, 1);

    const where: string[] = ['reindex_count >= @minCount'];
    const params: Record<string, string | number> = { limit, minCount };
    if (options.tenantId) {
      where.push('tenant_id = @tenantId');
      params['tenantId'] = options.tenantId;
    }
    pushBranchWhereCurrent(options.branch, where, params);

    const sql = `
      SELECT
        file_id,
        tenant_id,
        CASE WHEN json_array_length(branches) = 0 THEN '(none)'
             ELSE (SELECT group_concat(value, ', ') FROM json_each(branches)) END AS branch,
        file_path,
        reindex_count,
        first_indexed_at,
        size_bytes
      FROM file_metadata
      WHERE ${where.join(' AND ')}
      ORDER BY reindex_count DESC, file_id DESC
      LIMIT @limit
    `;

    try {
      return this.db.prepare(sql).all(params) as ChurnFileRow[];
    } catch {
      return this.listChurnFilesLegacy(options, limit, minCount);
    }
  }

  /**
   * Fallback for a search.db predating the branch-array migration. See
   * {@link listLargestFilesLegacy} — same rationale, applied to the churn
   * query's extra `reindex_count`/`first_indexed_at` columns. Also covers a
   * pre-v9 search.db lacking `reindex_count` entirely (degrades to `[]`
   * rather than throwing, since the admin UI polls this).
   */
  private listChurnFilesLegacy(
    options: ListChurnFilesOptions,
    limit: number,
    minCount: number
  ): ChurnFileRow[] {
    if (!this.db) return [];
    const where: string[] = ['reindex_count >= @minCount'];
    const params: Record<string, string | number> = { limit, minCount };
    if (options.tenantId) {
      where.push('tenant_id = @tenantId');
      params['tenantId'] = options.tenantId;
    }
    pushBranchWhereLegacy(options.branch, where, params);
    const sql = `
      SELECT
        file_id,
        tenant_id,
        COALESCE(branch, '(none)') AS branch,
        file_path,
        reindex_count,
        first_indexed_at,
        size_bytes
      FROM file_metadata
      WHERE ${where.join(' AND ')}
      ORDER BY reindex_count DESC, file_id DESC
      LIMIT @limit
    `;
    try {
      return this.db.prepare(sql).all(params) as ChurnFileRow[];
    } catch {
      return [];
    }
  }

  listBranchCounts(options: { tenantId?: string } = {}): SearchBranchCountRow[] {
    const status = this.initialize();
    if (status.status !== 'ok' || !this.db) return [];

    const where: string[] = [];
    const params: Record<string, string> = {};
    if (options.tenantId) {
      where.push('fm.tenant_id = @tenantId');
      params['tenantId'] = options.tenantId;
    }
    const whereSql = where.length > 0 ? `WHERE ${where.join(' AND ')}` : '';

    const sqlBranches = `
      SELECT
        fm.tenant_id,
        COALESCE(NULLIF(j.value, ''), '(none)') AS branch,
        COUNT(*) AS files,
        SUM(fm.size_bytes) AS total_bytes
      FROM file_metadata fm
      LEFT JOIN json_each(fm.branches) j
      ${whereSql}
      GROUP BY fm.tenant_id, branch
      ORDER BY fm.tenant_id, branch
    `;

    try {
      return this.db.prepare(sqlBranches).all(params) as SearchBranchCountRow[];
    } catch {
      const legacyWhereSql = whereSql.replaceAll('fm.tenant_id', 'tenant_id');
      const sqlLegacy = `
        SELECT
          tenant_id,
          COALESCE(branch, '(none)') AS branch,
          COUNT(*) AS files,
          SUM(size_bytes) AS total_bytes
        FROM file_metadata
        ${legacyWhereSql}
        GROUP BY tenant_id, branch
        ORDER BY tenant_id, branch
      `;
      try {
        return this.db.prepare(sqlLegacy).all(params) as SearchBranchCountRow[];
      } catch {
        return [];
      }
    }
  }

  /**
   * Count indexed files whose ABSOLUTE `file_path` satisfies the path filter —
   * matching `pathGlob` (if set) and NOT matching `pathExclude` (if set). This
   * is independent of any search pattern: it answers "does this filter select
   * any indexed file at all?", which the empty-result diagnosis uses to tell a
   * genuinely-empty filter (malformed / selects nothing → blame the shape) apart
   * from a well-formed filter over files that simply don't contain the pattern
   * (blame naming/casing, not the glob). Tenant-wide on purpose: glob
   * well-formedness is branch-independent, so a branch-membership quirk can't
   * make a valid glob look broken.
   *
   * Reuses the SAME floating matchers as the search post-filter
   * ({@link matchesPathInclude}/{@link matchesPathExclude}), which mirror the
   * daemon-side FTS glob — so this count agrees with what the daemon resolved.
   * Stops early at `limit` (the caller only needs to distinguish 0 from > 0).
   */
  countFilesMatchingPathFilters(options: CountFilesMatchingPathFiltersOptions): number {
    if (!options.pathGlob && !options.pathExclude) return 0;
    const status = this.initialize();
    if (status.status !== 'ok' || !this.db) return 0;

    const cap = Math.max(options.limit ?? DEFAULT_LIMIT, 1);
    try {
      const stmt = this.db.prepare(
        'SELECT DISTINCT file_path FROM file_metadata WHERE tenant_id = @tenantId'
      );
      let count = 0;
      for (const row of stmt.iterate({ tenantId: options.tenantId }) as Iterable<{
        file_path: string;
      }>) {
        const path = row.file_path;
        if (typeof path !== 'string' || path.length === 0) continue;
        if (options.pathGlob && !matchesPathInclude(path, options.pathGlob)) continue;
        if (options.pathExclude && matchesPathExclude(path, options.pathExclude)) continue;
        count += 1;
        if (count >= cap) break;
      }
      return count;
    } catch {
      // file_metadata absent on a fresh search.db, or any read error — degrade
      // to 0 so the diagnosis falls back to its shape-oriented message.
      return 0;
    }
  }
}
