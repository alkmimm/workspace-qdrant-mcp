/**
 * Per-branch views of tracked_files: which paths a branch holds (the index half
 * of the fallback guard) and how many distinct files each branch has (the
 * coverage report).
 */

import { describe, it, expect, beforeEach, afterEach } from 'vitest';
import Database, { type Database as DatabaseType } from 'better-sqlite3';

import {
  getPathsTrackedOnBranch,
  getTrackedFileCountsByBranch,
} from '../../src/clients/tracked-files-queries/index.js';

let db: DatabaseType;

function seed(path: string, branches: string[], hash: string, watch = 'wf'): void {
  db.prepare(
    'INSERT INTO tracked_files (watch_folder_id, relative_path, branches, file_hash) VALUES (?, ?, ?, ?)'
  ).run(watch, path, JSON.stringify(branches), hash);
}

beforeEach(() => {
  db = new Database(':memory:');
  db.exec(`CREATE TABLE tracked_files (
    file_id INTEGER PRIMARY KEY AUTOINCREMENT,
    watch_folder_id TEXT NOT NULL,
    relative_path TEXT,
    branches TEXT NOT NULL DEFAULT '[]',
    file_hash TEXT NOT NULL
  )`);
  // Two generations of page.tsx; a shared util; a branch-only file; another tenant.
  seed('app/page.tsx', ['develop'], 'h1');
  seed('app/page.tsx', ['feat'], 'h2');
  seed('app/util.ts', ['develop', 'feat'], 'h3');
  seed('src/new.ts', ['feat'], 'h4');
  seed('app/page.tsx', ['feat'], 'h9', 'other-wf');
});

afterEach(() => db.close());

describe('getPathsTrackedOnBranch', () => {
  it('answers which candidate paths the branch holds, any generation', () => {
    const held = getPathsTrackedOnBranch(db, 'wf', 'feat', [
      'app/page.tsx',
      'app/util.ts',
      'app/gone.ts',
    ]);
    expect([...held].sort()).toEqual(['app/page.tsx', 'app/util.ts']);
  });

  it('is scoped to the watch folder', () => {
    expect(getPathsTrackedOnBranch(db, 'other-wf', 'develop', ['app/page.tsx']).size).toBe(0);
  });

  it('chunks long candidate lists under the bound-parameter limit', () => {
    const many = Array.from({ length: 1500 }, (_, i) => `f${i}.ts`).concat(['src/new.ts']);
    expect([...getPathsTrackedOnBranch(db, 'wf', 'feat', many)]).toEqual(['src/new.ts']);
  });

  it('is empty (not a throw) without a database', () => {
    expect(getPathsTrackedOnBranch(null, 'wf', 'feat', ['a']).size).toBe(0);
  });
});

describe('getTrackedFileCountsByBranch', () => {
  it('counts distinct paths per branch, largest first', () => {
    expect(getTrackedFileCountsByBranch(db, 'wf')).toEqual([
      { branch: 'feat', files: 3 },
      { branch: 'develop', files: 2 },
    ]);
  });
});
