/**
 * Regression: the FTS5 paths (`search` with exact=true, and the `grep`
 * tool) must treat `branch: "*"` as the documented "any branch" opt-out.
 *
 * The daemon's TextSearchService has no "*" concept — passing it through
 * filters `fm.branch = '*'` literally and matches nothing. The Qdrant
 * path already drops the filter (search-filters.ts buildBranchCondition);
 * these two paths previously did not, so an exact-identifier lookup with
 * `branch:"*"` returned 0 hits even when the term was indexed. Both must
 * omit `branch` from the daemon request when it is "*", while still
 * forwarding a concrete branch name.
 */

import { describe, it, expect, vi } from 'vitest';
import { searchExact } from '../../src/tools/search-exact.js';
import { GrepTool } from '../../src/tools/grep.js';
import type { DaemonClient } from '../../src/clients/daemon-client.js';
import type { SqliteStateManager } from '../../src/clients/sqlite-state-manager.js';
import type { ProjectDetector } from '../../src/utils/project-detector.js';
import type { SearchOptions } from '../../src/tools/search-types.js';

vi.mock('../../src/utils/git-utils.js', () => ({
  getCurrentBranch: vi.fn().mockReturnValue('main'),
}));

function makeStateManager(): SqliteStateManager {
  return {
    logSearchEvent: vi.fn(),
    updateSearchEvent: vi.fn(),
    updateSearchEventEconomy: vi.fn(),
    getProjectById: vi.fn().mockReturnValue({ data: { project_path: '/some/path' } }),
    getWatchFolderIdByTenantId: vi.fn().mockReturnValue('watch-a'),
    getBaseBranch: vi.fn().mockReturnValue(null),
  } as unknown as SqliteStateManager;
}

function makeProjectDetector(projectId: string | undefined): ProjectDetector {
  return {
    findProjectRoot: vi.fn().mockReturnValue('/some/path'),
    getProjectInfo: vi
      .fn()
      .mockResolvedValue(projectId ? { projectId, projectPath: '/some/path' } : null),
  } as unknown as ProjectDetector;
}

function makeDaemonClient(): DaemonClient {
  return {
    textSearch: vi.fn().mockResolvedValue({ matches: [], total_matches: 0, truncated: false }),
    // GrepTool routes instrumentation through search-event-queries, which
    // calls these on the daemon client (logSearchEvent is awaited via .catch).
    logSearchEvent: vi.fn().mockResolvedValue(undefined),
    updateSearchEvent: vi.fn().mockResolvedValue(undefined),
    updateSearchEventEconomy: vi.fn().mockResolvedValue(undefined),
  } as unknown as DaemonClient;
}

function lastTextSearchRequest(daemon: DaemonClient): Record<string, unknown> {
  return (daemon.textSearch as ReturnType<typeof vi.fn>).mock.calls[0][0];
}

// FTS path only — Qdrant client is used by the non-`projects` scroll route.
function makeQdrant(): Parameters<typeof searchExact>[0] {
  return { scroll: vi.fn().mockResolvedValue({ points: [] }) } as unknown as Parameters<
    typeof searchExact
  >[0];
}

function makeOptions(overrides: Partial<SearchOptions> = {}): SearchOptions {
  return {
    query: 'SERVICE_STATUS_HEALTHY',
    scope: 'project',
    projectId: 'project-a',
    ...overrides,
  };
}

