/**
 * MCP `outputSchema` fragments for the read tools (P1.4).
 *
 * Declared (centrally, in getToolDefinitions) on search/grep/list/retrieve/graph
 * so a client can validate and route the `structuredContent` the dispatcher
 * mirrors alongside the TextContent fallback.
 *
 * IMPORTANT — kept PERMISSIVE (`additionalProperties: true`, minimal/no
 * `required`): the runtime responses carry fields beyond the load-bearing ones
 * (e.g. `health` from healthMonitor.augmentSearchResults, plus `status` /
 * `status_reason` / `indexing` on search), and some MCP clients REJECT a
 * `structuredContent` that fails to validate against a closed declared schema.
 * Permissive schemas advertise the useful shape without that rejection risk.
 */

/** One search hit — mirrors SearchResult in src/tools/search-types.ts. */
const SEARCH_HIT_SCHEMA = {
  type: 'object' as const,
  additionalProperties: true,
  properties: {
    id: { type: 'string', description: 'Qdrant point id — pass to retrieve(documentId=...)' },
    score: { type: 'number', description: 'Pre-rerank similarity (same scale as scoreThreshold)' },
    rerankScore: { type: 'number', description: 'Per-query blended rerank rank [0,1]; absent when rerank off' },
    collection: { type: 'string' },
    location: { type: 'string', description: 'grep-like relative_path:line locator; absent when no path' },
    content: { type: 'string', description: 'Chunk body (may be truncated; empty in summary mode)' },
    title: { type: 'string' },
    metadata: { type: 'object', additionalProperties: true },
  },
};

export const SEARCH_OUTPUT_SCHEMA = {
  type: 'object' as const,
  additionalProperties: true,
  properties: {
    results: { type: 'array', items: SEARCH_HIT_SCHEMA },
    total: { type: 'number' },
    query: { type: 'string' },
    mode: { type: 'string' },
    scope: { type: 'string' },
    collections_searched: { type: 'array', items: { type: 'string' } },
    hint: { type: 'string', description: 'In-band next-tool hint (graph), present only for symbol hits' },
    budget_truncated: {
      type: 'object',
      additionalProperties: true,
      description: 'Present when the response byte budget dropped trailing hits: { dropped: N }',
    },
  },
};

export const GREP_OUTPUT_SCHEMA = {
  type: 'object' as const,
  additionalProperties: true,
  properties: {
    success: { type: 'boolean' },
    matches: { type: 'array', items: { type: 'object', additionalProperties: true } },
    total_matches: { type: 'number' },
    truncated: { type: 'boolean' },
    latency_ms: { type: 'number' },
  },
};

export const LIST_OUTPUT_SCHEMA = {
  type: 'object' as const,
  additionalProperties: true,
  properties: {
    success: { type: 'boolean' },
    format: { type: 'string' },
    // `listing` shape varies by format (tree/flat/summary) — left unconstrained.
    listing: {},
    stats: { type: 'object', additionalProperties: true },
  },
};

export const RETRIEVE_OUTPUT_SCHEMA = {
  type: 'object' as const,
  additionalProperties: true,
  properties: {
    success: { type: 'boolean' },
    documents: { type: 'array', items: { type: 'object', additionalProperties: true } },
    total: { type: 'number' },
    hasMore: { type: 'boolean' },
    hint: { type: 'string' },
  },
};

export const GRAPH_OUTPUT_SCHEMA = {
  type: 'object' as const,
  additionalProperties: true,
  properties: {
    success: { type: 'boolean' },
    action: { type: 'string' },
  },
};
