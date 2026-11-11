/**
 * A cwd in ANY linked worktree resolves its project and its branch, read from
 * the main checkout's `.git/worktrees` (field feedback 2026-11-11, tecsul: 12
 * worktrees outside the repository needed `projectId` + `branch` on every call,
 * and a worktree cwd read the MAIN checkout's branch).
 */

import { describe, it, expect, beforeEach, afterEach } from 'vitest';
import Database from 'better-sqlite3';
import { mkdtempSync, mkdirSync, rmSync, writeFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';

import {
  clearLinkedWorktreeCache,
  linkedWorktreeOf,
  listLinkedWorktrees,
} from '../../src/utils/linked-worktrees.js';
import { resolveEffectiveBranch } from '../../src/tools/branch-scope.js';
import { runWithRequestContext } from '../../src/utils/request-context.js';
import { ProjectDetector } from '../../src/utils/project-detector.js';
import { SqliteStateManager } from '../../src/clients/sqlite-state-manager.js';

/** Record a linked worktree in `<main>/.git/worktrees/<id>`, as git does. */
function addWorktreeAdmin(main: string, id: string, root: string, head: string): void {
  const admin = join(main, '.git', 'worktrees', id);
  mkdirSync(admin, { recursive: true });
  writeFileSync(join(admin, 'gitdir'), `${root}/.git\n`);
  writeFileSync(join(admin, 'HEAD'), `${head}\n`);
}

describe('linked worktrees of a main checkout', () => {
  let tmp: string;
  let main: string;

  beforeEach(() => {
    clearLinkedWorktreeCache();
    tmp = mkdtempSync(join(tmpdir(), 'linked-wt-'));
    main = join(tmp, 'repo');
    mkdirSync(join(main, '.git'), { recursive: true });
    // Outside the repository, like ~/.codex/worktrees/<n>/<repo>.
    addWorktreeAdmin(main, 'agent', '/home/u/.codex/worktrees/42/repo', 'ref: refs/heads/agent/x');
    // Inside it, the /batch layout.
    addWorktreeAdmin(
      main,
      'inner',
      `${main}/.claude/worktrees/inner`,
      'ref: refs/heads/feat/inner'
    );
    addWorktreeAdmin(main, 'detached', '/elsewhere/detached', '0123456789abcdef');
    // A half-created admin entry (no HEAD yet) is skipped.
    mkdirSync(join(main, '.git', 'worktrees', 'broken'), { recursive: true });
    writeFileSync(join(main, '.git', 'worktrees', 'broken', 'gitdir'), '/x/.git\n');
  });

  afterEach(() => rmSync(tmp, { recursive: true, force: true }));

  it('lists every recorded worktree with its branch (null when detached)', () => {
    const byRoot = Object.fromEntries(listLinkedWorktrees(main).map((w) => [w.root, w.branch]));
    expect(byRoot).toEqual({
      '/home/u/.codex/worktrees/42/repo': 'agent/x',
      [`${main}/.claude/worktrees/inner`]: 'feat/inner',
      '/elsewhere/detached': null,
    });
  });

  it('finds the worktree containing a cwd, on a path boundary', () => {
    expect(linkedWorktreeOf('/home/u/.codex/worktrees/42/repo/src/a.ts', main)?.branch).toBe(
      'agent/x'
    );
    expect(linkedWorktreeOf('/home/u/.codex/worktrees/42/repo', main)?.branch).toBe('agent/x');
    expect(linkedWorktreeOf('/home/u/.codex/worktrees/42/repo-other', main)).toBeUndefined();
    expect(linkedWorktreeOf(main, main)).toBeUndefined();
  });

  it('a read from a worktree defaults to the worktree branch, not the main checkout', () => {
    const branchAt = (cwd: string) =>
      runWithRequestContext({ hostCwd: cwd }, () =>
        resolveEffectiveBranch({
          explicitBranch: undefined,
          scope: 'project',
          projectId: 't1',
          projectPath: main,
        })
      );
    expect(branchAt('/home/u/.codex/worktrees/42/repo/plataforma')).toBe('agent/x');
    expect(branchAt(`${main}/.claude/worktrees/inner/lib`)).toBe('feat/inner');
    // An explicit branch still wins.
    expect(
      runWithRequestContext({ hostCwd: '/home/u/.codex/worktrees/42/repo' }, () =>
        resolveEffectiveBranch({
          explicitBranch: 'release/platform',
          scope: 'project',
          projectId: 't1',
          projectPath: main,
        })
      )
    ).toBe('release/platform');
  });

  it('detects the project of a cwd in a worktree outside the repository', async () => {
    const dbPath = join(tmp, 'state.db');
    const db = new Database(dbPath);
    db.exec(`CREATE TABLE watch_folders (
      watch_id TEXT PRIMARY KEY, path TEXT NOT NULL UNIQUE, collection TEXT NOT NULL,
      tenant_id TEXT NOT NULL, parent_watch_id TEXT, submodule_path TEXT, git_remote_url TEXT,
      remote_hash TEXT, disambiguation_path TEXT, is_active INTEGER DEFAULT 0,
      last_activity_at TEXT, library_mode TEXT, follow_symlinks INTEGER DEFAULT 0,
      enabled INTEGER DEFAULT 1, cleanup_on_disable INTEGER DEFAULT 0,
      created_at TEXT NOT NULL, updated_at TEXT NOT NULL, last_scan TEXT)`);
    const insert = db.prepare(
      `INSERT INTO watch_folders (watch_id, path, collection, tenant_id, is_active, created_at, updated_at)
       VALUES (?, ?, 'projects', ?, 1, datetime('now'), datetime('now'))`
    );
    insert.run('w1', main, 'tenant-repo');
    insert.run('w2', join(tmp, 'other'), 'tenant-other');
    db.close();
    const stateManager = new SqliteStateManager({ dbPath });
    stateManager.initialize();
    try {
      const detector = new ProjectDetector({ stateManager });
      const info = await detector.getProjectInfo('/home/u/.codex/worktrees/42/repo/src');
      expect(info?.projectId).toBe('tenant-repo');
      expect(info?.projectPath).toBe(main);
      expect(await detector.getProjectInfo('/home/u/unrelated')).toBeNull();
    } finally {
      stateManager.close();
    }
  });
});
