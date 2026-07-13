/**
 * is_test parity for the FTS-backed read surfaces (grep / exact search).
 *
 * Semantic hits get the flag from the Qdrant ingest tags; FTS rows carry no
 * tags, so the daemon's verdict is read back from tracked_files.is_test by
 * absolute path — same classifier, second store. These tests cover the SQL
 * lookup (real in-memory SQLite), the best-effort wrapper guards, and the
 * end-to-end grep annotation.
 */

import { describe, it, expect, vi } from 'vitest';
import Database from 'better-sqlite3';
import { getIsTestByFilePaths } from '../../src/clients/tracked-files-queries/tracked-files.js';
import { lookupTestFlags, type TestFlagStateReader } from '../../src/tools/test-flag.js';
import { GrepTool } from '../../src/tools/grep.js';
import type { DaemonClient } from '../../src/clients/daemon-client.js';
import type { SqliteStateManager } from '../../src/clients/sqlite-state-manager.js';
import type { ProjectDetector } from '../../src/utils/project-detector.js';

function makeDb(): Database.Database {
  const db = new Database(':memory:');
  db.exec(
    `CREATE TABLE tracked_files (
       watch_folder_id TEXT NOT NULL,
       file_path TEXT NOT NULL,
       relative_path TEXT NOT NULL,
       is_test INTEGER NOT NULL DEFAULT 0
     )`
  );
  const ins = db.prepare(
    'INSERT INTO tracked_files (watch_folder_id, file_path, relative_path, is_test) VALUES (?, ?, ?, ?)'
  );
  ins.run('w1', '/proj/src/a.ts', 'src/a.ts', 0);
  ins.run('w1', '/proj/tests/a.test.ts', 'tests/a.test.ts', 1);
  ins.run('w-other', '/proj/src/a.ts', 'src/a.ts', 1); // other watch folder must not leak
  return db;
}

describe('getIsTestByFilePaths (SQL lookup)', () => {
  it('maps absolute paths to the daemon verdict, scoped to the watch folder', () => {
    const db = makeDb();
    const flags = getIsTestByFilePaths(db as never, 'w1', [
      '/proj/src/a.ts',
      '/proj/tests/a.test.ts',
      '/proj/not-tracked.ts',
    ]);
    expect(flags.get('/proj/src/a.ts')).toBe(false);
    expect(flags.get('/proj/tests/a.test.ts')).toBe(true);
    // Untracked path: absent from the map (unknown), never fabricated.
    expect(flags.has('/proj/not-tracked.ts')).toBe(false);
    db.close();
  });

  it('chunks past the SQLite bound-parameter limit', () => {
    const db = makeDb();
    const ins = db.prepare(
      'INSERT INTO tracked_files (watch_folder_id, file_path, relative_path, is_test) VALUES (?, ?, ?, ?)'
    );
    const paths: string[] = [];
    for (let i = 0; i < 950; i++) {
      const p = `/proj/gen/f${i}.ts`;
      ins.run('w1', p, `gen/f${i}.ts`, i % 2);
      paths.push(p);
    }
    const flags = getIsTestByFilePaths(db as never, 'w1', paths);
    expect(flags.size).toBe(950);
    expect(flags.get('/proj/gen/f1.ts')).toBe(true);
    expect(flags.get('/proj/gen/f2.ts')).toBe(false);
    db.close();
  });

  it('returns an empty map on a null db or empty input', () => {
    expect(getIsTestByFilePaths(null, 'w1', ['/x']).size).toBe(0);
    const db = makeDb();
    expect(getIsTestByFilePaths(db as never, 'w1', []).size).toBe(0);
    db.close();
  });
});

