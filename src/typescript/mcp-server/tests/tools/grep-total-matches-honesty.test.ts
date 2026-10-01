/**
 * Regression: grep's `total_matches` must never claim MORE matches than exist.
 *
 * It did, and the error grew as the page shrank. Measured against the real index
 * for the pattern `fn main`, whose true deduped total is 115 (`git grep` agrees):
 *
 *   countOnly    -> 115    maxResults=3  -> 216
 *   maxResults=50-> 169    maxResults=100-> 123    maxResults=500-> 115
 *
 * Two causes, both here: the per-branch responses were SUMMED although they
 * describe overlapping sets, and the correction subtracted only the duplicates
 * visible on the fetched page — so a small page corrected almost nothing. An
 * agent sizing a sweep from that number plans up to 2x the work that exists and
 * pages after a tail that is not there. Overstating is the dangerous direction;
 * `truncated: true` already carries "there is more", so a floor is enough.
 */

import { describe, it, expect, vi } from 'vitest';
import { GrepTool } from '../../src/tools/grep.js';
import type { DaemonClient } from '../../src/clients/daemon-client.js';
import type { SqliteStateManager } from '../../src/clients/sqlite-state-manager.js';
import type { ProjectDetector } from '../../src/utils/project-detector.js';

vi.mock('../../src/utils/git-utils.js', async (importOriginal) => {
  const actual = await importOriginal<typeof import('../../src/utils/git-utils.js')>();
  return { ...actual, getCurrentBranch: vi.fn().mockReturnValue('feature/x') };
});

interface DaemonRequest {
  max_results: number;
  branch?: string;
}

interface Row {
  file: string;
  line: number;
  content: string;
}

/**
 * Daemon emulator with per-branch content. Each branch answers with its own rows,
 * capped at the request's `max_results`, reporting that branch's true pre-cap
 * total — exactly what TextSearchService does now that it dedupes before counting.
 */
function daemonWithBranches(byBranch: Record<string, Row[]>): {
  daemon: DaemonClient;
  textSearch: ReturnType<typeof vi.fn>;
} {
  const textSearch = vi.fn().mockImplementation((req: DaemonRequest) => {
    const rows = byBranch[req.branch ?? '*'] ?? [];
    const cap = req.max_results > 0 ? req.max_results : rows.length;
    return Promise.resolve({
      matches: rows.slice(0, cap).map((r) => ({
        file_path: r.file,
        line_number: r.line,
        content: r.content,
        context_before: [],
        context_after: [],
      })),
      total_matches: rows.length,
      truncated: rows.length > cap,
    });
  });
  const target: Record<string, unknown> = { textSearch };
  const daemon = new Proxy(target, {
    get(t: Record<string, unknown>, prop: string | symbol) {
      if (typeof prop === 'string' && prop in t) return t[prop];
      if (prop === 'then' || typeof prop === 'symbol') return undefined;
      return () => Promise.resolve(undefined);
    },
  }) as unknown as DaemonClient;
  return { daemon, textSearch };
}

function rows(count: number, file: (i: number) => string): Row[] {
  return Array.from({ length: count }, (_, i) => ({
    file: file(i),
    line: i + 1,
    content: `fn main ${i}`,
  }));
}

function stateManager(baseBranch: string | null): SqliteStateManager {
  return {
    logSearchEvent: vi.fn(),
    updateSearchEvent: vi.fn(),
    updateSearchEventEconomy: vi.fn(),
    getProjectById: vi.fn().mockReturnValue({ data: { project_path: '/repo' } }),
    getWatchFolderIdByTenantId: vi.fn().mockReturnValue('watch-a'),
    getBaseBranch: vi.fn().mockReturnValue(baseBranch),
    getIsTestByFilePaths: vi.fn().mockReturnValue(new Map()),
    getFileAnnotationsByFilePaths: vi.fn().mockReturnValue(new Map()),
  } as unknown as SqliteStateManager;
}

