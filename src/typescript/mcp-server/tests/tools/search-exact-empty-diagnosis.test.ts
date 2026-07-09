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

function makeReader(
  rows: Array<{ branch: string; files: number }>,
  filesMatchingPathFilters = 0
): SearchDbReader {
  return {
    listBranchCounts: vi
      .fn()
      .mockReturnValue(
        rows.map((r) => ({ tenant_id: 'p-a', branch: r.branch, files: r.files, total_bytes: 0 }))
      ),
    // Second path-filter probe: 0 → glob shape excluded everything; > 0 → glob
    // is well-formed and the pattern is simply absent from those files.
    countFilesMatchingPathFilters: vi.fn().mockReturnValue(filesMatchingPathFilters),
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
  it('blames the glob SHAPE only when it selects no indexed file', async () => {
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
      makeReader([{ branch: 'main', files: 5 }], 0) // glob selects 0 files
    );
    expect(res.results).toHaveLength(0);
    expect(res.hint).toMatch(/matches NO indexed file/i);
    expect(res.hint).toMatch(/ADJACENT/);
  });

  it('does NOT blame a well-formed glob when the pattern is just absent from those files', async () => {
    // Parity with grep: `reference_schedule` under `**/*.proto` — the glob selects
    // real .proto files, the same query without it has hits elsewhere, but the
    // literal isn't in the .proto files. Must NOT accuse the glob of malformation.
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
      opts({ query: 'reference_schedule', pathGlob: '**/*.proto' }),
      'evt-1b',
      makeReader([{ branch: 'main', files: 5 }], 12) // glob selects 12 real files
    );
    expect(res.results).toHaveLength(0);
    expect(res.hint).toMatch(/well-formed/i);
    expect(res.hint).toMatch(/naming\/casing/i);
    expect(res.hint).not.toMatch(/ADJACENT/);
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
    expect(res.hint).toMatch(/0 files indexed under its own name/i);
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
    expect(res.hint).not.toMatch(/0 files indexed under its own name/i);
    expect(res.hint).toMatch(/scope:"all"/i);
  });

  it('does NOT claim "results may be from another branch" when pathExclude emptied the widened set', async () => {
    // Primary branch is empty → branch-widen fires and finds a cross-branch hit,
    // but pathExclude then removes it → results:[]. The hint must be honest about
    // the exclusion, not the misleading "results may be from another branch".
    const excludedHit = { ...MATCH, file_path: '/repo/excluded/Foo.java' };
    const daemon = makeDaemon((req) =>
      req.branch === 'main'
        ? Promise.resolve({ matches: [], total_matches: 0, truncated: false })
        : Promise.resolve({ matches: [excludedHit], total_matches: 1, truncated: false })
    );
    const res = await searchExact(
      makeQdrant(),
      daemon,
      makeStateManager(),
      makeProjectDetector('p-a'),
      opts({ branch: 'main', pathExclude: 'excluded/**' }),
      'evt-4',
      makeReader([{ branch: 'main', files: 99 }])
    );
    expect(res.results).toHaveLength(0);
    expect(res.hint).toMatch(/removed by pathExclude/i);
    expect(res.hint).not.toMatch(/results may be from another indexed branch/i);
  });
});
