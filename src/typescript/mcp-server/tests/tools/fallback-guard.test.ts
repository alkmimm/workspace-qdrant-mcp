/**
 * The trunk fill-in of a branch-scoped read admits a path only when the trunk's
 * copy IS the branch's copy: not changed between the tips (git), and not held
 * by the branch in the index.
 *
 * #408 admitted any trunk entry whose path had no HIT in the scoped result —
 * so on emnify-sms-sender (2026-10-03) a grep scoped to the phase-5 branch
 * returned `app/lib/actions/endpoint.ts` and `Message.ts`, which that branch
 * had deleted, and the trunk's `message/page.tsx` line 1 `'use server'` where
 * the branch's file starts with an import.
 */

import { describe, it, expect, vi } from 'vitest';
import {
  FallbackGuard,
  admitFallbackEntries,
  createFallbackGuard,
  guardWidened,
} from '../../src/tools/fallback-guard.js';

interface Hit {
  path?: string;
  branch?: unknown;
  tag: string;
}

const pathOf = (h: Hit) => h.path;
const branchOf = (h: Hit) => h.branch;

function guard(opts: {
  changed?: string[] | null;
  tracked?: string[];
  root?: string;
}): { g: FallbackGuard; trackedOnBranch: ReturnType<typeof vi.fn> } {
  const trackedOnBranch = vi
    .fn()
    .mockImplementation((paths: readonly string[]) =>
      new Set(paths.filter((p) => (opts.tracked ?? []).includes(p)))
    );
  const g = new FallbackGuard('feat/fase-5', 'develop', opts.root ?? '/repo', {
    trackedOnBranch,
    changedBetweenTips: async () => (opts.changed === null ? null : new Set(opts.changed ?? [])),
  });
  return { g, trackedOnBranch };
}

describe('FallbackGuard.admit', () => {
  it('refuses a path the branch deleted (git reports it changed)', async () => {
    const { g } = guard({ changed: ['app/lib/actions/endpoint.ts'] });
    const out = await g.admit(
      [
        { path: '/repo/app/lib/actions/endpoint.ts', tag: 'deleted-on-branch' },
        { path: '/repo/app/lib/util.ts', tag: 'unchanged' },
      ],
      pathOf
    );
    expect(out.map((h) => h.tag)).toEqual(['unchanged']);
  });

  it('refuses a path the branch holds even when its own copy did not match', async () => {
    // message/page.tsx: the branch's generation starts with an import, so the
    // scoped query had no hit for it; the trunk's starts with 'use server'.
    const { g } = guard({ changed: [], tracked: ['app/(user)/message/page.tsx'] });
    const out = await g.admit(
      [{ path: '/repo/app/(user)/message/page.tsx', tag: 'trunk-version' }],
      pathOf
    );
    expect(out).toEqual([]);
  });

  it('admits a path unchanged between the tips and not held by the branch', async () => {
    const { g } = guard({ changed: ['other.ts'], tracked: [] });
    const out = await g.admit([{ path: '/repo/src/shared.ts', tag: 'fill' }], pathOf);
    expect(out.map((h) => h.tag)).toEqual(['fill']);
  });

  it('applies only the index rule when git cannot answer', async () => {
    const { g } = guard({ changed: null, tracked: ['held.ts'] });
    const out = await g.admit(
      [
        { path: '/repo/held.ts', tag: 'held' },
        { path: '/repo/free.ts', tag: 'free' },
      ],
      pathOf
    );
    expect(out.map((h) => h.tag)).toEqual(['free']);
  });

  it('keeps an entry whose path cannot be read rather than losing it', async () => {
    const { g } = guard({ changed: ['a.ts'] });
    expect((await g.admit([{ tag: 'no-path' }], pathOf)).map((h) => h.tag)).toEqual(['no-path']);
  });

  it('accepts repo-relative paths as well as main-anchored absolute ones', async () => {
    const { g } = guard({ changed: ['src/a.ts'] });
    const out = await g.admit(
      [
        { path: 'src/a.ts', tag: 'relative-changed' },
        { path: '/repo/src/a.ts', tag: 'absolute-changed' },
        { path: 'src/b.ts', tag: 'relative-ok' },
      ],
      pathOf
    );
    expect(out.map((h) => h.tag)).toEqual(['relative-ok']);
  });

  it('asks the index only about paths git did not already refuse', async () => {
    const { g, trackedOnBranch } = guard({ changed: ['gone.ts'], tracked: [] });
    await g.admit(
      [
        { path: '/repo/gone.ts', tag: 'x' },
        { path: '/repo/keep.ts', tag: 'y' },
      ],
      pathOf
    );
    expect(trackedOnBranch).toHaveBeenCalledWith(['keep.ts']);
  });

  it('exposes the refused set for SQL (list)', async () => {
    const { g } = guard({ changed: ['a.ts', 'b/c.ts'] });
    expect((await g.refusedPaths()).sort()).toEqual(['a.ts', 'b/c.ts']);
    expect(await guard({ changed: null }).g.refusedPaths()).toEqual([]);
  });
});

