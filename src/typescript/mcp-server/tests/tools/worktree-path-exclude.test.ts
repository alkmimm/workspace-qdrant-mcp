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
import { filterResultsByPathExclude, applyPathDerank } from '../../src/tools/search-path-filters.js';
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

  it('a worktree caller still does not see a SIBLING worktree (grep parity)', () => {
    const SIBLING = '.claude/worktrees/other-agent-b7/lib/page.dart';
    const out = asWorktreeCaller(() =>
      filterGrepMatchesByExclude([gm(SIBLING), gm(WT), gm(MAIN)], undefined)
    );
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

// Shape rationale: see the module header of search-path-filters.ts (single
// home). Short version: worktree-origin points carry a MAIN-anchored
// relative_path; only the absolute path has `.claude/worktrees/<name>/`.
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

  it('a worktree caller still does not see a SIBLING worktree', () => {
    // The gate is name-aware, not presence-only: /batch runs sibling agents in
    // parallel worktrees, and agent A editing a hit from agent B's tree is the
    // original field failure with a different victim (review finding).
    const sibling: SearchResult = {
      id: 'sibling-wt',
      score: 1,
      collection: 'projects',
      content: '',
      metadata: {
        relative_path: MAIN,
        file_path: '/repo/.claude/worktrees/other-agent-b7/lib/page.dart',
      },
    };
    const out = asWorktreeCaller(() =>
      filterResultsByPathExclude([sibling, worktreeOrigin(), mainOrigin()], undefined)
    );
    expect(out.map((r) => r.id)).toEqual(['wt-origin', 'main-origin']);
  });

  it('worktree-caller gate applies even when an explicit exclude runs', () => {
    // pathExclude set (so the early return cannot fire) but not matching:
    // the gate must still keep the caller's own worktree paths.
    const out = asWorktreeCaller(() =>
      filterResultsByPathExclude([worktreeOrigin(), mainOrigin()], 'old_project/**')
    );
    expect(out).toHaveLength(2);
  });

  it('an explicit worktree INCLUDE suppresses the default drop', () => {
    // pathGlob targeting the subtree is consent to see those paths; the
    // default drop cancelling an explicit include returned a deterministic
    // zero with no diagnostic (review finding).
    const out = asMainCaller(() =>
      filterResultsByPathExclude([worktreeOrigin(), mainOrigin()], undefined, `${WT.split('/lib/')[0]}/**`)
    );
    expect(out).toHaveLength(2);
  });

  it('a floated exclude cannot match host ANCESTORS of the repo', () => {
    // Regression for the raw-absolute candidate: a checkout under ~/docs must
    // not have every hit dropped by pathExclude "docs/**". The glob is tested
    // against repo-relative coordinates only.
    const ancestor: SearchResult = {
      id: 'under-docs-home',
      score: 1,
      collection: 'projects',
      content: '',
      metadata: {
        relative_path: 'src/a.ts',
        file_path: '/home/u/docs/myproj/src/a.ts',
      },
    };
    const out = asMainCaller(() => filterResultsByPathExclude([ancestor], 'docs/**'));
    expect(out).toHaveLength(1);
  });

  it('falls back to the raw absolute only when no relative path exists', () => {
    // The exact/grep result shape: file_path only. Pre-existing globs are
    // written against it, and the worktree exclude must still bite.
    const absOnly: SearchResult = {
      id: 'abs-only',
      score: 1,
      collection: 'projects',
      content: '',
      metadata: { file_path: `/repo/${WT}` },
    };
    const out = asMainCaller(() =>
      filterResultsByPathExclude([absOnly], '.claude/worktrees/**')
    );
    expect(out).toHaveLength(0);
  });
});

describe('applyPathDerank — sees the absolute path too', () => {
  it('deranks a worktree-origin point via an absolute-only substring', () => {
    // The hard exclude and the soft derank must agree on what "the path of a
    // result" means — the derank stopping at the main-anchored relative_path
    // made WQM_SEARCH_DERANK=".claude/worktrees/" a silent no-op on exactly
    // the point class the exclude handles (review finding).
    const wt: SearchResult = {
      id: 'wt',
      score: 1,
      collection: 'projects',
      content: '',
      metadata: { relative_path: MAIN, file_path: `/repo/${WT}` },
    };
    const main: SearchResult = {
      id: 'main',
      score: 1,
      collection: 'projects',
      content: '',
      metadata: { relative_path: MAIN, file_path: `/repo/${MAIN}` },
    };
    applyPathDerank([wt, main], { substrings: ['.claude/worktrees/'], penalty: 0.2 });
    expect(wt.score).toBeCloseTo(0.2);
    expect(main.score).toBe(1);
  });
});
