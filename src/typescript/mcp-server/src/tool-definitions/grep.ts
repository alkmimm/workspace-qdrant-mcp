/**
 * MCP tool schema definition for the 'grep' tool
 */

export const grepToolDefinition = {
  name: 'grep',
  annotations: {
    title: 'Exact/regex search over the project index',
    readOnlyHint: true,
    openWorldHint: false,
  },
  description:
    'Exact substring or regex search over the FTS5 trigram index the daemon builds across the whole indexed project — branch-aware and covering files you have not opened, complementing a native working-tree grep. Use it for a known literal: identifier, import path, env var, or error string. For concept/meaning queries use the search tool; for caller/impact relationships use the graph tool.',
  inputSchema: {
    type: 'object' as const,
    properties: {
      pattern: {
        type: 'string',
        description: 'Search pattern (exact substring or regex)',
      },
      regex: {
        type: 'boolean',
        description: 'Treat pattern as regex (default: false)',
      },
      caseSensitive: {
        type: 'boolean',
        description: 'Case-sensitive matching (default: true)',
      },
      pathGlob: {
        type: 'string',
        description: 'File path glob filter (e.g., "**/*.rs", "src/**/*.ts")',
      },
      pathExclude: {
        type: 'string',
        description:
          'File path glob to EXCLUDE from matches (hard filter, opposite of pathGlob). Floats: "old_project/**" drops that directory at the repo root AND at any nested depth. Use it to silence matches from a legacy/vendored tree.',
      },
      scope: {
        type: 'string',
        enum: ['project', 'all'],
        description:
          'Search scope. "project" (default) searches ONLY the current repo; on an empty result it does NOT fall back to other repos (it returns a hint suggesting scope:"all"). Pass "all" to search across every indexed repository — this crosses project/tenant boundaries and is opt-in.',
      },
      contextLines: {
        type: 'number',
        description: 'Lines of context before/after each match (default: 0)',
      },
      maxResults: {
        type: 'number',
        description: 'Maximum results to return (default: 1000)',
      },
      branch: {
        type: 'string',
        description: 'Filter by branch name',
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
    },
    required: ['pattern'],
  },
};
