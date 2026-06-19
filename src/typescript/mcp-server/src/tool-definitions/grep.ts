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
      scope: {
        type: 'string',
        enum: ['project', 'all'],
        description: 'Search scope: project (current) or all (default: project)',
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