const detector = {
  findProjectRoot: vi.fn().mockReturnValue('/repo'),
  getProjectInfo: vi.fn().mockResolvedValue({ projectId: 'tenant-a', projectPath: '/repo' }),
} as unknown as ProjectDetector;

describe('grep total_matches honesty', () => {
  it('never exceeds the true total as the page shrinks (the 216-for-115 regression)', async () => {
    const all = rows(115, (i) => `/repo/src/f${i}.rs`);
    const { daemon } = daemonWithBranches({ 'feature/x': all, main: all });
    const tool = new GrepTool(daemon, detector, stateManager('main'));

    for (const maxResults of [3, 50, 100, 500]) {
      const res = await tool.grep({ pattern: 'fn main', cwd: '/repo', maxResults });
      expect(res.total_matches, `maxResults=${maxResults}`).toBeLessThanOrEqual(115);
    }
  });

  it('reports the exact total when nothing is truncated', async () => {
    const all = rows(7, (i) => `/repo/src/f${i}.rs`);
    const { daemon } = daemonWithBranches({ 'feature/x': all, main: all });
    const tool = new GrepTool(daemon, detector, stateManager('main'));

    const res = await tool.grep({ pattern: 'fn main', cwd: '/repo', maxResults: 50 });

    expect(res.truncated).toBe(false);
    expect(res.total_matches).toBe(7);
  });

  it('does not sum overlapping branch scopes', async () => {
    // Both branches carry the same 10 paths. The answer is 10, never 20.
    const all = rows(10, (i) => `/repo/src/f${i}.rs`);
    const { daemon } = daemonWithBranches({ 'feature/x': all, main: all });
    const tool = new GrepTool(daemon, detector, stateManager('main'));

    const res = await tool.grep({ pattern: 'fn main', cwd: '/repo', maxResults: 100 });

    expect(res.total_matches).toBe(10);
  });

  it('uses the scoped total as the floor on a truncated page, not the page size', async () => {
    const all = rows(115, (i) => `/repo/src/f${i}.rs`);
    const { daemon } = daemonWithBranches({ 'feature/x': all, main: all });
    const tool = new GrepTool(daemon, detector, stateManager('main'));

    const res = await tool.grep({ pattern: 'fn main', cwd: '/repo', maxResults: 3 });

    expect(res.truncated).toBe(true);
    expect(res.matches).toHaveLength(3);
    // A floor worth acting on: the scoped branch's exact count, not "3".
    expect(res.total_matches).toBe(115);
  });

  it('counts a fallback-only path once, in addition to the scoped set', async () => {
    const scoped = rows(2, (i) => `/repo/src/f${i}.rs`);
    const base = [
      ...scoped, // same paths — must not be counted twice
      { file: '/repo/src/unchanged.rs', line: 1, content: 'fn main unchanged' },
    ];
    const { daemon } = daemonWithBranches({ 'feature/x': scoped, main: base });
    const tool = new GrepTool(daemon, detector, stateManager('main'));

    const res = await tool.grep({ pattern: 'fn main', cwd: '/repo', maxResults: 100 });

    expect(res.total_matches).toBe(3);
    expect(res.matches.map((m) => m.file)).toEqual([
      '/repo/src/f0.rs',
      '/repo/src/f1.rs',
      '/repo/src/unchanged.rs',
    ]);
  });

  it('serves the branch-scoped generation, never the base branch copy of the same path', async () => {
    // The file was CHANGED on the feature branch: same path, different content.
    const { daemon } = daemonWithBranches({
      'feature/x': [{ file: '/repo/src/a.rs', line: 10, content: 'fn main // fixed' }],
      main: [{ file: '/repo/src/a.rs', line: 10, content: 'fn main // pre-edit bug' }],
    });
    const tool = new GrepTool(daemon, detector, stateManager('main'));

    const res = await tool.grep({ pattern: 'fn main', cwd: '/repo', maxResults: 100 });

    expect(res.matches).toHaveLength(1);
    expect(res.matches[0]!.content).toBe('fn main // fixed');
    expect(res.total_matches).toBe(1);
  });

  it('sends pathExclude to the daemon, which applies it before the cap', async () => {
    const all = rows(5, (i) => `/repo/src/f${i}.rs`);
    const { daemon, textSearch } = daemonWithBranches({ 'feature/x': all });
    const tool = new GrepTool(daemon, detector, stateManager(null));

    await tool.grep({ pattern: 'fn main', cwd: '/repo', maxResults: 3, pathExclude: 'old/**' });

    const firstRequest = textSearch.mock.calls[0]![0] as { path_exclude?: string };
    expect(firstRequest.path_exclude).toBe('old/**');
  });

  it('does not fetch the fallback branch while the scoped result is truncated', async () => {
    // Fallback rows sort after every scoped row, so they cannot reach a window
    // the scope still overflows — and the paths the scope covers are not fully
    // known yet, which is how a stale base generation used to slip in.
    const all = rows(115, (i) => `/repo/src/f${i}.rs`);
    const { daemon, textSearch } = daemonWithBranches({ 'feature/x': all, main: all });
    const tool = new GrepTool(daemon, detector, stateManager('main'));

    await tool.grep({ pattern: 'fn main', cwd: '/repo', maxResults: 3 });

    const branches = textSearch.mock.calls.map((c) => (c[0] as DaemonRequest).branch);
    expect(branches).toEqual(['feature/x']);
  });

  it('refills a page the local filter emptied instead of returning it empty', async () => {
    // Ten foreign-worktree paths come first in index order; only this side can
    // judge them (caller-aware), so the daemon counts and returns them. A page of
    // 3 used to come back EMPTY with truncated:true — and since offset indexes
    // the FILTERED list, no offset could page past it.
    const foreign = rows(10, (i) => `/repo/.claude/worktrees/other/src/w${i}.rs`);
    const real = rows(5, (i) => `/repo/src/real${i}.rs`);
    const { daemon, textSearch } = daemonWithBranches({ 'feature/x': [...foreign, ...real] });
    const tool = new GrepTool(daemon, detector, stateManager(null));

    const res = await tool.grep({ pattern: 'fn main', cwd: '/repo', maxResults: 3 });

    expect(res.matches.map((m) => m.file)).toEqual([
      '/repo/src/real0.rs',
      '/repo/src/real1.rs',
      '/repo/src/real2.rs',
    ]);
    expect(textSearch.mock.calls.length).toBeGreaterThan(1);
    expect(res.next_offset).toBe(3);
  });

  it('does not report the daemon total when the local filter dropped rows it counted', async () => {
    const foreign = rows(10, (i) => `/repo/.claude/worktrees/other/src/w${i}.rs`);
    const real = rows(5, (i) => `/repo/src/real${i}.rs`);
    const { daemon } = daemonWithBranches({ 'feature/x': [...foreign, ...real] });
    const tool = new GrepTool(daemon, detector, stateManager(null));

    const res = await tool.grep({ pattern: 'fn main', cwd: '/repo', maxResults: 3 });

    // The daemon says 15; five of them are servable. Never claim more.
    expect(res.total_matches).toBeLessThanOrEqual(5);
    expect(res.total_matches).toBeGreaterThanOrEqual(3);
  });

  it('does not widen at all when the caller is already on the trunk', async () => {
    const all = rows(5, (i) => `/repo/src/f${i}.rs`);
    const { daemon, textSearch } = daemonWithBranches({ 'feature/x': all });
    // getBaseBranch returns null on the trunk — one query, no fallback.
    const tool = new GrepTool(daemon, detector, stateManager(null));

    const res = await tool.grep({ pattern: 'fn main', cwd: '/repo', maxResults: 100 });

    expect(res.total_matches).toBe(5);
    expect(textSearch).toHaveBeenCalledTimes(1);
  });
});
