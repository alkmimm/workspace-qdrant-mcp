/**
 * Empty-result diagnosis for the grep tool: a bare "No matches" must not
 * conflate "pattern absent" with "the filter/branch hid it". When a
 * project-scoped grep is still empty after the branch-widen, grep runs cheap
 * probes and surfaces a specific message:
 *   1a. a path filter whose SHAPE selects no indexed file (malformed / too
 *       restrictive),
 *   1b. a well-formed path filter over files that simply don't contain the
 *       pattern (naming/casing, not a broken glob), and
 *   2.  a branch with no indexed content (freshly created / not-yet-indexed).
 */

import { describe, it, expect, vi } from 'vitest';
import { GrepTool } from '../../src/tools/grep.js';
import {
  pathFilterExcludedAllMessage,
  patternAbsentUnderPathFilterMessage,
  branchNotIndexedMessage,
} from '../../src/tools/empty-diagnosis.js';
import type { DaemonClient } from '../../src/clients/daemon-client.js';
import type { SqliteStateManager } from '../../src/clients/sqlite-state-manager.js';
import type { ProjectDetector } from '../../src/utils/project-detector.js';
import type { SearchDbReader } from '../../src/clients/search-db-reader.js';

function makeStateManager(): SqliteStateManager {
  return {
    getWatchFolderIdByTenantId: vi.fn().mockReturnValue(null),
    getBaseBranch: vi.fn().mockReturnValue(null),
    getProjectById: vi.fn().mockReturnValue({ data: null }),
  } as unknown as SqliteStateManager;
}

function makeProjectDetector(projectId: string | undefined): ProjectDetector {
  return {
    findProjectRoot: vi.fn().mockReturnValue('/some/path'),
    getProjectInfo: vi.fn().mockResolvedValue(projectId ? { projectId } : null),
  } as unknown as ProjectDetector;
}

/** Daemon whose textSearch is driven by `impl`; all other (instrumentation)
 *  methods resolve to undefined so event logging never throws. */
