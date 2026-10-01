/**
 * Regression: `getBaseBranch` must be able to answer "you are already on the
 * trunk" — the answer that stops a read from widening into another branch.
 *
 * It could not. The function took an `excludeBranch` argument, always the
 * caller's own branch, and returned "the most-tracked branch that isn't you", so
 * on the trunk it returned a sibling branch instead of nothing. Every read
 * surface (grep, search both lanes, list, retrieve) calls this one function, so
 * all of them widened on the trunk: measured on this repo, a read on `main`
 * pulled in an abandoned `fix/...` branch holding 1952 files, re-serving pre-edit
 * content generations and inflating grep's `total_matches` from 2033 to 4033.
 */

import { describe, it, expect, beforeEach, afterEach, vi } from 'vitest';
import Database, { type Database as DatabaseType } from 'better-sqlite3';

const gitState = vi.hoisted(() => ({ defaultBranch: null as string | null }));

vi.mock('../../src/utils/git-utils.js', async (importOriginal) => {
  const actual = await importOriginal<typeof import('../../src/utils/git-utils.js')>();
  return { ...actual, getDefaultBranch: vi.fn(() => gitState.defaultBranch) };
});

const { getBaseBranch } = await import('../../src/clients/tracked-files-queries/index.js');

const SCHEMA = `
CREATE TABLE watch_folders (
    watch_id TEXT PRIMARY KEY,
    path TEXT NOT NULL UNIQUE,
    collection TEXT NOT NULL,
    tenant_id TEXT NOT NULL,
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL
);
CREATE TABLE tracked_files (
    file_id INTEGER PRIMARY KEY AUTOINCREMENT,
    watch_folder_id TEXT NOT NULL,
    branches TEXT NOT NULL DEFAULT '[]',
    relative_path TEXT NOT NULL,
    language TEXT,
    is_test INTEGER DEFAULT 0
);
`;

const WATCH = 'watch-a';
let db: DatabaseType;

/** Add `count` files, each tagged with every branch in `branches`. */
function addFiles(count: number, branches: string[], prefix = 'f'): void {
  const insert = db.prepare(
    'INSERT INTO tracked_files (watch_folder_id, branches, relative_path) VALUES (?, ?, ?)'
  );
  for (let i = 0; i < count; i++) {
    insert.run(WATCH, JSON.stringify(branches), `src/${prefix}${i}.ts`);
  }
}

beforeEach(() => {
  gitState.defaultBranch = null;
  db = new Database(':memory:');
  db.exec(SCHEMA);
  db.prepare(
    'INSERT INTO watch_folders (watch_id, path, collection, tenant_id, created_at, updated_at)' +
      " VALUES (?, '/repo', 'projects', 'tenant-a', '', '')"
  ).run(WATCH);
});

afterEach(() => {
  db.close();
});

describe('getBaseBranch — trunk detection', () => {
  it('returns null on the trunk, so a read there does not widen at all', () => {
    addFiles(100, ['main']);
    addFiles(80, ['feature/x']);

    expect(getBaseBranch(db, WATCH, 'main')).toBeNull();
  });

  it('returns the trunk from a feature branch, so unchanged files stay visible', () => {
    addFiles(100, ['main']);
    addFiles(80, ['feature/x']);

    expect(getBaseBranch(db, WATCH, 'feature/x')).toBe('main');
  });

  it('does NOT widen into an abandoned sibling branch that outnumbers nothing', () => {
    // The exact shape measured in production: the trunk leads, a merged feature
    // branch is a close second. A read on the trunk must ignore the sibling.
    addFiles(1971, ['main']);
    addFiles(1952, ['fix/global-ignore-src-tree-reinclusion']);

    expect(getBaseBranch(db, WATCH, 'main')).toBeNull();
  });

  it('prefers git default branch over the most-tracked branch when the index has it', () => {
    // A long-lived feature branch can out-track the trunk (measured: 98.7% vs
    // 97.3%). Git knows which one is the trunk; the index does not.
    addFiles(100, ['feature/long-lived']);
    addFiles(97, ['main']);
    gitState.defaultBranch = 'main';

    expect(getBaseBranch(db, WATCH, 'feature/long-lived')).toBe('main');
    expect(getBaseBranch(db, WATCH, 'main')).toBeNull();
  });

  it('ignores a git default branch the index never tagged', () => {
    // The write path defaults a branch tag to "main", so a repo whose git
    // default is "master" can hold every file under "main". Pointing the
    // fallback at "master" would target an empty branch AND tell a caller on
    // "master" it was already on the trunk.
    addFiles(100, ['main']);
    gitState.defaultBranch = 'master';

    expect(getBaseBranch(db, WATCH, 'master')).toBe('main');
    expect(getBaseBranch(db, WATCH, 'main')).toBeNull();
  });

  it('prefers the caller branch on an exact tie, which means no widening', () => {
    addFiles(50, ['main']);
    addFiles(50, ['feature/x']);

    expect(getBaseBranch(db, WATCH, 'feature/x')).toBeNull();
    expect(getBaseBranch(db, WATCH, 'main')).toBeNull();
  });

  it('counts a file tagged on several branches toward each of them', () => {
    addFiles(60, ['main', 'feature/x'], 'shared');
    addFiles(10, ['main'], 'only-main');

    expect(getBaseBranch(db, WATCH, 'feature/x')).toBe('main');
    expect(getBaseBranch(db, WATCH, 'main')).toBeNull();
  });

  it('returns null when the project has no tracked files', () => {
    expect(getBaseBranch(db, WATCH, 'main')).toBeNull();
  });

  it('returns null for an unknown watch folder rather than guessing', () => {
    addFiles(100, ['main']);

    expect(getBaseBranch(db, 'watch-missing', 'anything')).toBeNull();
  });

  it('returns null without a database handle', () => {
    expect(getBaseBranch(null, WATCH, 'main')).toBeNull();
  });
});
