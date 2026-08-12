/**
 * Read surfaces drop other-worktree paths by default. `.claude/worktrees/<wt>/…`
 * is a leaked/stale generation (worktree content is stored main-anchored) that
 * points into the WRONG checkout — reading/editing it touches another worktree
 * (field feedback 2026-08-10, DOC-V2). The drop is gated: a caller whose own cwd
 * lives inside a worktree keeps those paths (they are legitimately its own).
 */

import { describe, it, expect } from 'vitest';
import { isWorktreeSubtreePath } from '../../src/utils/path-glob.js';
import { filterGrepMatchesByExclude, type GrepMatch } from '../../src/tools/grep.js';
import { filterResultsByPathExclude } from '../../src/tools/search-path-filters.js';
import { runWithRequestContext } from '../../src/utils/request-context.js';
import type { SearchResult } from '../../src/tools/search-types.js';

const gm = (file: string): GrepMatch => ({
  file,
  line: 1,
  content: 'x',
  context_before: [],
  context_after: [],
});

const sr = (relative_path: string | undefined): SearchResult => ({
  id: relative_path ?? 'no-path',
  score: 1,
  collection: 'projects',
  content: '',
  metadata: relative_path ? { relative_path } : {},
});

const WT = '.claude/worktrees/pr-client-modal/lib/page.dart';
const MAIN = 'lib/page.dart';

// A non-worktree caller (the main-tenant case the leak reports).
const asMainCaller = <T>(fn: () => T): T => runWithRequestContext({ hostCwd: '/repo/lib' }, fn);
// A caller whose own cwd is inside a worktree checkout.
const asWorktreeCaller = <T>(fn: () => T): T =>
  runWithRequestContext({ hostCwd: '/repo/.claude/worktrees/pr-client-modal/lib' }, fn);

describe('isWorktreeSubtreePath', () => {
  it('matches a .claude/worktrees/<wt>/ path at root, nested, and absolute', () => {
    expect(isWorktreeSubtreePath('.claude/worktrees/foo/a.ts')).toBe(true);
    expect(isWorktreeSubtreePath('repo/.claude/worktrees/foo/a.ts')).toBe(true);
    expect(isWorktreeSubtreePath('/home/u/repo/.claude/worktrees/foo/a.ts')).toBe(true);
  });

  it('does not match normal paths or other .claude/ files', () => {
    expect(isWorktreeSubtreePath('src/a.ts')).toBe(false);
    expect(isWorktreeSubtreePath('.claude/settings.json')).toBe(false);
  });
});

describe('filterGrepMatchesByExclude — other-worktree drop', () => {
  it('drops a worktree match for a main-tenant caller (no pathExclude)', () => {
    const out = asMainCaller(() => filterGrepMatchesByExclude([gm(WT), gm(MAIN)], undefined));
    expect(out.map((m) => m.file)).toEqual([MAIN]);
  });

  it('keeps worktree matches when the caller works inside a worktree', () => {
    const out = asWorktreeCaller(() => filterGrepMatchesByExclude([gm(WT), gm(MAIN)], undefined));
    expect(out.map((m) => m.file)).toEqual([WT, MAIN]);
  });

  it('still honors an explicit pathExclude alongside the worktree drop', () => {
    const out = asMainCaller(() =>
      filterGrepMatchesByExclude([gm(WT), gm(MAIN), gm('old_project/x.ts')], 'old_project/**')
    );
    expect(out.map((m) => m.file)).toEqual([MAIN]);
  });
});

describe('filterResultsByPathExclude — other-worktree drop', () => {
  it('drops a worktree result for a main-tenant caller (no pathExclude)', () => {
    const out = asMainCaller(() => filterResultsByPathExclude([sr(WT), sr(MAIN)], undefined));
    expect(out.map((r) => r.metadata['relative_path'])).toEqual([MAIN]);
  });

  it('keeps worktree results when the caller works inside a worktree', () => {
    const out = asWorktreeCaller(() => filterResultsByPathExclude([sr(WT), sr(MAIN)], undefined));
    expect(out).toHaveLength(2);
  });

  it('keeps a result with no resolvable path (never a silent false-drop)', () => {
    const out = asMainCaller(() => filterResultsByPathExclude([sr(undefined), sr(MAIN)], undefined));
    expect(out).toHaveLength(2);
  });
});

/**
 * The shape the tests above never built, and the reason the drop shipped as a
 * no-op on the semantic lane: a worktree-origin point is indexed with the
 * worktree prefix STRIPPED, so its `relative_path` is byte-identical to the main
 * tree's file and ONLY the absolute path carries `.claude/worktrees/<name>/`.
 * Every case above sets `relative_path` to the worktree path — which the daemon
 * never produces for these points.
 *
 * Measured 2026-08-12 (DOC-V2): `pathExclude: ".claude/worktrees/**"` returned
 * hits from three foreign worktrees. It works in `grep` only because grep
 * matches carry the absolute path alone.
 */
describe('filterResultsByPathExclude — worktree-origin shape (stripped relative)', () => {
  /** relative_path as the daemon stores it (main-anchored) + worktree absolute. */
  const worktreeOrigin = (): SearchResult => ({
    id: 'wt-origin',
    score: 1,
    collection: 'projects',
    content: '',
    metadata: {
      relative_path: MAIN,
      file_path: `/repo/${WT}`,
      absolute_path: `/repo/${WT}`,
    },
  });

  const mainOrigin = (): SearchResult => ({
    id: 'main-origin',
    score: 1,
    collection: 'projects',
    content: '',
    metadata: {
      relative_path: MAIN,
      file_path: `/repo/${MAIN}`,
      absolute_path: `/repo/${MAIN}`,
    },
  });

  it('drops it under an explicit worktree pathExclude', () => {
    const out = asMainCaller(() =>
      filterResultsByPathExclude([worktreeOrigin(), mainOrigin()], '.claude/worktrees/**')
    );
    expect(out.map((r) => r.id)).toEqual(['main-origin']);
  });

  it('drops it by default for a main-tenant caller', () => {
    const out = asMainCaller(() =>
      filterResultsByPathExclude([worktreeOrigin(), mainOrigin()], undefined)
    );
    expect(out.map((r) => r.id)).toEqual(['main-origin']);
  });

  it('keeps it when the caller itself works inside a worktree', () => {
    const out = asWorktreeCaller(() =>
      filterResultsByPathExclude([worktreeOrigin(), mainOrigin()], undefined)
    );
    expect(out).toHaveLength(2);
  });

  it('does not over-drop: a main-tree result survives the worktree exclude', () => {
    const out = asMainCaller(() =>
      filterResultsByPathExclude([mainOrigin()], '.claude/worktrees/**')
    );
    expect(out).toHaveLength(1);
  });
});