describe('searchExact — branch "*" wildcard', () => {
  it('defaults project searches to the current git branch', async () => {
    const daemon = makeDaemonClient();
    await searchExact(
      makeQdrant(),
      daemon,
      makeStateManager(),
      makeProjectDetector(undefined),
      makeOptions()
    );

    expect(lastTextSearchRequest(daemon).branch).toBe('main');
  });

  it('omits branch from the daemon request when branch="*"', async () => {
    const daemon = makeDaemonClient();
    await searchExact(
      makeQdrant(),
      daemon,
      makeStateManager(),
      makeProjectDetector(undefined),
      makeOptions({ branch: '*' })
    );

    const request = lastTextSearchRequest(daemon);
    expect(request.branch).toBeUndefined();
    // Tenant scoping must still apply — "*" only widens the branch.
    expect(request.tenant_id).toBe('project-a');
  });

  it('forwards a concrete branch name unchanged', async () => {
    const daemon = makeDaemonClient();
    await searchExact(
      makeQdrant(),
      daemon,
      makeStateManager(),
      makeProjectDetector(undefined),
      makeOptions({ branch: 'main' })
    );

    expect(lastTextSearchRequest(daemon).branch).toBe('main');
  });

  it('also searches the indexed base branch for feature-branch exact search', async () => {
    const daemon = makeDaemonClient();
    (daemon.textSearch as ReturnType<typeof vi.fn>)
      .mockResolvedValueOnce({ matches: [], total_matches: 0, truncated: false })
      .mockResolvedValueOnce({
        matches: [
          {
            file_path: '/repo/src/example.ts',
            line_number: 48,
            content: 'export class Example {}',
            branch: 'main',
            context_before: [],
            context_after: [],
          },
        ],
        total_matches: 1,
        truncated: false,
      });
    const state = makeStateManager();
    (state.getBaseBranch as ReturnType<typeof vi.fn>).mockReturnValue('main');

    const result = await searchExact(
      makeQdrant(),
      daemon,
      state,
      makeProjectDetector(undefined),
      makeOptions({ branch: 'feature/current' })
    );

    expect((daemon.textSearch as ReturnType<typeof vi.fn>).mock.calls[0][0].branch).toBe(
      'feature/current'
    );
    expect((daemon.textSearch as ReturnType<typeof vi.fn>).mock.calls[1][0].branch).toBe('main');
    expect(result.results).toHaveLength(1);
    expect(result.results[0].metadata.file_path).toBe('/repo/src/example.ts');
    expect(result.results[0].metadata.branch).toBe('feature/current');
    expect(result.results[0].metadata._matched_branch).toBe('main');
  });

  it('deduplicates exact-search hits returned from branch and base-branch fallbacks', async () => {
    const daemon = makeDaemonClient();
    const duplicateMatch = {
      file_path: '/repo/src/example.ts',
      line_number: 48,
      content: 'export class Example {}',
      context_before: [],
      context_after: [],
    };
    (daemon.textSearch as ReturnType<typeof vi.fn>)
      .mockResolvedValueOnce({ matches: [duplicateMatch], total_matches: 1, truncated: false })
      .mockResolvedValueOnce({ matches: [duplicateMatch], total_matches: 1, truncated: false });
    const state = makeStateManager();
    (state.getBaseBranch as ReturnType<typeof vi.fn>).mockReturnValue('main');

    const result = await searchExact(
      makeQdrant(),
      daemon,
      state,
      makeProjectDetector(undefined),
      makeOptions({ branch: 'feature/current' })
    );

    expect(result.results).toHaveLength(1);
    expect(result.total).toBe(1);
  });
});

describe('GrepTool — branch "*" wildcard', () => {
  it('defaults project searches to the current git branch', async () => {
    const daemon = makeDaemonClient();
    const tool = new GrepTool(daemon, makeProjectDetector('project-a'));

    await tool.grep({ pattern: 'SERVICE_STATUS_HEALTHY' });

    expect(lastTextSearchRequest(daemon).branch).toBe('main');
  });

  it('omits branch from the daemon request when branch="*"', async () => {
    const daemon = makeDaemonClient();
    const tool = new GrepTool(daemon, makeProjectDetector(undefined));

    await tool.grep({ pattern: 'SERVICE_STATUS_HEALTHY', projectId: 'project-a', branch: '*' });

    const request = lastTextSearchRequest(daemon);
    expect(request.branch).toBeUndefined();
    expect(request.tenant_id).toBe('project-a');
  });

  it('forwards a concrete branch name unchanged', async () => {
    const daemon = makeDaemonClient();
    const tool = new GrepTool(daemon, makeProjectDetector(undefined));

    await tool.grep({ pattern: 'SERVICE_STATUS_HEALTHY', projectId: 'project-a', branch: 'main' });

    expect(lastTextSearchRequest(daemon).branch).toBe('main');
  });

  it('also searches the indexed base branch for feature-branch grep', async () => {
    const daemon = makeDaemonClient();
    (daemon.textSearch as ReturnType<typeof vi.fn>)
      .mockResolvedValueOnce({ matches: [], total_matches: 0, truncated: false })
      .mockResolvedValueOnce({
        matches: [
          {
            file_path: '/repo/src/example.ts',
            line_number: 48,
            content: 'export class Example {}',
            context_before: [],
            context_after: [],
            file_size: 123,
          },
        ],
        total_matches: 1,
        truncated: false,
      });
    const state = makeStateManager();
    (state.getBaseBranch as ReturnType<typeof vi.fn>).mockReturnValue('main');
    const tool = new GrepTool(daemon, makeProjectDetector(undefined), state);

    const result = await tool.grep({
      pattern: 'export class Example',
      projectId: 'project-a',
      branch: 'feature/current',
    });

    expect((daemon.textSearch as ReturnType<typeof vi.fn>).mock.calls[0][0].branch).toBe(
      'feature/current'
    );
    expect((daemon.textSearch as ReturnType<typeof vi.fn>).mock.calls[1][0].branch).toBe('main');
    expect(result.matches).toHaveLength(1);
    expect(result.matches[0].file).toBe('/repo/src/example.ts');
  });
});