function makeDaemon(impl: (req: { branch?: string; path_glob?: string }) => Promise<unknown>): {
  daemon: DaemonClient;
  textSearch: ReturnType<typeof vi.fn>;
} {
  const textSearch = vi.fn().mockImplementation(impl);
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

function makeReader(
  rows: Array<{ branch: string; files: number }>,
  filesMatchingPathFilters = 0
): SearchDbReader {
  return {
    listBranchCounts: vi.fn().mockReturnValue(
      rows.map((r) => ({
        tenant_id: 'project-a',
        branch: r.branch,
        files: r.files,
        total_bytes: 0,
      }))
    ),
    // Second path-filter probe: how many indexed files the glob selects,
    // regardless of pattern. 0 → filter shape excluded everything; > 0 → filter
    // is well-formed and the pattern is simply absent from those files.
    countFilesMatchingPathFilters: vi.fn().mockReturnValue(filesMatchingPathFilters),
  } as unknown as SearchDbReader;
}

const MATCH = {
  file_path: '/repo/src/Foo.java',
  line_number: 10,
  content: '@Service class Foo {}',
  tenant_id: 'project-a',
  branch: 'main',
  context_before: [],
  context_after: [],
  file_size: 1234,
};

describe('GrepTool — empty-result diagnosis', () => {
  it('blames the glob SHAPE only when it selects no indexed file', async () => {
    // Empty WITH a path_glob, non-empty WITHOUT it, AND the glob matches 0
    // indexed files → the filter shape is genuinely the cause.
    const { daemon } = makeDaemon((req) =>
      req.path_glob
        ? Promise.resolve({ matches: [], total_matches: 0, truncated: false })
        : Promise.resolve({ matches: [MATCH], total_matches: 1, truncated: false })
    );
    const tool = new GrepTool(
      daemon,
      makeProjectDetector('project-a'),
      makeStateManager(),
      makeReader([{ branch: 'main', files: 5 }], 0) // glob selects 0 files
    );

    const res = await tool.grep({
      pattern: '@Service',
      scope: 'project',
      projectId: 'project-a',
      branch: 'main',
      pathGlob: 'management/test/**/*.dart',
    });

    expect(res.matches).toHaveLength(0);
    expect(res.message).toMatch(/matches NO indexed file/i);
    expect(res.message).toMatch(/ADJACENT/);
  });

  it('does NOT blame a well-formed glob when the pattern is just absent from those files', async () => {
    // The reported false-blame: `reference_schedule` (snake_case) under
    // `**/*.proto`. The glob selects real .proto files, the same query without
    // it has hits elsewhere, but the literal isn't in the .proto files (they use
    // PascalCase). The message must NOT accuse the glob of being malformed.
    const { daemon } = makeDaemon((req) =>
      req.path_glob
        ? Promise.resolve({ matches: [], total_matches: 0, truncated: false })
        : Promise.resolve({ matches: [MATCH], total_matches: 1, truncated: false })
    );
    const tool = new GrepTool(
      daemon,
      makeProjectDetector('project-a'),
      makeStateManager(),
      makeReader([{ branch: 'main', files: 5 }], 12) // glob selects 12 real files
    );

    const res = await tool.grep({
      pattern: 'reference_schedule',
      scope: 'project',
      projectId: 'project-a',
      branch: 'main',
      pathGlob: '**/*.proto',
    });

    expect(res.matches).toHaveLength(0);
    expect(res.message).toMatch(/well-formed/i);
    expect(res.message).toMatch(/naming\/casing/i);
    // Must NOT reach for the shape-blame advice.
    expect(res.message).not.toMatch(/ADJACENT/);
  });

  it('reports when the requested branch has no indexed content (mode #1)', async () => {
    // Pattern absent on every branch (all queries empty), and the requested
    // branch is not among the indexed branches → branch-not-indexed, not absent.
    const { daemon } = makeDaemon(() =>
      Promise.resolve({ matches: [], total_matches: 0, truncated: false })
    );
    const tool = new GrepTool(
      daemon,
      makeProjectDetector('project-a'),
      makeStateManager(),
      makeReader([
        { branch: 'main', files: 145 },
        { branch: 'develop', files: 10 },
      ])
    );

    const res = await tool.grep({
      pattern: '@Service',
      scope: 'project',
      projectId: 'project-a',
      branch: 'fix/magic-numbers',
    });

    expect(res.matches).toHaveLength(0);
    expect(res.message).toMatch(/0 files indexed under its own name/i);
    expect(res.message).toMatch(/main, develop/);
  });

  it('does NOT claim branch-not-indexed when the branch IS indexed (no false positive)', async () => {
    const { daemon } = makeDaemon(() =>
      Promise.resolve({ matches: [], total_matches: 0, truncated: false })
    );
    const tool = new GrepTool(
      daemon,
      makeProjectDetector('project-a'),
      makeStateManager(),
      makeReader([{ branch: 'main', files: 145 }])
    );

    const res = await tool.grep({
      pattern: 'DoesNotExistAnywhere',
      scope: 'project',
      projectId: 'project-a',
      branch: 'main',
    });

    expect(res.matches).toHaveLength(0);
    expect(res.message).not.toMatch(/0 files indexed under its own name/i);
    // Falls back to the generic scope-opt-in hint.
    expect(res.message).toMatch(/scope:"all"/i);
  });
});

describe('grep empty-diagnosis message helpers', () => {
  it('pathFilterExcludedAllMessage names both filters and the adjacency rule', () => {
    const msg = pathFilterExcludedAllMessage(50, '**/x.dart', 'old_project/**');
    expect(msg).toContain('pathGlob "**/x.dart"');
    expect(msg).toContain('pathExclude "old_project/**"');
    expect(msg).toContain('50+'); // probe cap reached → "+"
    expect(msg).toMatch(/matches NO indexed file/i);
    expect(msg).toMatch(/ADJACENT/);
  });

  it('patternAbsentUnderPathFilterMessage says the filter is well-formed and hints casing', () => {
    const msg = patternAbsentUnderPathFilterMessage(50, 12, '**/*.proto', undefined);
    expect(msg).toContain('pathGlob "**/*.proto"');
    expect(msg).toContain('12 indexed file(s)'); // below cap → no "+"
    expect(msg).toContain('50+'); // unfiltered probe hit the cap → "+"
    expect(msg).toMatch(/well-formed/i);
    expect(msg).toMatch(/naming\/casing/i);
    expect(msg).not.toMatch(/ADJACENT/); // must not blame glob shape
  });

  it('branchNotIndexedMessage lists indexed branches (or "(none yet)")', () => {
    expect(branchNotIndexedMessage('feature-x', ['main', 'develop'])).toContain('main, develop');
    expect(branchNotIndexedMessage('feature-x', [])).toContain('(none yet)');
  });
});
