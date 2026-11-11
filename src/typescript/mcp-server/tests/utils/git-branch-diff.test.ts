/**
 * The changed-path set between the trunk and branch tips: the git half of the
 * fallback guard. Ref resolution prefers a local branch over `origin/<name>`,
 * matches the names `for-each-ref` returns exactly (its patterns are prefixes),
 * and answers `null` — never an empty set — when git cannot say, so the caller
 * can tell "nothing changed" from "unknown". Every call is async (a synchronous
 * spawn blocked the MCP event loop) and paths are relative to the project root.
 */

import { describe, it, expect, vi, beforeEach } from 'vitest';
import {
  clearBranchDiffCache,
  countFilesAtBranchTipWith,
  getPathsChangedBetweenWith,
  type GitRunner,
} from '../../src/utils/git-branch-diff.js';

function runner(
  refs: string[],
  diff: string | null,
  lsTree: string | null = null
): ReturnType<typeof vi.fn> & GitRunner {
  return vi.fn().mockImplementation(async (_root: string, args: readonly string[]) => {
    if (args[0] === 'for-each-ref') return refs.join('\n') + '\n';
    if (args[0] === 'diff') return diff;
    if (args[0] === 'ls-tree') return lsTree;
    return null;
  }) as ReturnType<typeof vi.fn> & GitRunner;
}

const NOW = () => 1_000;

describe('getPathsChangedBetween', () => {
  beforeEach(() => clearBranchDiffCache());

  it('returns the NUL-separated paths of a root-relative `git diff --name-only -z`', async () => {
    const run = runner(
      ['refs/heads/develop', 'refs/heads/feat/x'],
      'app/lib/actions/endpoint.ts\0app/(user)/message/page.tsx\0'
    );
    const out = await getPathsChangedBetweenWith('/repo', 'develop', 'feat/x', run, NOW);
    expect([...(out ?? [])].sort()).toEqual([
      'app/(user)/message/page.tsx',
      'app/lib/actions/endpoint.ts',
    ]);
    expect(run).toHaveBeenLastCalledWith('/repo', [
      'diff',
      '--relative',
      '--no-renames',
      '--name-only',
      '-z',
      'refs/heads/develop',
      'refs/heads/feat/x',
      '--',
    ]);
  });

  it('prefers the local branch and falls back to origin/<name>', async () => {
    const run = runner(['refs/remotes/origin/develop', 'refs/heads/feat/x'], '');
    await getPathsChangedBetweenWith('/repo', 'develop', 'feat/x', run, NOW);
    const diffArgs = run.mock.calls.at(-1)?.[1] as string[];
    expect(diffArgs).toContain('refs/remotes/origin/develop');
    expect(diffArgs).toContain('refs/heads/feat/x');
  });

  it('matches ref names exactly, not by prefix', async () => {
    // `refs/heads/main/x` is NOT `refs/heads/main`.
    const run = runner(['refs/heads/main/x', 'refs/heads/feat'], '');
    expect(await getPathsChangedBetweenWith('/repo', 'main', 'feat', run, NOW)).toBeNull();
  });

  it('is null when a side has no ref, or git fails', async () => {
    expect(
      await getPathsChangedBetweenWith(
        '/repo',
        'develop',
        'gone',
        runner(['refs/heads/develop'], ''),
        NOW
      )
    ).toBeNull();
    clearBranchDiffCache();
    expect(
      await getPathsChangedBetweenWith(
        '/repo',
        'develop',
        'feat',
        runner(['refs/heads/develop', 'refs/heads/feat'], null),
        NOW
      )
    ).toBeNull();
  });

  it('is an EMPTY set (not null) when the tips agree', async () => {
    const run = runner(['refs/heads/develop', 'refs/heads/feat'], '');
    expect((await getPathsChangedBetweenWith('/repo', 'develop', 'feat', run, NOW))?.size).toBe(0);
  });

  it('reuses an answer for 30 s, then asks git again', async () => {
    const run = runner(['refs/heads/develop', 'refs/heads/feat'], 'a.ts\0');
    let t = 0;
    const now = () => t;
    await getPathsChangedBetweenWith('/repo', 'develop', 'feat', run, now);
    t = 29_999;
    await getPathsChangedBetweenWith('/repo', 'develop', 'feat', run, now);
    expect(run).toHaveBeenCalledTimes(2); // for-each-ref + diff, once
    t = 30_001;
    await getPathsChangedBetweenWith('/repo', 'develop', 'feat', run, now);
    expect(run).toHaveBeenCalledTimes(4);
  });

  it('shares one git spawn between concurrent misses for the same key', async () => {
    const run = runner(['refs/heads/develop', 'refs/heads/feat'], 'a.ts\0');
    await Promise.all([
      getPathsChangedBetweenWith('/repo', 'develop', 'feat', run, NOW),
      getPathsChangedBetweenWith('/repo', 'develop', 'feat', run, NOW),
    ]);
    expect(run).toHaveBeenCalledTimes(2);
  });
});

