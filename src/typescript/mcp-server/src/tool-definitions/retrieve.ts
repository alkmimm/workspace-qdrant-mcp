/**
 * MCP tool schema definition for the 'retrieve' tool
 */

import {
  COLLECTION_PROJECTS,
  COLLECTION_LIBRARIES,
  COLLECTION_RULES,
  COLLECTION_SCRATCHPAD,
} from '../common/native-bridge.js';

export const retrieveToolDefinition = {
  name: 'retrieve',
  annotations: {
    title: 'Retrieve indexed document by id or locator',
    readOnlyHint: true,
    openWorldHint: false,
  },
  description:
    'Retrieve documents by their point id, by an exact-search file locator, or by a metadata filter. Pass `documentId` = the `id` field from a search/list result (NOT the metadata `document_id`). For exact-search hits, pass `filePath` + `lineNumber` from the result metadata. To look up by the `document_id` metadata field instead, use `filter: {"document_id": "..."}`; the tool will also try that filter automatically if the point id lookup misses. Prefer `search` for discovery, `retrieve` for known points.',
  inputSchema: {
    type: 'object' as const,
    properties: {
      documentId: {
        type: 'string',
        description:
          'The point id to retrieve — the `id` field from a search or list result (a Qdrant point UUID). NOT the metadata `document_id` (a content hash); to match that, use `filter: {"document_id": "..."}` instead. The tool will also try that filter automatically when the point id lookup misses.',
      },
      filePath: {
        type: 'string',
        description:
          'Exact-search file locator — accepts either the absolute `file_path` or the repo-relative `relative_path` from a result. Use together with `lineNumber` to retrieve the chunk covering that line. Prefer this for line-scoped exact-search hits instead of overloading `documentId`.',
      },
      lineNumber: {
        type: 'number',
        description: '1-based line number for an exact-search hit. Use together with `filePath`.',
      },
      collection: {
        type: 'string',
        enum: [COLLECTION_PROJECTS, COLLECTION_LIBRARIES, COLLECTION_RULES, COLLECTION_SCRATCHPAD],
        description: 'Collection to retrieve from (default: projects)',
      },
      filter: {
        type: 'object',
        additionalProperties: { type: 'string' },
        description: 'Metadata filter key-value pairs',
      },
      limit: {
        type: 'number',
        description: 'Maximum results (default: 10)',
      },
      offset: {
        type: 'number',
        description: 'Pagination offset (default: 0)',
      },
      projectId: {
        type: 'string',
        description: 'Project ID for projects collection',
      },
      cwd: {
        type: 'string',
        description:
          'Absolute path of your current working directory. Pass this so the server can auto-detect the project over HTTP (it cannot otherwise observe your location). The response echoes project_id + project_source (cwd | sticky-cwd | projectId): check they name the repo you meant — a cwd-less call may ride the session sticky cwd and answer from the previous project. Ignored when projectId is provided.',
      },
      libraryName: {
        type: 'string',
        description: 'Library name for libraries collection',
      },
      branch: {
        type: 'string',
        description:
          'Branch to scope projects-collection results to (default: your current Git branch, widened to the base branch for files unchanged on a feature branch). Pass "*" to retrieve across all branches — use this only when you deliberately want stale/other-branch versions. Ignored for scratchpad/libraries/rules: those collections are branch-agnostic.',
      },
    },
  },
};

/**
 * Canonical retrieve argument names, derived from the input schema so the
 * builder's unknown-argument check can never drift from the published schema.
 */
export const RETRIEVE_ARG_KEYS = Object.keys(retrieveToolDefinition.inputSchema.properties);
