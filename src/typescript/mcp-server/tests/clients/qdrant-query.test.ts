import { describe, expect, it, vi } from 'vitest';
import type { QdrantClient } from '@qdrant/js-client-rest';
import { queryPoints } from '../../src/clients/qdrant-query.js';

describe('queryPoints', () => {
  it('uses the Query API and unwraps points for a named vector', async () => {
    const points = [{ id: 'p1', score: 0.9, payload: { path: 'a.ts' } }];
    const query = vi.fn().mockResolvedValue({ points });
    const client = { query } as unknown as QdrantClient;

    await expect(
      queryPoints(client, 'projects', {
        vector: { name: 'dense', vector: [0.1, 0.2] },
        limit: 10,
        score_threshold: 0.3,
        with_payload: true,
      })
    ).resolves.toEqual(points);
    expect(query).toHaveBeenCalledWith('projects', {
      query: [0.1, 0.2],
      using: 'dense',
      limit: 10,
      score_threshold: 0.3,
      with_payload: true,
    });
  });

  it('preserves a sparse vector and exact-search parameters', async () => {
    const query = vi.fn().mockResolvedValue({ points: [] });
    const client = { query } as unknown as QdrantClient;
    const sparse = { indices: [1, 4], values: [0.8, 0.2] };

    await queryPoints(client, 'projects', {
      vector: { name: 'sparse', vector: sparse },
      limit: 5,
      params: { exact: true },
    });
    expect(query).toHaveBeenCalledWith('projects', {
      query: sparse,
      using: 'sparse',
      limit: 5,
      params: { exact: true },
    });
  });

  it('uses legacy search only when query is unavailable', async () => {
    const search = vi.fn().mockResolvedValue([{ id: 1, score: 1 }]);
    const client = { search } as unknown as QdrantClient;
    const request = { vector: [0.1], limit: 1 };
    await queryPoints(client, 'rules', request);
    expect(search).toHaveBeenCalledWith('rules', request);
  });
});
