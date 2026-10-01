/**
 * `search` with `exact: true` — paging and the exclude.
 *
 * Paging was broken outright: the daemon request asked for `limit` rows while
 * the page was `slice(offset, offset + limit)`, so every page past the first was
 * EMPTY (measured live: offset 5 and 10 returned 0 of 2042 matches) and
 * `next_offset` never appeared, because the fetched list could never run past
 * the page it was cut to. `grep` had always fetched `offset + maxResults`; this
 * surface had not.
 */

import { describe, it, expect, vi } from 'vitest';
import { searchExact } from '../../src/tools/search-exact.js';
import type { DaemonClient } from '../../src/clients/daemon-client.js';
import type { SqliteStateManager } from '../../src/clients/sqlite-state-manager.js';
import type { ProjectDetector } from '../../src/utils/project-detector.js';
import type { SearchOptions } from '../../src/tools/search-types.js';

vi.mock('../../src/utils/git-utils.js', async (importOriginal) => {
  const actual = await importOriginal<typeof import('../../src/utils/git-utils.js')>();
  return { ...actual, getCurrentBranch: vi.fn().mockReturnValue('main') };
});

interface DaemonRequest {
  max_results: number;
  branch?: string;
  path_glob?: string;
  path_exclude?: string;
}

/**
 * Daemon emulator: `total` distinct matches in index order, capped at the
 * request's `max_results`, reporting the true pre-cap total. An exclude makes it
 * return nothing — enough to drive the empty-result diagnosis.
 */
function daemon(total: number): { client: DaemonClient; textSearch: ReturnType<typeof vi.fn> } {
  const all = Array.from({ length: total }, (_, i) => ({
    file_path: `/repo/src/f${String(i).padStart(3, '0')}.rs`,
    line_number: 1,
    content: `needle ${i}`,
    context_before: [],
    context_after: [],
  }));
  const textSearch = vi.fn().mockImplementation((req: DaemonRequest) => {
    const rows = req.path_exclude ? [] : all;
    const cap = req.max_results > 0 ? req.max_results : rows.length;
    return Promise.resolve({
      matches: rows.slice(0, cap),
      total_matches: rows.length,
      truncated: rows.length > cap,
    });
  });
  const client = {
    textSearch,
    logSearchEvent: vi.fn().mockResolvedValue(undefined),
    updateSearchEvent: vi.fn().mockResolvedValue(undefined),
    updateSearchEventEconomy: vi.fn().mockResolvedValue(undefined),
  } as unknown as DaemonClient;
  return { client, textSearch };
}

const stateManager = {
  logSearchEvent: vi.fn(),
  updateSearchEvent: vi.fn(),
  updateSearchEventEconomy: vi.fn(),
  getProjectById: vi.fn().mockReturnValue({ data: { project_path: '/repo' } }),
  getWatchFolderIdByTenantId: vi.fn().mockReturnValue('watch-a'),
  getBaseBranch: vi.fn().mockReturnValue(null),
  getFileAnnotationsByFilePaths: vi.fn().mockReturnValue(new Map()),
} as unknown as SqliteStateManager;

const detector = {
  findProjectRoot: vi.fn().mockReturnValue('/repo'),
  getProjectInfo: vi.fn().mockResolvedValue({ projectId: 'tenant-a', projectPath: '/repo' }),
} as unknown as ProjectDetector;

const qdrant = { scroll: vi.fn().mockResolvedValue({ points: [] }) } as unknown as Parameters<
  typeof searchExact
>[0];

function options(overrides: Partial<SearchOptions> = {}): SearchOptions {
  return {
    query: 'needle',
    scope: 'project',
    projectId: 'tenant-a',
    exact: true,
    cwd: '/repo',
    ...overrides,
  };
}

const contents = (res: Awaited<ReturnType<typeof searchExact>>) => res.results.map((r) => r.content);

describe('exact search paging', () => {
  it('announces a next page on the first page', async () => {
    const { client } = daemon(20);
    const res = await searchExact(qdrant, client, stateManager, detector, options({ limit: 5 }));

    expect(contents(res)).toEqual(['needle 0', 'needle 1', 'needle 2', 'needle 3', 'needle 4']);
    expect(res.next_offset).toBe(5);
    expect(res.total).toBe(20);
  });

  it('returns the requested window past the first page (it used to be empty)', async () => {
    const { client, textSearch } = daemon(20);
    const res = await searchExact(
      qdrant,
      client,
      stateManager,
      detector,
      options({ limit: 5, offset: 5 })
    );

    expect(contents(res)).toEqual(['needle 5', 'needle 6', 'needle 7', 'needle 8', 'needle 9']);
    expect(res.next_offset).toBe(10);
    // The fetch must reach offset + limit deep.
    expect((textSearch.mock.calls[0]![0] as DaemonRequest).max_results).toBe(10);
  });

  it('ends without next_offset on the last page', async () => {
    const { client } = daemon(20);
    const res = await searchExact(
      qdrant,
      client,
      stateManager,
      detector,
      options({ limit: 5, offset: 15 })
    );

    expect(contents(res)).toEqual(['needle 15', 'needle 16', 'needle 17', 'needle 18', 'needle 19']);
    expect(res.next_offset).toBeUndefined();
    expect(res.total).toBe(20);
  });
});

describe('exact search pathExclude', () => {
  it('sends the exclude to the daemon, which applies it before the cap', async () => {
    const { client, textSearch } = daemon(20);
    await searchExact(
      qdrant,
      client,
      stateManager,
      detector,
      options({ limit: 5, pathExclude: 'old/**' })
    );

    expect((textSearch.mock.calls[0]![0] as DaemonRequest).path_exclude).toBe('old/**');
  });

  it('keeps the unfiltered diagnosis probe unfiltered', async () => {
    // The probe measures the scope WITHOUT path filters to say "your filter
    // excluded everything". Now that the exclude travels to the daemon, a probe
    // that only dropped pathGlob would have filtered the very scope it measures,
    // found nothing, and blamed absence instead of the filter.
    // branch:"*" keeps the branch auto-widen (and its own probe) out of the way,
    // so this exercises the diagnosis probe alone.
    const { client, textSearch } = daemon(20);
    const res = await searchExact(
      qdrant,
      client,
      stateManager,
      detector,
      options({ limit: 5, branch: '*', pathExclude: 'src/**' })
    );

    const unfiltered = textSearch.mock.calls
      .map((c) => c[0] as DaemonRequest)
      .filter((r) => r.path_exclude === undefined && r.path_glob === undefined);
    expect(unfiltered.length).toBeGreaterThan(0);
    expect(res.results).toHaveLength(0);
    expect(res.hint).toMatch(/without the path filter/i);
  });
});