/**
 * The trunk resolves as the daemon indexes it (`git::followed_trunk`): tecsul's
 * local `release/platform` sat 1,406 commits behind origin (2026-11-11), and a
 * diff against it reported every file origin had changed as the branch's own.
 */
describe('the trunk at its followed tip', () => {
  beforeEach(() => clearBranchDiffCache());

  const TRUNK_REFS = ['refs/heads/main', 'refs/remotes/origin/main', 'refs/heads/feat'];

  /** A repo whose main folder has `head` checked out; `fastForward`: origin/main contains main. */
  function trunkRunner(head: string, fastForward: boolean): ReturnType<typeof vi.fn> & GitRunner {
    return vi.fn().mockImplementation(async (_root: string, args: readonly string[]) => {
      if (args[0] === 'for-each-ref') return TRUNK_REFS.join('\n') + '\n';
      if (args[0] === 'symbolic-ref') return `${head}\n`;
      if (args[0] === 'merge-base') return fastForward ? '' : null;
      if (args[0] === 'diff') return '';
      if (args[0] === 'ls-tree') return 'a.ts\0';
      return null;
    }) as ReturnType<typeof vi.fn> & GitRunner;
  }

  const trunkIsMain = () => 'main';
  const diffArgs = (run: ReturnType<typeof vi.fn>) =>
    run.mock.calls.find((c) => (c[1] as string[])[0] === 'diff')?.[1] as string[];

  it('diffs against origin/<trunk> when it fast-forwards the local trunk', async () => {
    const run = trunkRunner('refs/heads/develop', true);
    await getPathsChangedBetweenWith('/repo', 'main', 'feat', run, NOW, trunkIsMain);
    expect(diffArgs(run)).toContain('refs/remotes/origin/main');
    expect(run).toHaveBeenCalledWith('/repo', [
      'merge-base',
      '--is-ancestor',
      'refs/heads/main',
      'refs/remotes/origin/main',
    ]);
  });

  it('keeps the local trunk when the main folder has it, or it has commits of its own', async () => {
    const onTrunk = trunkRunner('refs/heads/main', true);
    await getPathsChangedBetweenWith('/repo', 'main', 'feat', onTrunk, NOW, trunkIsMain);
    expect(diffArgs(onTrunk)).toContain('refs/heads/main');
    clearBranchDiffCache();
    const diverged = trunkRunner('refs/heads/develop', false);
    await getPathsChangedBetweenWith('/repo', 'main', 'feat', diverged, NOW, trunkIsMain);
    expect(diffArgs(diverged)).toContain('refs/heads/main');
  });

  it('leaves every other branch at its local ref', async () => {
    const run = trunkRunner('refs/heads/develop', true);
    await getPathsChangedBetweenWith('/repo', 'main', 'feat', run, NOW, () => 'develop');
    expect(diffArgs(run)).toContain('refs/heads/main');
    expect(run).not.toHaveBeenCalledWith('/repo', expect.arrayContaining(['merge-base']));
  });

  it('counts the files of the followed trunk tip', async () => {
    const run = trunkRunner('refs/heads/develop', true);
    expect(await countFilesAtBranchTipWith('/repo', 'main', run, NOW, trunkIsMain)).toBe(1);
    expect(run).toHaveBeenLastCalledWith('/repo', [
      'ls-tree',
      '-r',
      '--name-only',
      '-z',
      'refs/remotes/origin/main',
      '--',
      '.',
    ]);
  });
});

describe('countFilesAtBranchTip', () => {
  beforeEach(() => clearBranchDiffCache());

  it('counts the tip files under the project root only', async () => {
    const run = runner(['refs/heads/feat'], null, 'a.ts\0b/c.ts\0');
    expect(await countFilesAtBranchTipWith('/repo/apps/web', 'feat', run, NOW)).toBe(2);
    expect(run).toHaveBeenLastCalledWith('/repo/apps/web', [
      'ls-tree',
      '-r',
      '--name-only',
      '-z',
      'refs/heads/feat',
      '--',
      '.',
    ]);
  });
});
