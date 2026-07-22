/**
 * Regression: a project-scoped SEMANTIC search whose tenant cannot be resolved
 * MUST refuse — not run an unfiltered query. `buildProjectCondition` drops the
 * `tenant_id` filter on an undefined projectId, so the pre-fix main vector
 * pipeline searched every tenant and silently returned another repo's code.
 * (The scroll fallback already refused via F-001; the vector path was the gap.)
 *
 * The decisive assertion is that `embedText` is NOT called: the guard short-
 * circuits before `prepareEmbeddings`, so no embedding is generated and no
 * Qdrant query runs.
 */

import { describe, it, expect, beforeEach, vi } from 'vitest';
import { SearchTool, type SearchOptions } from '../../src/tools/search.js';
import type { DaemonClient } from '../../src/clients/daemon-client.js';
import type { SqliteStateManager } from '../../src/clients/sqlite-state-manager.js';
import type { ProjectDetector } from '../../src/utils/project-detector.js';

vi.mock('@qdrant/js-client-rest', () => ({
  QdrantClient: vi.fn().mockImplementation(() => ({
    search: vi.fn().mockResolvedValue([]),
    scroll: vi.fn().mockResolvedValue({ points: [] }),
    retrieve: vi.fn().mockResolvedValue([]),
    getCollection: vi.fn().mockResolvedValue({ status: 'green' }),
  })),
}));

function createMockDaemonClient(): DaemonClient {
  return {
    isConnected: vi.fn().mockReturnValue(true),
    embedText: vi.fn().mockResolvedValue({
      embedding: new Array(384).fill(0).map((_, i) => i / 384),
      dimensions: 384,
      model_name: 'all-MiniLM-L6-v2',
      success: true,
    }),
    generateSparseVector: vi.fn().mockResolvedValue({
      indices_values: { 1: 0.5 },
      vocab_size: 1000,
      success: true,
    }),
  } as unknown as DaemonClient;
}

function createMockStateManager(): SqliteStateManager {
  return {
    initialize: vi.fn().mockReturnValue({ status: 'ok' }),
    logSearchEvent: vi.fn(),
    updateSearchEvent: vi.fn(),
    updateSearchEventEconomy: vi.fn(),
    getMatchingTags: vi.fn().mockReturnValue([]),
    getKeywordBasketsForTags: vi.fn().mockReturnValue([]),
    getWatchFolderIdByTenantId: vi.fn().mockReturnValue(null),
    getProjectById: vi.fn().mockReturnValue({ data: null }),
    getActiveBasePoints: vi.fn().mockReturnValue([]),
    countCloneInstancesByTenantId: vi.fn().mockReturnValue(0),
  } as unknown as SqliteStateManager;
}

/** Detector that resolves NO project — the reconnect / missing-cwd case. */
function createUnresolvableProjectDetector(): ProjectDetector {
  return {
    findProjectRoot: vi.fn().mockReturnValue(null),
    getProjectInfo: vi.fn().mockResolvedValue(null),
  } as unknown as ProjectDetector;
}

describe('SearchTool — unresolved project tenant refusal', () => {
  let searchTool: SearchTool;
  let daemon: DaemonClient;
  let detector: ProjectDetector;

  beforeEach(() => {
    vi.clearAllMocks();
    daemon = createMockDaemonClient();
    detector = createUnresolvableProjectDetector();
    searchTool = new SearchTool(
      { qdrantUrl: 'http://localhost:6333', qdrantTimeout: 5000 },
      daemon,
      createMockStateManager(),
      detector
    );
  });

  it('refuses a project-scoped search when the tenant cannot be resolved', async () => {
    const options: SearchOptions = { query: 'shift trade eligible users', scope: 'project' };
    const result = await searchTool.search(options);

    expect(result.results).toEqual([]);
    expect(result.total).toBe(0);
    expect(result.status).toBe('uncertain');
    expect(result.status_reason).toMatch(/no project could be resolved/i);
    expect(result.status_reason).toMatch(/cross tenant|scope: ?"all"/i);
    expect(result.hint).toBeDefined();

    // The point of the fix: it must NOT reach embedding generation / the query.
    expect(detector.getProjectInfo).toHaveBeenCalled();
    expect(daemon.embedText).not.toHaveBeenCalled();
    expect(daemon.generateSparseVector).not.toHaveBeenCalled();
  });

  it('defaults to project scope and therefore also refuses on an unresolved tenant', async () => {
    const result = await searchTool.search({ query: 'anything' });
    expect(result.scope).toBe('project');
    expect(result.status).toBe('uncertain');
    expect(daemon.embedText).not.toHaveBeenCalled();
  });

  it('does NOT refuse scope:"all" — explicit cross-tenant search is intended', async () => {
    const result = await searchTool.search({ query: 'test query', scope: 'all' });
    // scope:"all" carries no tenant filter by design; it must proceed (embed).
    expect(daemon.embedText).toHaveBeenCalled();
    expect(result.status_reason ?? '').not.toMatch(/no project could be resolved/i);
  });
});