describe('lookupTestFlags (best-effort wrapper)', () => {
  const reader = (over: Partial<TestFlagStateReader> = {}): TestFlagStateReader =>
    ({
      getWatchFolderIdByTenantId: vi.fn().mockReturnValue('w1'),
      getIsTestByFilePaths: vi.fn().mockReturnValue(new Map([['/p/t.test.ts', true]])),
      ...over,
    }) as TestFlagStateReader;

  it('resolves the watch folder from the tenant and returns the flag map', () => {
    const flags = lookupTestFlags(reader(), 't1', ['/p/t.test.ts']);
    expect(flags.get('/p/t.test.ts')).toBe(true);
  });

  it('returns an empty map without a state manager, tenant, or watch folder', () => {
    expect(lookupTestFlags(undefined, 't1', ['/p']).size).toBe(0);
    expect(lookupTestFlags(reader(), undefined, ['/p']).size).toBe(0);
    expect(
      lookupTestFlags(
        reader({ getWatchFolderIdByTenantId: vi.fn().mockReturnValue(null) }),
        't1',
        ['/p']
      ).size
    ).toBe(0);
  });

  it('swallows lookup failures — annotation must never fail the search', () => {
    const throwing = reader({
      getWatchFolderIdByTenantId: vi.fn().mockImplementation(() => {
        throw new Error('db locked');
      }),
    });
    expect(lookupTestFlags(throwing, 't1', ['/p']).size).toBe(0);
  });
});

describe('GrepTool — matches from test files carry is_test (project scope)', () => {
  function makeDaemon(): DaemonClient {
    const textSearch = vi.fn().mockResolvedValue({
      matches: [
        {
          file_path: '/proj/src/a.ts',
          line_number: 1,
          content: 'answer()',
          context_before: [],
          context_after: [],
        },
        {
          file_path: '/proj/tests/a.test.ts',
          line_number: 9,
          content: 'answer()',
          context_before: [],
          context_after: [],
        },
      ],
      total_matches: 2,
      truncated: false,
    });
    const target: Record<string, unknown> = { textSearch };
    return new Proxy(target, {
      get(t: Record<string, unknown>, prop: string | symbol) {
        if (typeof prop === 'string' && prop in t) return t[prop];
        if (prop === 'then' || typeof prop === 'symbol') return undefined;
        return () => Promise.resolve(undefined);
      },
    }) as unknown as DaemonClient;
  }

  it('annotates only the test-classified match, from the daemon verdict', async () => {
    const stateManager = {
      getWatchFolderIdByTenantId: vi.fn().mockReturnValue('w1'),
      getBaseBranch: vi.fn().mockReturnValue(null),
      getIsTestByFilePaths: vi
        .fn()
        .mockReturnValue(new Map([['/proj/tests/a.test.ts', true]])),
    } as unknown as SqliteStateManager;
    const detector = {
      getProjectInfo: vi.fn().mockResolvedValue({ projectId: 't1', projectPath: '/proj' }),
    } as unknown as ProjectDetector;
    const tool = new GrepTool(makeDaemon(), detector, stateManager);

    const res = await tool.grep({ pattern: 'answer', scope: 'project', projectId: 't1' });

    expect(res.matches).toHaveLength(2);
    const byFile = new Map(res.matches.map((m) => [m.file, m]));
    expect(byFile.get('/proj/tests/a.test.ts')?.is_test).toBe(true);
    expect(byFile.get('/proj/src/a.ts')?.is_test).toBeUndefined();
  });

  it('skips annotation for cross-tenant scope:"all" sweeps', async () => {
    const stateManager = {
      getWatchFolderIdByTenantId: vi.fn(),
      getBaseBranch: vi.fn().mockReturnValue(null),
      getIsTestByFilePaths: vi.fn(),
    } as unknown as SqliteStateManager;
    const tool = new GrepTool(makeDaemon(), {} as ProjectDetector, stateManager);

    const res = await tool.grep({ pattern: 'answer', scope: 'all' });

    expect(res.matches.every((m) => m.is_test === undefined)).toBe(true);
    expect(stateManager.getIsTestByFilePaths).not.toHaveBeenCalled();
  });
});
