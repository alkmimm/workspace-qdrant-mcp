/**
 * `search` carries the read-side project echo on tenant-addressed reads only:
 * project-scoped searches of the projects/scratchpad collections name the
 * project they answered from; libraries/rules reads (not tenant-addressed, and
 * excluded by retrieve for the same reason) do not.
 */
import { describe, it, expect, vi } from 'vitest';
import { SearchTool } from '../../src/tools/search.js';
import type { DaemonClient } from '../../src/clients/daemon-client.js';
import type { SqliteStateManager } from '../../src/clients/sqlite-state-manager.js';
import type { ProjectDetector } from '../../src/utils/project-detector.js';
import { runWithRequestContext } from '../../src/utils/request-context.js';

vi.mock('@qdrant/js-client-rest', () => ({
  QdrantClient: vi.fn().mockImplementation(() => ({
    search: vi.fn().mockResolvedValue([]),
    scroll: vi.fn().mockResolvedValue({ points: [] }),
    retrieve: vi.fn().mockResolvedValue([]),
    getCollection: vi.fn().mockResolvedValue({ status: 'green' }),
  })),
}));

function daemonClient(): DaemonClient {
  return {
    connect: vi.fn().mockResolvedValue(undefined),
    close: vi.fn(),
    isConnected: vi.fn().mockReturnValue(true),
    getConnectionState: vi.fn().mockReturnValue({ connected: true }),
    healthCheck: vi.fn().mockResolvedValue({ status: 1 }),
    getStatus: vi.fn().mockResolvedValue({}),
    getMetrics: vi.fn().mockResolvedValue({}),
    embedText: vi.fn().mockResolvedValue({
      embedding: new Array(384).fill(0).map((_, i) => i / 384),
      dimensions: 384,
      model_name: 'all-MiniLM-L6-v2',
      success: true,
    }),
    generateSparseVector: vi.fn().mockResolvedValue({
      indices_values: { 1: 0.5, 2: 0.3 },
      vocab_size: 1000,
      success: true,
    }),
  } as unknown as DaemonClient;
}

function stateManager(): SqliteStateManager {
  return {
    initialize: vi.fn().mockReturnValue({ status: 'ok' }),
    close: vi.fn(),
    getProjectByPath: vi.fn().mockResolvedValue(null),
    listProjects: vi.fn().mockResolvedValue([]),
    logSearchEvent: vi.fn(),
    updateSearchEvent: vi.fn(),
    updateSearchEventEconomy: vi.fn(),
    getMatchingTags: vi.fn().mockReturnValue([]),
    getKeywordBasketsForTags: vi.fn().mockReturnValue([]),
    listTags: vi.fn().mockReturnValue([]),
    getTagHierarchy: vi.fn().mockReturnValue([]),
    getWatchFolderIdByTenantId: vi.fn().mockReturnValue(null),
    getActiveBasePoints: vi.fn().mockReturnValue([]),
  } as unknown as SqliteStateManager;
}

const detector = {
  findProjectRoot: vi.fn().mockReturnValue('/test/project'),
  getProjectInfo: vi.fn().mockResolvedValue({
    projectId: 'test-project-123',
    projectPath: '/test/project',
    name: 'test-project',
  }),
} as unknown as ProjectDetector;

function tool(): SearchTool {
  return new SearchTool(
    { qdrantUrl: 'http://localhost:6333' },
    daemonClient(),
    stateManager(),
    detector
  );
}

describe('search read-side project echo', () => {
  it('names the cwd-resolved project on a project-scoped search', async () => {
    const res = await tool().search({ query: 'test' });
    expect(res.project_id).toBe('test-project-123');
    expect(res.project_path).toBe('/test/project');
    expect(res.project_source).toBe('cwd');
  });

  it('labels a sticky cwd as sticky-cwd', async () => {
    const res = await runWithRequestContext({ hostCwd: '/test/project', cwdSource: 'sticky' }, () =>
      tool().search({ query: 'test' })
    );
    expect(res.project_source).toBe('sticky-cwd');
  });

  it('omits the echo for libraries/rules reads (not tenant-addressed; parity with retrieve)', async () => {
    const res = await tool().search({
      query: 'test',
      collection: 'libraries',
      libraryName: 'tokio',
    });
    expect(res.project_id).toBeUndefined();
    expect(res.project_source).toBeUndefined();
  });

  it('omits the echo on a cross-tenant sweep (scope all)', async () => {
    const res = await tool().search({ query: 'test', scope: 'all' });
    expect(res.project_id).toBeUndefined();
  });
});
