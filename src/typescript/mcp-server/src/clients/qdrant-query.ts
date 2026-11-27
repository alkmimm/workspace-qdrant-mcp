import type { QdrantClient } from '@qdrant/js-client-rest';

type LegacyVector =
  | number[]
  | { indices: number[]; values: number[] }
  | { name: string; vector: number[] | { indices: number[]; values: number[] } };

export interface LegacySearchRequest {
  vector: LegacyVector;
  limit: number;
  score_threshold?: number;
  with_payload?: boolean;
  with_vector?: boolean;
  filter?: Record<string, unknown>;
  params?: { exact: boolean };
}

type QueryCapableClient = QdrantClient & {
  query?: (collection: string, request: Record<string, unknown>) => Promise<{ points: QdrantQueryPoint[] }>;
  search?: (collection: string, request: LegacySearchRequest) => Promise<QdrantQueryPoint[]>;
};

export interface QdrantQueryPoint {
  id: string | number;
  score: number;
  payload?: Record<string, unknown> | null;
}

/** Adapt the removed REST `search` request to Qdrant's universal Query API. */
export async function queryPoints(
  client: QdrantClient,
  collection: string,
  request: LegacySearchRequest
): Promise<QdrantQueryPoint[]> {
  const compatible = client as QueryCapableClient;
  if (typeof compatible.query === 'function') {
    const { vector, ...rest } = request;
    const named = typeof vector === 'object' && !Array.isArray(vector) && 'name' in vector && 'vector' in vector;
    const response = await compatible.query(collection, {
      ...rest,
      query: named ? vector.vector : vector,
      ...(named ? { using: vector.name } : {}),
    });
    return response.points;
  }
  // Rolling-upgrade/test-double compatibility. js-client 1.20 always takes query().
  if (typeof compatible.search === 'function') return compatible.search(collection, request);
  throw new TypeError('Qdrant client exposes neither query() nor legacy search()');
}
