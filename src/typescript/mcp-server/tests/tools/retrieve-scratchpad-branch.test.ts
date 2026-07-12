/**
 * Scratchpad retrieve scrolls must NOT be branch-scoped (regression).
 *
 * Notes are branch-agnostic: the write path pins their queue branch to "main"
 * whatever the writer's checkout, so scoping a scratchpad scroll to the
 * session's feature branch (or its base-branch widening) silently empties the
 * read on any repo whose base branch is not literally "main". Projects
 * scrolls keep the branch scope — that gate is load-bearing (stale-chunk
 * leak, see the F-002/F-011 comments in retrieve.ts).
 */

import { describe, it, expect, beforeEach, vi } from 'vitest';
import { RetrieveTool } from '../../src/tools/retrieve.js';
import type { ProjectDetector } from '../../src/utils/project-detector.js';

const scrollMock = vi.hoisted(() => vi.fn());

vi.mock('@qdrant/js-client-rest', () => ({
  QdrantClient: vi.fn().mockImplementation(() => ({
    scroll: scrollMock,
    retrieve: vi.fn().mockResolvedValue([]),
  })),
}));

// Pin the session branch to a feature branch so a leaked branch condition is
// observable (real git detection would be environment-dependent here).
vi.mock('../../src/tools/branch-scope.js', async (importOriginal) => {
  const actual = await importOriginal<typeof import('../../src/tools/branch-scope.js')>();
  return {
    ...actual,
    resolveProjectIdentity: vi.fn().mockResolvedValue({
      projectId: 'tenant-1',
      projectPath: '/test/project',
    }),
    resolveEffectiveBranch: vi.fn().mockReturnValue('feature/x'),
    resolveFallbackBranch: vi.fn().mockReturnValue(undefined),
  };
});

function mockDetector(): ProjectDetector {
  return {
    getProjectInfo: vi.fn().mockResolvedValue({
      projectId: 'tenant-1',
      projectPath: '/test/project',
      name: 'test',
    }),
  } as unknown as ProjectDetector;
}

/** Filter of the first scroll call, serialized for condition asserts. */
function firstScrollFilterJson(): string {
  expect(scrollMock).toHaveBeenCalled();
  const req = scrollMock.mock.calls[0][1] as { filter?: unknown };
  return JSON.stringify(req.filter ?? {});
}

describe('RetrieveTool — scratchpad scrolls are branch-agnostic', () => {
  let tool: RetrieveTool;

  beforeEach(() => {
    vi.clearAllMocks();
    scrollMock.mockResolvedValue({ points: [] });
    tool = new RetrieveTool({ qdrantUrl: 'http://localhost:6333' }, mockDetector());
  });

  it('scratchpad scroll filter carries the tenant but NO branch condition', async () => {
    const res = await tool.retrieve({ collection: 'scratchpad', limit: 5 });

    expect(res.success).toBe(true);
    const json = firstScrollFilterJson();
    expect(json).toContain('tenant-1');
    expect(json).not.toContain('"branch"');
  });

  it('projects scroll keeps the branch scope (control)', async () => {
    await tool.retrieve({ collection: 'projects', limit: 5 });

    const json = firstScrollFilterJson();
    expect(json).toContain('"branch"');
    expect(json).toContain('feature/x');
  });
});
