/**
 * Scratchpad provenance when the client cwd is inside a linked worktree.
 *
 * Reproduced 2026-09-05: a `store` from `.claude/worktrees/<name>` (UNC path,
 * Windows client) was stamped `origin_branch = <main checkout's branch>` and
 * `origin_worktree: false`, while `search`/`grep` on the same cwd emitted their
 * worktree note. The session branch and the registered project path both
 * describe the MAIN checkout; the worktree's own branch lives in the main
 * repo's `.git/worktrees/<id>/HEAD`.
 */
import { describe, it, expect, vi, beforeEach, afterEach } from 'vitest';
import { mkdtempSync, mkdirSync, writeFileSync, rmSync } from 'node:fs';
import { join } from 'node:path';
import { tmpdir } from 'node:os';
import { storeScratchpad } from '../src/store-handlers.js';
import type { SqliteStateManager } from '../src/clients/sqlite-state-manager.js';
import type { ProjectDetector } from '../src/utils/project-detector.js';
import { runWithRequestContext } from '../src/utils/request-context.js';
import { readLinkedWorktreeBranch } from '../src/utils/git-utils.js';

function mockStateManager(): SqliteStateManager {
  return {
    enqueueUnified: vi.fn().mockResolvedValue({ status: 'ok', data: { queueId: 'q-1' } }),
    upsertScratchpadMirror: vi.fn(),
  } as unknown as SqliteStateManager;
}

/** The queue payload is the 5th positional arg of enqueueUnified. */
function payloadOf(sm: SqliteStateManager): Record<string, unknown> {
  return (sm.enqueueUnified as unknown as ReturnType<typeof vi.fn>).mock.calls[0][4] as Record<
    string,
    unknown
  >;
}

function detectorFor(repo: string): ProjectDetector {
  return {
    getProjectInfo: vi.fn().mockResolvedValue({ projectId: 'proj-1', projectPath: repo }),
  } as unknown as ProjectDetector;
}

// A Windows client's UNC cwd inside the worktree — the server (in a container)
// cannot open this path; only its SHAPE is usable.
const HOST_CWD = '\\\\wsl.localhost\\ubuntu\\home\\u\\repo\\.claude\\worktrees\\feat\\src';

let repo: string;

beforeEach(() => {
  repo = mkdtempSync(join(tmpdir(), 'wqm-wt-origin-'));
  // Main checkout metadata for a linked worktree whose git id ("feat-1a2b")
  // differs from the directory name ("feat") — git disambiguates collisions.
  mkdirSync(join(repo, '.git', 'worktrees', 'feat-1a2b'), { recursive: true });
  writeFileSync(
    join(repo, '.git', 'worktrees', 'feat-1a2b', 'HEAD'),
    'ref: refs/heads/claude/feature-x\n'
  );
  mkdirSync(join(repo, '.claude', 'worktrees', 'feat'), { recursive: true });
  writeFileSync(
    join(repo, '.claude', 'worktrees', 'feat', '.git'),
    'gitdir: //wsl.localhost/ubuntu/home/u/repo/.git/worktrees/feat-1a2b\n'
  );
});

afterEach(() => {
  rmSync(repo, { recursive: true, force: true });
});

describe('readLinkedWorktreeBranch', () => {
  it('follows the gitlink to the recorded worktree id and reads its HEAD', () => {
    expect(readLinkedWorktreeBranch(repo, 'feat')).toBe('claude/feature-x');
  });

  it('falls back to the directory name when there is no gitlink', () => {
    mkdirSync(join(repo, '.git', 'worktrees', 'other'), { recursive: true });
    writeFileSync(
      join(repo, '.git', 'worktrees', 'other', 'HEAD'),
      'ref: refs/heads/other-branch\n'
    );
    expect(readLinkedWorktreeBranch(repo, 'other')).toBe('other-branch');
  });

  it('returns null for a detached HEAD or an unknown worktree', () => {
    writeFileSync(join(repo, '.git', 'worktrees', 'feat-1a2b', 'HEAD'), 'abc123def\n');
    expect(readLinkedWorktreeBranch(repo, 'feat')).toBeNull();
    expect(readLinkedWorktreeBranch(repo, 'nope')).toBeNull();
  });
});

describe('scratchpad provenance from a worktree cwd', () => {
  it('stamps the worktree branch and origin_worktree=true, not the session (main checkout) branch', async () => {
    const sm = mockStateManager();
    await runWithRequestContext({ hostCwd: HOST_CWD, cwdSource: 'body' }, () =>
      storeScratchpad({ content: 'note from a worktree' }, sm, detectorFor(repo), {
        projectId: 'proj-1',
        currentBranch: 'fix/other-work',
        isWorktree: false,
      })
    );
    const payload = payloadOf(sm);
    expect(payload['origin_worktree']).toBe(true);
    expect(payload['origin_branch']).toBe('claude/feature-x');
    expect(payload['origin_cwd']).toBe(HOST_CWD);
  });

  it('omits origin_branch rather than borrowing the main checkout branch when the worktree HEAD is unreadable', async () => {
    rmSync(join(repo, '.git', 'worktrees', 'feat-1a2b'), { recursive: true, force: true });
    const sm = mockStateManager();
    await runWithRequestContext({ hostCwd: HOST_CWD, cwdSource: 'body' }, () =>
      storeScratchpad({ content: 'note from a worktree' }, sm, detectorFor(repo), {
        projectId: 'proj-1',
        currentBranch: 'fix/other-work',
        isWorktree: false,
      })
    );
    const payload = payloadOf(sm);
    expect(payload['origin_worktree']).toBe(true);
    expect(payload['origin_branch']).toBeUndefined();
  });

  it('honours an explicit branch override from a worktree', async () => {
    const sm = mockStateManager();
    await runWithRequestContext({ hostCwd: HOST_CWD, cwdSource: 'body' }, () =>
      storeScratchpad({ content: 'x', branch: 'explicit/one' }, sm, detectorFor(repo), {
        projectId: 'proj-1',
        currentBranch: 'fix/other-work',
        isWorktree: false,
      })
    );
    const payload = payloadOf(sm);
    expect(payload['origin_branch']).toBe('explicit/one');
    expect(payload['origin_worktree']).toBe(true);
  });

  it('keeps the session branch for a plain (non-worktree) cwd', async () => {
    const sm = mockStateManager();
    await runWithRequestContext({ hostCwd: '/home/u/repo', cwdSource: 'body' }, () =>
      storeScratchpad({ content: 'x' }, sm, detectorFor(repo), {
        projectId: 'proj-1',
        currentBranch: 'main',
        isWorktree: false,
      })
    );
    const payload = payloadOf(sm);
    expect(payload['origin_branch']).toBe('main');
    expect(payload['origin_worktree']).toBe(false);
  });
});
