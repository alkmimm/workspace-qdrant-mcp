/**
 * Tests for RetrieveTool — file-locator retrieval (filePath [+ lineNumber]).
 *
 * Regression guard: the locator must match EITHER the absolute `file_path` OR
 * the repo-relative `relative_path` payload field, so an agent can pass whichever
 * path form a search/list result surfaced. Matching only `file_path` made a
 * relative path silently scroll the whole tenant and return arbitrary documents.
 */

import { describe, it, expect, vi } from 'vitest';
import { RetrieveTool } from '../../src/tools/retrieve.js';
import type { ProjectDetector } from '../../src/utils/project-detector.js';

vi.mock('@qdrant/js-client-rest', () => ({
  QdrantClient: vi.fn(),
}));

function mockDetector(): ProjectDetector {
  return {
    findProjectRoot: vi.fn().mockReturnValue('/test/project'),
    getProjectInfo: vi.fn().mockResolvedValue({
      projectId: 'tenant-1',
      projectPath: '/test/project',
      name: 'test',
    }),
  } as unknown as ProjectDetector;
}

async function toolWithScroll(scrollMock: ReturnType<typeof vi.fn>): Promise<RetrieveTool> {
  const QdrantClientMock = await import('@qdrant/js-client-rest');
  vi.mocked(QdrantClientMock.QdrantClient).mockImplementation(
    () => ({ scroll: scrollMock }) as unknown as ReturnType<typeof QdrantClientMock.QdrantClient>
  );
  return new RetrieveTool({ qdrantUrl: 'http://localhost:6333' }, mockDetector());
}

describe('RetrieveTool - retrieve by file locator', () => {
  it('matches file_path OR relative_path for a filePath locator', async () => {
    const scrollMock = vi.fn().mockResolvedValue({ points: [] });
    const tool = await toolWithScroll(scrollMock);

    await tool.retrieve({ filePath: 'src/tools/search-filters.ts', projectId: 'tenant-1' });

    expect(scrollMock).toHaveBeenCalledWith(
      'projects',
      expect.objectContaining({
        filter: expect.objectContaining({
          must: expect.arrayContaining([
            expect.objectContaining({
              should: expect.arrayContaining([
                { key: 'file_path', match: { value: 'src/tools/search-filters.ts' } },
                { key: 'relative_path', match: { value: 'src/tools/search-filters.ts' } },
              ]),
            }),
          ]),
        }),
      })
    );
  });

  it('keeps the tenant filter alongside the path locator', async () => {
    const scrollMock = vi.fn().mockResolvedValue({ points: [] });
    const tool = await toolWithScroll(scrollMock);

    await tool.retrieve({ filePath: '/abs/path/a.ts', projectId: 'tenant-1' });

    expect(scrollMock).toHaveBeenCalledWith(
      'projects',
      expect.objectContaining({
        filter: expect.objectContaining({
          must: expect.arrayContaining([
            expect.objectContaining({ key: 'tenant_id', match: { value: 'tenant-1' } }),
          ]),
        }),
      })
    );
  });
});
