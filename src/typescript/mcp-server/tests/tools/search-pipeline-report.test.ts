/**
 * `pipeline` — the search response must say which optional ranking stages
 * actually ran.
 *
 * Every optional stage fails OPEN: a dead or slow rerank sidecar hands back the
 * pre-rerank order, a translation timeout hands back the single-leg answer.
 * That is the right behaviour for an interactive session and a silent
 * confound for anything that compares calls: two byte-identical requests can
 * be served by different pipelines depending on machine load, and before this
 * field the only trace was a debug log line. `pipeline.rerank` is stamped by
 * finalizeResults for the leg it ranks; the orchestrator adds `translation`.
 */

import { describe, it, expect, vi, afterEach } from 'vitest';
import { finalizeResults } from '../../src/tools/search-helpers.js';
import type { SearchResult } from '../../src/tools/search-types.js';
import type { QdrantClient } from '@qdrant/js-client-rest';
import type { DaemonClient } from '../../src/clients/daemon-client.js';
import type { SqliteStateManager } from '../../src/clients/sqlite-state-manager.js';

function hit(id: string, score: number, relativePath: string): SearchResult {
  return {
    id,
    score,
    collection: 'projects',
    content: `body of ${id}`,
    metadata: { relative_path: relativePath, _search_type: 'semantic' },
  };
}

function baseParams(query = 'anything') {
  return {
    allResults: [hit('A', 0.6, 'src/a.ts'), hit('B', 0.5, 'src/b.ts')],
    mode: 'semantic' as const,
    limit: 10,
    options: { query },
    eventId: 'evt',
    searchStartMs: Date.now(),
    query,
    scope: 'global' as const,
    collectionsToSearch: ['projects'],
    status: 'ok' as const,
    statusReason: undefined,
    currentProjectId: undefined,
  };
}

const qdrant = {} as unknown as QdrantClient;
const state = { updateSearchEvent: vi.fn() } as unknown as SqliteStateManager;

describe('finalizeResults — pipeline.rerank reports what happened', () => {
  const savedRerank = process.env['WQM_SEARCH_RERANK'];
  afterEach(() => {
    if (savedRerank === undefined) delete process.env['WQM_SEARCH_RERANK'];
    else process.env['WQM_SEARCH_RERANK'] = savedRerank;
  });

  it("is 'failed' when rerank is on but the daemon call throws (fail-open, not silent)", async () => {
    process.env['WQM_SEARCH_RERANK'] = '1';
    // No `rerank` method at all — the shape of a dead sidecar / older daemon.
    const daemon = {} as unknown as DaemonClient;
    const response = await finalizeResults(qdrant, daemon, state, baseParams());
    expect(response.pipeline).toEqual({ rerank: 'failed' });
    // The pre-rerank order stands and nothing carries a rerankScore.
    expect(response.results.map((r) => r.id)).toEqual(['A', 'B']);
    expect(response.results.every((r) => r.rerankScore === undefined)).toBe(true);
  });

  it("is 'applied' when the reranker scored the pool", async () => {
    process.env['WQM_SEARCH_RERANK'] = '1';
    const daemon = {
      rerank: vi.fn().mockResolvedValue({
        success: true,
        results: [
          { index: 0, score: 0.2 },
          { index: 1, score: 0.9 },
        ],
      }),
    } as unknown as DaemonClient;
    const response = await finalizeResults(qdrant, daemon, state, baseParams());
    expect(response.pipeline).toEqual({ rerank: 'applied' });
    expect(response.results.every((r) => typeof r.rerankScore === 'number')).toBe(true);
  });

  it("is 'off' when the deployment disables rerank", async () => {
    process.env['WQM_SEARCH_RERANK'] = '0';
    const daemon = { rerank: vi.fn() } as unknown as DaemonClient;
    const response = await finalizeResults(qdrant, daemon, state, baseParams());
    expect(response.pipeline).toEqual({ rerank: 'off' });
    expect(daemon.rerank).not.toHaveBeenCalled();
  });

  it("is 'off' on a per-call rerank:false regardless of the deployment default", async () => {
    process.env['WQM_SEARCH_RERANK'] = '1';
    const daemon = { rerank: vi.fn() } as unknown as DaemonClient;
    const response = await finalizeResults(qdrant, daemon, state, {
      ...baseParams(),
      options: { query: 'anything', rerank: false },
    });
    expect(response.pipeline).toEqual({ rerank: 'off' });
    expect(daemon.rerank).not.toHaveBeenCalled();
  });
});
