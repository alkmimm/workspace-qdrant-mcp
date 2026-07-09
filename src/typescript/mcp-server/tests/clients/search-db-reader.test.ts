/**
 * Unit tests for SearchDbReader.
 *
 * Two schema fixtures, matching the two code paths in the reader:
 *   - CURRENT: `branches` JSON array (search.db v10+, layer-2 branch dedup —
 *     one row shared across every branch it's indexed on). This is what a
 *     live search.db actually looks like today.
 *   - LEGACY: bare `branch` column (pre branch-array migration). Exercises
 *     the fallback path a not-yet-migrated dev search.db would hit.
 *
 * A real on-disk SQLite database is built for each, seeded, and exercised
 * across every filter combo. The CURRENT-schema suite is the one that matters
 * for a live deployment — a prior version of this file only ever seeded the
 * LEGACY shape, which silently masked `listLargestFiles`/`listChurnFiles`
 * throwing "no such column: branch" against a real search.db (both queries
 * degrade to `[] on error`, so the admin UI just showed "no files indexed").
 */

import { afterEach, beforeEach, describe, expect, it } from 'vitest';
import Database from 'better-sqlite3';
import { mkdtempSync, rmSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';

import { SearchDbReader } from '../../src/clients/search-db-reader.js';

// ── CURRENT schema: `branches` JSON array ───────────────────────────────────

const CREATE_FILE_METADATA_CURRENT = `
  CREATE TABLE IF NOT EXISTS file_metadata (
    file_id INTEGER PRIMARY KEY,
    tenant_id TEXT NOT NULL,
    branches TEXT NOT NULL DEFAULT '[]',
    file_path TEXT NOT NULL,
    size_bytes INTEGER,
    fts5_skipped INTEGER NOT NULL DEFAULT 0,
    reindex_count INTEGER NOT NULL DEFAULT 0,
    first_indexed_at TEXT,
    base_point TEXT,
    relative_path TEXT,
    file_hash TEXT
  )
`;

/** [file_id, tenant, branches[], path, size_bytes, fts5_skipped, reindex_count] */
const FIXTURE_CURRENT: Array<[number, string, string[], string, number | null, number, number]> = [
  [1, 'proj-a', ['main'], '/a.rs', 100, 0, 1],
  [2, 'proj-a', ['main'], '/b.rs', 200, 0, 5],
  [3, 'proj-a', ['main'], '/big.csv', 50_000, 1, 1],
  [4, 'proj-a', ['feature/x'], '/c.rs', 75, 0, 1],
  [5, 'proj-b', ['main'], '/d.rs', 999, 0, 1],
  // Empty branches array — exercises the "(none)" filter path.
  [6, 'proj-b', [], '/orphan.md', 42, 0, 3],
  // NULL size_bytes — should sort last under DESC NULLS LAST.
  [7, 'proj-c', ['main'], '/legacy.md', null, 0, 1],
  // Belongs to TWO branches (the dedup case) — proves the joined-string
  // projection, and doubles as a distinct "matches branch=main" row.
  [8, 'proj-a', ['main', 'feature/y'], '/shared.rs', 150, 0, 8],
];

function seedCurrent(dbPath: string): void {
  const db = new Database(dbPath);
  db.exec(CREATE_FILE_METADATA_CURRENT);
  const stmt = db.prepare(
    'INSERT INTO file_metadata (file_id, tenant_id, branches, file_path, size_bytes, fts5_skipped, reindex_count, first_indexed_at) VALUES (?, ?, ?, ?, ?, ?, ?, ?)'
  );
  for (const [id, tenant, branches, path, size, skipped, reindexCount] of FIXTURE_CURRENT) {
    stmt.run(id, tenant, JSON.stringify(branches), path, size, skipped, reindexCount, '2026-01-01T00:00:00.000Z');
  }
  db.close();
}

// ── LEGACY schema: bare `branch` column ─────────────────────────────────────

const CREATE_FILE_METADATA_LEGACY = `
  CREATE TABLE IF NOT EXISTS file_metadata (
    file_id INTEGER PRIMARY KEY,
    tenant_id TEXT NOT NULL,
    branch TEXT,
    file_path TEXT NOT NULL,
    size_bytes INTEGER,
    fts5_skipped INTEGER NOT NULL DEFAULT 0,
    reindex_count INTEGER NOT NULL DEFAULT 0,
    first_indexed_at TEXT,
    base_point TEXT,
    relative_path TEXT,
    file_hash TEXT
  )
`;

/** [file_id, tenant, branch, path, size_bytes, fts5_skipped, reindex_count] */
const FIXTURE_LEGACY: Array<[number, string, string | null, string, number | null, number, number]> = [
  [1, 'proj-a', 'main', '/a.rs', 100, 0, 1],
  [2, 'proj-a', 'main', '/b.rs', 200, 0, 5],
  [3, 'proj-a', 'main', '/big.csv', 50_000, 1, 1],
  [4, 'proj-a', 'feature/x', '/c.rs', 75, 0, 1],
  [5, 'proj-b', 'main', '/d.rs', 999, 0, 1],
  // NULL branch — exercises the "(none)" filter path.
  [6, 'proj-b', null, '/orphan.md', 42, 0, 3],
  // NULL size_bytes — should sort last under DESC NULLS LAST.
  [7, 'proj-c', 'main', '/legacy.md', null, 0, 1],
];

function seedLegacy(dbPath: string): void {
  const db = new Database(dbPath);
  db.exec(CREATE_FILE_METADATA_LEGACY);
  const stmt = db.prepare(
    'INSERT INTO file_metadata (file_id, tenant_id, branch, file_path, size_bytes, fts5_skipped, reindex_count, first_indexed_at) VALUES (?, ?, ?, ?, ?, ?, ?, ?)'
  );
  for (const [id, tenant, branch, path, size, skipped, reindexCount] of FIXTURE_LEGACY) {
    stmt.run(id, tenant, branch, path, size, skipped, reindexCount, '2026-01-01T00:00:00.000Z');
  }
  db.close();
}

function withTmpDb(seed: (dbPath: string) => void) {
  let tmpDir = '';
  let dbPath = '';
  let reader: SearchDbReader = null as unknown as SearchDbReader;

  beforeEach(() => {
    tmpDir = mkdtempSync(join(tmpdir(), 'wqm-search-db-reader-'));
    dbPath = join(tmpDir, 'search.db');
    seed(dbPath);
    reader = new SearchDbReader({ dbPath });
  });

  afterEach(() => {
    reader.close();
    rmSync(tmpDir, { recursive: true, force: true });
  });

  return {
    reader: () => reader,
    dbPath: () => dbPath,
    tmpDir: () => tmpDir,
  };
}

// ── CURRENT schema (branches JSON array) — the live production shape ───────

describe('SearchDbReader — listLargestFiles (current schema, branches array)', () => {
  const ctx = withTmpDb(seedCurrent);

  it('lists files ordered by size_bytes DESC with NULLs last', () => {
    const rows = ctx.reader().listLargestFiles({ limit: 10 });
    expect(rows.map((r) => r.file_id)).toEqual([
      3, // 50000
      5, // 999
      2, // 200
      8, // 150
      1, // 100
      4, // 75
      6, // 42
      7, // NULL → last
    ]);
  });

  it('joins a multi-branch file\'s branches into a display string', () => {
    // Proves the CURRENT (JSON-array) query ran — only it can produce a
    // comma-joined value; the legacy fallback never would.
    const rows = ctx.reader().listLargestFiles({ tenantId: 'proj-a', limit: 10 });
    const shared = rows.find((r) => r.file_id === 8);
    expect(shared?.branch).toBe('main, feature/y');
  });

  it('respects the limit parameter and clamps to DEFAULT', () => {
    expect(ctx.reader().listLargestFiles({ limit: 3 })).toHaveLength(3);
    expect(ctx.reader().listLargestFiles({ limit: 0 }).length).toBeGreaterThan(0); // clamped to >=1
  });

  it('filters by tenant_id', () => {
    const rows = ctx.reader().listLargestFiles({ tenantId: 'proj-a' });
    expect(rows.every((r) => r.tenant_id === 'proj-a')).toBe(true);
    expect(rows.map((r) => r.file_id).sort()).toEqual([1, 2, 3, 4, 8]);
  });

  it('filters by branch via array membership', () => {
    const rows = ctx.reader().listLargestFiles({ branch: 'feature/x' });
    expect(rows.map((r) => r.file_id)).toEqual([4]);
    // id=8 is on ["main","feature/y"] — must NOT match "feature/x".
    expect(rows.map((r) => r.file_id)).not.toContain(8);
  });

  it('a multi-branch file matches EACH of its branches', () => {
    const main = ctx.reader().listLargestFiles({ branch: 'main', limit: 20 });
    expect(main.map((r) => r.file_id).sort((a, b) => a - b)).toEqual([1, 2, 3, 5, 7, 8]);
    const featureY = ctx.reader().listLargestFiles({ branch: 'feature/y' });
    expect(featureY.map((r) => r.file_id)).toEqual([8]);
  });

  it('translates branch "(none)" to an empty branches array', () => {
    const rows = ctx.reader().listLargestFiles({ branch: '(none)' });
    expect(rows.map((r) => r.file_id)).toEqual([6]);
    expect(rows[0]?.branch).toBe('(none)');
  });

  it('combines tenant_id + branch filters', () => {
    const rows = ctx.reader().listLargestFiles({ tenantId: 'proj-a', branch: 'main' });
    expect(rows.map((r) => r.file_id).sort((a, b) => a - b)).toEqual([1, 2, 3, 8]);
  });

  it('returns only fts5_skipped=1 rows when skippedOnly is true', () => {
    const rows = ctx.reader().listLargestFiles({ skippedOnly: true });
    expect(rows.map((r) => r.file_id)).toEqual([3]);
    expect(rows[0]?.fts5_skipped).toBe(1);
  });

  it('returns degraded status when search.db is missing', () => {
    const missing = new SearchDbReader({ dbPath: join(ctx.tmpDir(), 'does-not-exist.db') });
    const status = missing.initialize();
    expect(status.status).toBe('degraded');
    if (status.status === 'degraded') {
      expect(status.reason).toBe('database_not_found');
    }
    expect(missing.listLargestFiles()).toEqual([]);
  });

  it('returns [] when file_metadata table is missing entirely', () => {
    const db = new Database(ctx.dbPath());
    db.exec('DROP TABLE file_metadata');
    db.close();
    const noTable = new SearchDbReader({ dbPath: ctx.dbPath() });
    expect(noTable.listLargestFiles()).toEqual([]);
    noTable.close();
  });

  describe('countFilesMatchingPathFilters', () => {
    // proj-a files: /a.rs, /b.rs, /big.csv, /c.rs, /shared.rs
    it('counts files matching a pathGlob (floats like the daemon glob)', () => {
      expect(
        ctx.reader().countFilesMatchingPathFilters({ tenantId: 'proj-a', pathGlob: '**/*.rs' })
      ).toBe(4);
      expect(
        ctx.reader().countFilesMatchingPathFilters({ tenantId: 'proj-a', pathGlob: '**/*.csv' })
      ).toBe(1);
    });

    it('returns 0 for a well-formed glob that selects no file (the false-blame case)', () => {
      expect(
        ctx.reader().countFilesMatchingPathFilters({ tenantId: 'proj-a', pathGlob: '**/*.proto' })
      ).toBe(0);
    });

    it('honors pathExclude and combines it with pathGlob', () => {
      expect(
        ctx.reader().countFilesMatchingPathFilters({ tenantId: 'proj-a', pathExclude: '**/*.rs' })
      ).toBe(1); // only /big.csv
      expect(
        ctx.reader().countFilesMatchingPathFilters({
          tenantId: 'proj-a',
          pathGlob: '**/*.rs',
          pathExclude: '**/b.rs',
        })
      ).toBe(3); // /a.rs, /c.rs, /shared.rs
    });

    it('is tenant-scoped', () => {
      expect(
        ctx.reader().countFilesMatchingPathFilters({ tenantId: 'proj-b', pathGlob: '**/*.rs' })
      ).toBe(1);
    });

    it('caps the count at the limit (only "> 0" matters to the caller)', () => {
      expect(
        ctx
          .reader()
          .countFilesMatchingPathFilters({ tenantId: 'proj-a', pathGlob: '**/*.rs', limit: 2 })
      ).toBe(2);
    });

    it('returns 0 when no path filter is supplied', () => {
      expect(ctx.reader().countFilesMatchingPathFilters({ tenantId: 'proj-a' })).toBe(0);
    });
  });
});

describe('SearchDbReader — listChurnFiles (current schema, branches array)', () => {
  const ctx = withTmpDb(seedCurrent);

  it('ranks by reindex_count DESC, default minReindexCount=2 excludes count=1 rows', () => {
    const rows = ctx.reader().listChurnFiles({ limit: 10 });
    // reindex_count: 8=8, 2=5, 6=3 all >= 2; 1/3/4/5/7 all =1, excluded.
    expect(rows.map((r) => r.file_id)).toEqual([8, 2, 6]);
  });

  it('respects an explicit minReindexCount', () => {
    const rows = ctx.reader().listChurnFiles({ minReindexCount: 1, limit: 10 });
    expect(rows.length).toBe(FIXTURE_CURRENT.length);
  });

  it('filters by tenant_id', () => {
    const rows = ctx.reader().listChurnFiles({ tenantId: 'proj-a', limit: 10 });
    expect(rows.every((r) => r.tenant_id === 'proj-a')).toBe(true);
    expect(rows.map((r) => r.file_id).sort((a, b) => a - b)).toEqual([2, 8]);
  });

  it('filters by branch via array membership, including the "(none)" sentinel', () => {
    const main = ctx.reader().listChurnFiles({ branch: 'main', limit: 10 });
    expect(main.map((r) => r.file_id).sort((a, b) => a - b)).toEqual([2, 8]);
    const none = ctx.reader().listChurnFiles({ branch: '(none)', limit: 10 });
    expect(none.map((r) => r.file_id)).toEqual([6]);
    expect(none[0]?.branch).toBe('(none)');
  });

  it('carries reindex_count and first_indexed_at through', () => {
    const rows = ctx.reader().listChurnFiles({ tenantId: 'proj-a', limit: 10 });
    const top = rows.find((r) => r.file_id === 8);
    expect(top?.reindex_count).toBe(8);
    expect(top?.first_indexed_at).toBe('2026-01-01T00:00:00.000Z');
  });
});

// ── LEGACY schema (bare `branch` column) — fallback coverage ────────────────

describe('SearchDbReader — listLargestFiles (legacy schema, bare branch column)', () => {
  const ctx = withTmpDb(seedLegacy);

  it('falls back to the legacy query and lists files ordered by size_bytes DESC with NULLs last', () => {
    const rows = ctx.reader().listLargestFiles({ limit: 10 });
    expect(rows.map((r) => r.file_id)).toEqual([3, 5, 2, 1, 4, 6, 7]);
  });

  it('filters by branch (literal value)', () => {
    const rows = ctx.reader().listLargestFiles({ branch: 'feature/x' });
    expect(rows.map((r) => r.file_id)).toEqual([4]);
  });

  it('translates branch "(none)" to IS NULL', () => {
    const rows = ctx.reader().listLargestFiles({ branch: '(none)' });
    expect(rows.map((r) => r.file_id)).toEqual([6]);
    expect(rows[0]?.branch).toBe('(none)');
  });

  it('combines tenant_id + branch filters', () => {
    const rows = ctx.reader().listLargestFiles({ tenantId: 'proj-a', branch: 'main' });
    expect(rows.map((r) => r.file_id).sort()).toEqual([1, 2, 3]);
  });

  it('returns only fts5_skipped=1 rows when skippedOnly is true', () => {
    const rows = ctx.reader().listLargestFiles({ skippedOnly: true });
    expect(rows.map((r) => r.file_id)).toEqual([3]);
  });
});

describe('SearchDbReader — listChurnFiles (legacy schema, bare branch column)', () => {
  const ctx = withTmpDb(seedLegacy);

  it('falls back to the legacy query and ranks by reindex_count DESC', () => {
    const rows = ctx.reader().listChurnFiles({ limit: 10 });
    expect(rows.map((r) => r.file_id)).toEqual([2, 6]); // reindex_count 5, 3 (>= default minCount 2)
  });

  it('filters by branch (literal value) including "(none)"', () => {
    const main = ctx.reader().listChurnFiles({ branch: 'main', minReindexCount: 1, limit: 10 });
    expect(main.map((r) => r.file_id).sort((a, b) => a - b)).toEqual([1, 2, 3, 5, 7]);
    const none = ctx.reader().listChurnFiles({ branch: '(none)', minReindexCount: 1 });
    expect(none.map((r) => r.file_id)).toEqual([6]);
  });

  it('returns [] when even the legacy query fails (e.g. table missing)', () => {
    const db = new Database(ctx.dbPath());
    db.exec('DROP TABLE file_metadata');
    db.close();
    const noTable = new SearchDbReader({ dbPath: ctx.dbPath() });
    expect(noTable.listChurnFiles()).toEqual([]);
    noTable.close();
  });
});
