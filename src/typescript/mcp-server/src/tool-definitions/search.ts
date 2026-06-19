/**
 * MCP tool schema definition for the 'search' tool
 */

import {
  COLLECTION_PROJECTS,
  COLLECTION_LIBRARIES,
  COLLECTION_RULES,
  COLLECTION_SCRATCHPAD,
} from '../common/native-bridge.js';

export const searchToolDefinition = {
  name: 'search',
  description:
    "Semantic + keyword search over the user's indexed code, libraries, and saved notes — your PRIMARY way to answer questions about this project's code, architecture, or docs. Call this FIRST: it searches the actual indexed codebase (more accurate than training data) and finds code by MEANING, which a literal file grep cannot. Default mode is \"semantic\" (the strongest mode here). Write queries in English; when you want the implementation (not docs or tests) add fileType:\"code\" or a pathGlob. For a known identifier or exact string, use the `grep` tool instead.",
  inputSchema: {
    type: 'object' as const,
    properties: {
      query: {
        type: 'string',
        description:
          'The search query text. Write it in ENGLISH regardless of the conversation language — the embedding model is multilingual, but code is overwhelmingly English and cross-lingual recall for code is weak, so a non-English query matches same-language prose/docs instead of code and recall collapses. Prefer wording close to the likely identifiers and comments (e.g. "recover stale queue leases", not a loose paraphrase).',
      },
      collection: {
        type: 'string',
        enum: [COLLECTION_PROJECTS, COLLECTION_LIBRARIES, COLLECTION_RULES, COLLECTION_SCRATCHPAD],
        description: 'Specific collection to search',
      },
      mode: {
        type: 'string',
        enum: ['semantic', 'hybrid'],
        description:
          '`semantic` (default) ranks by meaning via dense vectors — the strongest general mode and the right choice for concept / "how does X work" questions. `hybrid` adds a keyword (sparse BM25) leg fused with the dense one; it mainly helps queries centered on an exact identifier or symbol. For a literal token or substring prefer the `grep` tool or `exact:true`, not a search mode.',
      },
      scope: {
        type: 'string',
        enum: ['project', 'global', 'all'],
        description: 'Search scope: project (current), global, or all (default: project)',
      },
      limit: {
        type: 'number',
        description: 'Maximum results to return (default: 10)',
      },
      projectId: {
        type: 'string',
        description: 'Specific project ID to search',
      },
      cwd: {
        type: 'string',
        description:
          'Absolute path of your current working directory. Pass this so the server can auto-detect the project over HTTP (it cannot otherwise observe your location). Ignored when projectId is provided.',
      },
      libraryName: {
        type: 'string',
        description: 'Library name when searching libraries collection',
      },
      branch: {
        type: 'string',
        description: 'Filter by branch name',
      },
      fileType: {
        type: 'string',
        description:
          'Filter by content classification: "code", "text", "config", "data", "docs", "web", "slides", "build". Prose documentation and Markdown are classified "text"; "docs" is for binary document formats (PDF, Office), so to bias toward project docs use "text", not "docs". Use "code" when seeking an implementation so documentation and test-adjacent files do not crowd out source files.',
      },
      scoreThreshold: {
        type: 'number',
        description:
          'Minimum similarity score threshold (0-1, default: 0.3). Applied at the vector-store stage on the raw cosine similarity, before any reranking. In each result, `score` is that pre-rerank similarity (comparable across queries, same scale as this threshold); when the cross-encoder reranker is active, results also carry a `rerankScore` (per-query blended rank, 0-1) that reflects ordering. Compare this threshold against `score`, not `rerankScore`.',
      },
      includeLibraries: {
        type: 'boolean',
        description: 'Include libraries in search (default: false)',
      },
      includeScratchpad: {
        type: 'boolean',
        description:
          'Append a small, tenant-filtered scratchpad recall lane to project-scoped searches so project notes/snippets surface automatically (labeled collection:"scratchpad", capped, never displacing code hits). Default: true for scope="project"; ignored for global/all or when an explicit collection is set.',
      },
      tag: {
        type: 'string',
        description: 'Filter results by concept tag (exact match)',
      },
      tags: {
        type: 'array',
        items: { type: 'string' },
        description: 'Filter results by multiple concept tags (OR logic)',
      },
      pathGlob: {
        type: 'string',
        description: 'File path glob filter (e.g., "**/*.rs", "src/**/*.ts")',
      },
      component: {
        type: 'string',
        description:
          'Filter by project component (e.g., "daemon", "daemon.core"). Supports prefix matching.',
      },
      exact: {
        type: 'boolean',
        description: 'Use exact substring search instead of semantic search (default: false)',
      },
      contextLines: {
        type: 'number',
        description: 'Lines of context before/after matches in exact mode (default: 0)',
      },
      includeGraphContext: {
        type: 'boolean',
        description:
          'Include code relationship graph context (callers/callees) for matched symbols (default: false)',
      },
      maxBytesPerHit: {
        type: 'number',
        description:
          'Per-hit text cap in characters (default: 1500). Hits with content longer than this are truncated with a marker pointing to retrieve() for the full chunk body. Set to 0 to disable truncation.',
      },
      summary: {
        type: 'boolean',
        description:
          'When true, drop chunk text bodies and return only metadata (id, score, collection, title, path/symbol). Use for pure discovery before a follow-up retrieve() call. Default: false.',
      },
    },
    required: ['query'],
  },
};
