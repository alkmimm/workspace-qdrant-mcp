/**
 * Regression: the displayed `score` must stay the pre-boost similarity (raw
 * cosine for semantic), on the SAME scale as `scoreThreshold`.
 *
 * The path-relevance boost reorders results (a precisely-named file is promoted
 * over higher-cosine but path-irrelevant hits), but it must NOT inflate the
 * number the caller sees. Before this fix the boost mutated `score` in place, so
 * a ~0.55-cosine hit displayed ~0.99 after a ×1.8 boost — and a threshold set
 * against that visible 0.99 (e.g. 0.85) filtered on the raw cosine instead and
 * returned nothing. See finalizeResults in search-helpers.ts.
 */

import { describe, it, expect, vi } from 'vitest';
import { finalizeResults } from '../../src/tools/search-helpers.js';
import type { SearchResult } from '../../src/tools/search-types.js';
import type { QdrantClient } from '@qdrant/js-client-rest';
import type { DaemonClient } from '../../src/clients/daemon-client.js';
import type { SqliteStateManager } from '../../src/clients/sqlite-state-manager.js';

function hit(
  id: string,
  score: number,
  relativePath: string,
  metadata: Record<string, unknown> = {}
): SearchResult {
  return {
    id,
    score,
    collection: 'projects',
    content: 'irrelevant body',
    metadata: { relative_path: relativePath, _search_type: 'semantic', ...metadata },
  };
}

describe('finalizeResults — displayed score stays raw (threshold-aligned)', () => {
  it('orders by the path boost but returns the pre-boost similarity in `score`', async () => {
    const qdrant = {} as unknown as QdrantClient; // no parent/graph expansion
    const daemon = {} as unknown as DaemonClient; // rerank off by default
    const state = { updateSearchEvent: vi.fn() } as unknown as SqliteStateManager;

    // A: lower raw cosine but its path spells out every query token → boosted.
    // B: higher raw cosine but a path-irrelevant file → no boost.
    const response = await finalizeResults(qdrant, daemon, state, {
      allResults: [
        hit('A', 0.5, 'src/auth/auth_session_service.ts'),
        hit('B', 0.55, 'src/misc/unrelated.ts'),
      ],
      mode: 'semantic',
      limit: 10,
      options: { query: 'auth session service' },
      eventId: 'evt-1',
      searchStartMs: Date.now(),
      query: 'auth session service',
      scope: 'global', // skips the project-only indexing probe
      collectionsToSearch: ['projects'],
      status: 'ok',
      statusReason: undefined,
      currentProjectId: undefined,
    });

    // The boost promoted A above its higher-cosine neighbour B...
    expect(response.results.map((r) => r.id)).toEqual(['A', 'B']);
    // ...but the displayed scores are the raw cosines, NOT the boosted values
    // (A would show ~0.9 if the boost had leaked into `score`).
    expect(response.results[0]?.score).toBe(0.5);
    expect(response.results[1]?.score).toBe(0.55);
  });

  it('treats identifier queries as implementation intent so source can outrank tests', async () => {
    const qdrant = {} as unknown as QdrantClient;
    const daemon = {} as unknown as DaemonClient;
    const state = { updateSearchEvent: vi.fn() } as unknown as SqliteStateManager;

    const response = await finalizeResults(qdrant, daemon, state, {
      allResults: [
        hit('test', 0.53, 'tests/events/event_transform_engine.test.ts', { file_type: 'code' }),
        hit('impl', 0.5, 'src/events/event_transform_engine.ts', { file_type: 'code' }),
      ],
      mode: 'semantic',
      limit: 10,
      options: { query: 'EventTransformEngine behavior' },
      eventId: 'evt-2',
      searchStartMs: Date.now(),
      query: 'EventTransformEngine behavior',
      scope: 'global',
      collectionsToSearch: ['projects'],
      status: 'ok',
      statusReason: undefined,
      currentProjectId: undefined,
    });

    expect(response.results.map((r) => r.id)).toEqual(['impl', 'test']);
    expect(response.results[0]?.score).toBe(0.5);
    expect(response.results[1]?.score).toBe(0.53);
  });
});