describe('admitFallbackEntries (one query, both branches)', () => {
  it('guards only the entries the caller branch does not carry', async () => {
    const { g } = guard({ changed: ['app/lib/actions/endpoint.ts'] });
    const hits: Hit[] = [
      { path: '/repo/app/lib/actions/endpoint.ts', branch: ['develop'], tag: 'trunk-deleted' },
      { path: '/repo/app/lib/actions/endpoint.ts', branch: ['feat/fase-5'], tag: 'own' },
      { path: '/repo/src/shared.ts', branch: 'develop,feat/fase-5', tag: 'shared' },
      { path: '/repo/src/fill.ts', branch: ['develop'], tag: 'trunk-fill' },
      { path: '/repo/src/unknown.ts', tag: 'no-branch' },
    ];
    expect((await admitFallbackEntries(hits, g, pathOf, branchOf)).map((h) => h.tag)).toEqual([
      'own',
      'shared',
      'trunk-fill',
      'no-branch',
    ]);
  });

  it('is a no-op without a guard', async () => {
    const hits: Hit[] = [{ path: 'a', branch: ['develop'], tag: 'x' }];
    expect(await admitFallbackEntries(hits, undefined, pathOf, branchOf)).toEqual(hits);
  });
});

describe('createFallbackGuard', () => {
  const base = {
    stateManager: null,
    watchFolderId: 'wf',
    projectRoot: '/repo',
  };

  it('is undefined when no fallback is in play', async () => {
    expect(createFallbackGuard({ ...base, branch: 'feat', fallbackBranch: undefined })).toBeUndefined();
    expect(createFallbackGuard({ ...base, branch: '*', fallbackBranch: 'main' })).toBeUndefined();
    expect(createFallbackGuard({ ...base, branch: 'main', fallbackBranch: 'main' })).toBeUndefined();
    expect(createFallbackGuard({ ...base, branch: undefined, fallbackBranch: 'main' })).toBeUndefined();
  });

  it('degrades to "branch holds nothing" when the index lookup is missing or throws', async () => {
    const throwing = {
      getPathsTrackedOnBranch: () => {
        throw new Error('db gone');
      },
    };
    const g = createFallbackGuard({
      ...base,
      projectRoot: undefined, // no git either: both rules unavailable
      stateManager: throwing,
      branch: 'feat',
      fallbackBranch: 'main',
    });
    expect((await g?.admit([{ path: 'a.ts', tag: 'x' }], pathOf))?.map((h) => h.tag)).toEqual(['x']);
  });
});

describe('guardWidened (empty-result auto-widen to branch "*")', () => {
  it('withholds other branches\' copies of files the branch deleted or changed', async () => {
    const { g } = guard({ changed: ['app/lib/actions/endpoint.ts'] });
    const widened: Hit[] = [
      { path: '/repo/app/lib/actions/endpoint.ts', tag: 'deleted-on-branch' },
      { path: '/repo/src/only-elsewhere.ts', tag: 'unchanged' },
    ];
    const out = await guardWidened(widened, g, pathOf);
    expect(out.kept.map((h) => h.tag)).toEqual(['unchanged']);
    expect(out.withheld).toBe(1);
  });

  it('keeps everything when no fallback (and so no guard) is in play', async () => {
    const widened: Hit[] = [{ path: '/repo/a.ts', tag: 'x' }];
    expect(await guardWidened(widened, undefined, pathOf)).toEqual({ kept: widened, withheld: 0 });
  });
});
