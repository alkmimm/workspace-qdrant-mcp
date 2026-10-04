/**
 * `graph` answers for one branch, like search/grep/list.
 *
 * The daemon keeps each file's version per branch; before that a branch's
 * relations/impact answered with whichever branch was indexed last, and a file
 * the branch deleted stayed in its answers. These pin the client half: the
 * branch reaches every request (explicit wins, else the checkout at the project
 * path), and an answer from a partially rebuilt graph says so before the data.
 */
import { describe, it, expect, vi, beforeEach } from 'vitest';

vi.mock('../../src/utils/git-utils.js', async (importOriginal) => {
  const actual = await importOriginal<typeof import('../../src/utils/git-utils.js')>();
  return { ...actual, getCurrentBranch: vi.fn(() => 'fase-5') };
});

import { graphCoverageHint, handleGraph } from '../../src/tools/graph.js';
import type { DaemonClient } from '../../src/clients/daemon-client.js';
import type { ProjectDetector } from '../../src/utils/project-detector.js';
import { getCurrentBranch } from '../../src/utils/git-utils.js';

const detector = {
  getProjectInfo: vi.fn().mockResolvedValue({ projectId: 't1', projectPath: '/repo' }),
} as unknown as ProjectDetector;

function client(scope?: { branch: string; indexed_files: number; graphed_files: number }) {
  const stats = { total_nodes: 1, total_edges: 0, nodes_by_type: {}, edges_by_type: {}, scope };
  return {
    getGraphStats: vi.fn().mockResolvedValue(stats),
    impactAnalysis: vi
      .fn()
      .mockResolvedValue({ impacted_nodes: [], total_impacted: 0, query_time_ms: 1, scope }),
    detectCycles: vi
      .fn()
      .mockResolvedValue({ cycles: [], total: 0, query_time_ms: 1, suppressed_ubiquitous: 2, scope }),
    detectCommunities: vi.fn().mockResolvedValue({
      communities: [],
      total_communities: 0,
      query_time_ms: 1,
      scope,
    }),
    textSearchCount: vi.fn().mockResolvedValue({ count: 0 }),
  } as unknown as DaemonClient & Record<string, ReturnType<typeof vi.fn>>;
}

describe('graph branch scoping', () => {
  beforeEach(() => vi.mocked(getCurrentBranch).mockReturnValue('fase-5'));

  it('asks for the branch checked out at the project path by default', async () => {
    const c = client();
    await handleGraph({ action: 'stats' }, c, detector);
    expect(c.getGraphStats).toHaveBeenCalledWith({ tenant_id: 't1', branch: 'fase-5' });
  });

  it('an explicit branch wins, including * for every branch', async () => {
    const c = client();
    await handleGraph({ action: 'impact', symbol: 'save', branch: 'develop' }, c, detector);
    expect(c.impactAnalysis.mock.calls[0][0]).toMatchObject({ branch: 'develop' });
    await handleGraph({ action: 'stats', branch: '*' }, c, detector);
    expect(c.getGraphStats).toHaveBeenLastCalledWith({ tenant_id: 't1', branch: '*' });
  });

  it('leaves the branch to the daemon when no checkout names one', async () => {
    vi.mocked(getCurrentBranch).mockReturnValue(null as unknown as string);
    const c = client();
    await handleGraph({ action: 'stats' }, c, detector);
    expect(c.getGraphStats).toHaveBeenCalledWith({ tenant_id: 't1' });
  });

  it('leads with the partial-graph caveat, ahead of the action hint', async () => {
    const c = client({ branch: 'fase-5', indexed_files: 400, graphed_files: 102 });
    const r = (await handleGraph({ action: 'cycles' }, c, detector)) as { hint: string };
    expect(r.hint.startsWith('PARTIAL GRAPH: it covers 102 of 400 file versions')).toBe(true);
    expect(r.hint).toContain('excluded before cycle detection');
  });

  it('carries the scope through the modules reshaping', async () => {
    const scope = { branch: 'fase-5', indexed_files: 10, graphed_files: 10 };
    const r = (await handleGraph({ action: 'modules' }, client(scope), detector)) as {
      scope: unknown;
      hint?: string;
    };
    expect(r.scope).toEqual(scope);
    expect(r.hint).toBeUndefined();
  });
});

describe('graphCoverageHint', () => {
  it('is silent for a complete graph, an unscoped answer, or no scope', () => {
    expect(graphCoverageHint({ branch: 'main', indexed_files: 5, graphed_files: 5 })).toBeUndefined();
    expect(graphCoverageHint({ branch: '*', indexed_files: 5, graphed_files: 1 })).toBeUndefined();
    expect(graphCoverageHint(undefined)).toBeUndefined();
  });

  it('flags a branch that holds nothing as a probable typo', () => {
    const hint = graphCoverageHint({ branch: 'fase5', indexed_files: 0, graphed_files: 0 });
    expect(hint).toContain("branch 'fase5' holds no indexed files");
  });

  it('names a non-git project without a branch', () => {
    const hint = graphCoverageHint({ branch: '', indexed_files: 3, graphed_files: 1 });
    expect(hint).toContain('on this project');
  });
});
