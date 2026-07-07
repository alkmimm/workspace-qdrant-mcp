/**
 * Empty-result diagnosis parity for exact search (shared with grep via
 * tools/empty-diagnosis.ts). A project-scoped exact miss must distinguish
 * "pattern absent" from "pathGlob excluded everything" and "branch not indexed"
 * instead of the generic scope-opt-in hint (CLAUDE.md shared-behavior rule).
 */

import { describe, it, expect, vi } from 'vitest';
import { searchExact } from '../../src/tools/search-exact.js';
import type { QdrantClient } from '@qdrant/js-client-rest';
import type { DaemonClient } from '../../src/clients/daemon-client.js';
import type { SqliteStateManager } from '../../src/clients/sqlite-state-manager.js';
import type { ProjectDetector } from '../../src/utils/project-detector.js';
import type { SearchDbReader } from '../../src/clients/search-db-reader.js';
import type { SearchOptions } from '../../src/tools/search-types.js';

vi.mock('../../src/utils/git-utils.js', () => ({
  getCurrentBranch: vi.fn().mockReturnValue('main'),
}));

function makeStateManager(): SqliteStateManager {
  return {
    logSearchEvent: vi.fn(),
    updateSearchEvent: vi.fn(),
    updateSearchEventEconomy: vi.fn(),
    getProjectById: vi.fn().mockReturnValue({ data: { project_path: '/repo' } }),
    getWatchFolderIdByTenantId: vi.fn().mockReturnValue('watch-a'),
    getBaseBranch: vi.fn().mockReturnValue(null),
  } as unknown as SqliteStateManager;
}

function makeProjectDetector(projectId: string | undefined): ProjectDetector {
  return {
    findProjectRoot: vi.fn().mockReturnValue('/repo'),
    getProjectInfo: vi
      .fn()
      .mockResolvedValue(projectId ? { projectId, projectPath: '/repo' } : null),
  } as unknown as ProjectDetector;
}

function makeDaemon(
  impl: (req: { branch?: string; path_glob?: string }) => Promise<unknown>
): DaemonClient {
  return {
    textSearch: vi.fn().mockImplementation(impl),
    logSearchEvent: vi.fn().mockResolvedValue(undefined),
    updateSearchEvent: vi.fn().mockResolvedValue(undefined),
    updateSearchEventEconomy: vi.fn().mockResolvedValue(undefined),
    getIndexingProgress: vi.fn().mockResolvedValue(undefined),
  } as unknown as DaemonClient;
}

function makeQdrant(): QdrantClient {
  return { scroll: vi.fn().mockResolvedValue({ points: [] }) } as unknown as QdrantClient;
}

function makeReader(rows: Array<{ branch: string; files: number }>): SearchDbReader {
  return {
    listBranchCounts: vi
      .fn()
      .mockReturnValue(
        rows.map((r) => ({ tenant_id: 'p-a', branch: r.branch, files: r.files, total_bytes: 0 }))
      ),
  } as unknown as SearchDbReader;
}

const MATCH = {
  file_path: '/repo/src/Foo.java',
  line_number: 3,
  content: '@Service',
  tenant_id: 'p-a',
  branch: 'main',
  context_before: [],
  context_after: [],
  file_size: 10,
};

function opts(o: Partial<SearchOptions>): SearchOptions {
  return { query: '@Service', scope: 'project', projectId: 'p-a', ...o };
}

describe('searchExact — empty-result diagnosis', () => {
  it('reports when a pathGlob excluded everything', async () => {
    const daemon = makeDaemon((req) =>
      req.path_glob
        ? Promise.resolve({ matches: [], total_matches: 0, truncated: false })
        : Promise.resolve({ matches: [MATCH], total_matches: 1, truncated: false })
    );
    const res = await searchExact(
      makeQdrant(),
      daemon,
      makeStateManager(),
      makeProjectDetector('p-a'),
      opts({ pathGlob: 'management/test/**/*.dart' }),
      'evt-1',
      makeReader([{ branch: 'main', files: 5 }])
    );
    expect(res.results).toHaveLength(0);
    expect(res.hint).toMatch(/path filter excluded everything/i);
  });

  it('reports when the requested branch has no indexed content', async () => {
    const daemon = makeDaemon(() =>
      Promise.resolve({ matches: [], total_matches: 0, truncated: false })
    );
    const res = await searchExact(
      makeQdrant(),
      daemon,
      makeStateManager(),
      makeProjectDetector('p-a'),
      opts({ branch: 'fix/x' }),
      'evt-2',
      makeReader([
        { branch: 'main', files: 99 },
        { branch: 'develop', files: 4 },
      ])
    );
    expect(res.results).toHaveLength(0);
    expect(res.hint).toMatch(/no indexed content yet/i);
    expect(res.hint).toMatch(/main, develop/);
  });

  it('keeps the generic opt-in hint when neither probe applies', async () => {
    const daemon = makeDaemon(() =>
      Promise.resolve({ matches: [], total_matches: 0, truncated: false })
    );
    const res = await searchExact(
      makeQdrant(),
      daemon,
      makeStateManager(),
      makeProjectDetector('p-a'),
      opts({ query: 'NopeNotHere', branch: 'main' }),
      'evt-3',
      makeReader([{ branch: 'main', files: 99 }])
    );
    expect(res.results).toHaveLength(0);
    expect(res.hint).not.toMatch(/no indexed content/i);
    expect(res.hint).toMatch(/scope:"all"/i);
  });
});
