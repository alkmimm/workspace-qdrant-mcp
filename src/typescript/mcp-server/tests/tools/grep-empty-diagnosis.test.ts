/**
 * Empty-result diagnosis for the grep tool: a bare "No matches" must not
 * conflate "pattern absent" with "the filter/branch hid it". When a
 * project-scoped grep is still empty after the branch-widen, grep runs two
 * cheap probes and surfaces a specific message:
 *   1. a pathGlob/pathExclude that excluded everything, and
 *   2. a branch with no indexed content (freshly created / not-yet-indexed).
 */

import { describe, it, expect, vi } from 'vitest';
import { GrepTool } from '../../src/tools/grep.js';
import {
  pathFilterExcludedAllMessage,
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

function makeReader(rows: Array<{ branch: string; files: number }>): SearchDbReader {
  return {
    listBranchCounts: vi.fn().mockReturnValue(
      rows.map((r) => ({
        tenant_id: 'project-a',
        branch: r.branch,
        files: r.files,
        total_bytes: 0,
      }))
    ),
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
  it('reports when a pathGlob excluded everything (mode #2)', async () => {
    // Empty WITH a path_glob, non-empty WITHOUT it → the glob is the cause.
    const { daemon } = makeDaemon((req) =>
      req.path_glob
        ? Promise.resolve({ matches: [], total_matches: 0, truncated: false })
        : Promise.resolve({ matches: [MATCH], total_matches: 1, truncated: false })
    );
    const tool = new GrepTool(
      daemon,
      makeProjectDetector('project-a'),
      makeStateManager(),
      makeReader([{ branch: 'main', files: 5 }])
    );

    const res = await tool.grep({
      pattern: '@Service',
      scope: 'project',
      projectId: 'project-a',
      branch: 'main',
      pathGlob: 'management/test/**/*.dart',
    });

    expect(res.matches).toHaveLength(0);
    expect(res.message).toMatch(/path filter excluded everything/i);
    expect(res.message).toMatch(/ADJACENT/);
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
    expect(res.message).toMatch(/no indexed content yet/i);
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
    expect(res.message).not.toMatch(/no indexed content/i);
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
    expect(msg).toMatch(/ADJACENT/);
  });

  it('branchNotIndexedMessage lists indexed branches (or "(none yet)")', () => {
    expect(branchNotIndexedMessage('feature-x', ['main', 'develop'])).toContain('main, develop');
    expect(branchNotIndexedMessage('feature-x', [])).toContain('(none yet)');
  });
});
