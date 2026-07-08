/**
 * Regression tests for retrieve branch scoping.
 *
 * Before the fix, `retrieve`'s scroll paths (document_id fallback, path
 * locator, metadata filter) applied only a tenant filter — never a branch
 * filter — so retrieving a document by its `document_id` returned chunks from
 * EVERY branch that shares the tenant + document_id, mixing stale feature-branch
 * content into the result. These tests pin the branch clause, the base-branch
 * widening, the auto-widen-on-empty fallback, and the `branch:"*"` opt-out.
 */

import { describe, it, expect, beforeEach, vi } from 'vitest';
import { RetrieveTool } from '../../src/tools/retrieve.js';
import type { ProjectDetector } from '../../src/utils/project-detector.js';
import type { SqliteStateManager } from '../../src/clients/sqlite-state-manager.js';

// Default mock — individual tests override the scroll implementation via
// mockImplementationOnce so they can assert on the exact request sent.
vi.mock('@qdrant/js-client-rest', () => ({
  QdrantClient: vi.fn().mockImplementation(() => ({
    retrieve: vi.fn().mockResolvedValue([]),
    scroll: vi.fn().mockResolvedValue({ points: [] }),
  })),
}));

const TENANT = 'test-project-123';

function createMockProjectDetector(): ProjectDetector {
  return {
    findProjectRoot: vi.fn().mockReturnValue('/test/project'),
    getProjectInfo: vi.fn().mockResolvedValue({
      projectId: TENANT,
      projectPath: '/test/project',
      name: 'test-project',
    }),
  } as unknown as ProjectDetector;
}

/** State manager stub that maps the tenant to a base branch of `main`. */
function createMockStateManager(baseBranch: string | null = 'main'): SqliteStateManager {
  return {
    getWatchFolderIdByTenantId: vi.fn().mockReturnValue('wf-1'),
    getBaseBranch: vi.fn().mockReturnValue(baseBranch),
  } as unknown as SqliteStateManager;
}

async function toolWithScroll(
  scrollFn: ReturnType<typeof vi.fn>,
  detector: ProjectDetector,
  stateManager?: SqliteStateManager
): Promise<RetrieveTool> {
  const QdrantClientMock = await import('@qdrant/js-client-rest');
  vi.mocked(QdrantClientMock.QdrantClient).mockImplementationOnce(
    () =>
      ({
        retrieve: vi.fn().mockResolvedValue([]),
        scroll: scrollFn,
      }) as unknown as ReturnType<typeof QdrantClientMock.QdrantClient>
  );
  return new RetrieveTool({ qdrantUrl: 'http://localhost:6333' }, detector, undefined, stateManager);
}

describe('RetrieveTool — branch scoping', () => {
  let detector: ProjectDetector;

  beforeEach(() => {
    vi.clearAllMocks();
    detector = createMockProjectDetector();
  });

  it('scopes the document_id fallback to the caller branch widened to the base branch', async () => {
    const scrollFn = vi.fn().mockResolvedValue({
      points: [
        {
          id: 'chunk-1',
          payload: {
            content: 'current content',
            tenant_id: TENANT,
            document_id: 'doc-abc',
            branch: ['feature-x'],
          },
        },
      ],
    });
    const tool = await toolWithScroll(scrollFn, detector, createMockStateManager('main'));

    const result = await tool.retrieve({
      documentId: 'doc-abc',
      collection: 'projects',
      projectId: TENANT,
      branch: 'feature-x',
    });

    expect(result.success).toBe(true);
    expect(scrollFn).toHaveBeenCalledTimes(1);
    const [, request] = scrollFn.mock.calls[0];
    const filter = (request as Record<string, unknown>).filter as { must: unknown[] };
    expect(filter.must).toEqual(
      expect.arrayContaining([
        { key: 'tenant_id', match: { value: TENANT } },
        { key: 'document_id', match: { value: 'doc-abc' } },
        {
          should: [
            { key: 'branch', match: { value: 'feature-x' } },
            { key: 'branch', match: { value: 'main' } },
          ],
        },
      ])
    );
  });

  it('auto-widens to all branches when the branch-scoped scroll returns nothing', async () => {
    const scrollFn = vi
      .fn()
      .mockResolvedValueOnce({ points: [] }) // branch-scoped attempt: empty
      .mockResolvedValueOnce({
        points: [
          {
            id: 'chunk-1',
            payload: {
              content: 'other-branch content',
              tenant_id: TENANT,
              document_id: 'doc-abc',
              branch: ['some-other-branch'],
            },
          },
        ],
      });
    const tool = await toolWithScroll(scrollFn, detector, createMockStateManager('main'));

    const result = await tool.retrieve({
      documentId: 'doc-abc',
      collection: 'projects',
      projectId: TENANT,
      branch: 'feature-x',
    });

    expect(scrollFn).toHaveBeenCalledTimes(2);
    // First attempt carries the branch clause; the widened retry drops it.
    expect(JSON.stringify(scrollFn.mock.calls[0][1])).toContain('feature-x');
    const widened = (scrollFn.mock.calls[1][1] as Record<string, unknown>).filter as {
      must: unknown[];
    };
    expect(JSON.stringify(widened)).not.toContain('feature-x');
    expect(JSON.stringify(widened)).not.toContain('"branch"');
    expect(result.success).toBe(true);
    expect(result.documents).toHaveLength(1);
    expect(result.documents[0].content).toBe('other-branch content');
  });

  it('branch:"*" reads across all branches (no branch clause, no widen retry)', async () => {
    const scrollFn = vi.fn().mockResolvedValue({
      points: [
        {
          id: 'chunk-1',
          payload: {
            content: 'any-branch content',
            tenant_id: TENANT,
            document_id: 'doc-abc',
            branch: ['stale-branch'],
          },
        },
      ],
    });
    const tool = await toolWithScroll(scrollFn, detector, createMockStateManager('main'));

    const result = await tool.retrieve({
      documentId: 'doc-abc',
      collection: 'projects',
      projectId: TENANT,
      branch: '*',
    });

    expect(scrollFn).toHaveBeenCalledTimes(1);
    const filter = (scrollFn.mock.calls[0][1] as Record<string, unknown>).filter as {
      must: unknown[];
    };
    expect(JSON.stringify(filter)).not.toContain('"branch"');
    expect(result.success).toBe(true);
    expect(result.documents).toHaveLength(1);
  });

  it('does not branch-scope the libraries collection (branch-agnostic)', async () => {
    const scrollFn = vi.fn().mockResolvedValue({
      points: [
        {
          id: 'lib-1',
          payload: { content: 'library doc', library_name: 'mylib' },
        },
      ],
    });
    const tool = await toolWithScroll(scrollFn, detector, createMockStateManager('main'));

    const result = await tool.retrieve({
      documentId: 'doc-abc',
      collection: 'libraries',
      libraryName: 'mylib',
      branch: 'feature-x',
    });

    expect(result.success).toBe(true);
    expect(scrollFn).toHaveBeenCalledTimes(1);
    const filter = (scrollFn.mock.calls[0][1] as Record<string, unknown>).filter as {
      must: unknown[];
    };
    expect(JSON.stringify(filter)).not.toContain('"branch"');
  });
});
