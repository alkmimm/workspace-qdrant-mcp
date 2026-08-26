/**
 * MCP tool schema definition for the 'help' tool.
 *
 * On-demand topical manual (progressive disclosure, issue #357): the always-on
 * server instructions carry only a short behavioral kernel, and the detailed
 * chapters live behind this tool. Keep the topic list in the description in
 * sync with HELP_TOPICS (tools/help-topics.ts) — pinned by tests/tools/help.test.ts.
 */

export const helpToolDefinition = {
  name: 'help',
  annotations: {
    title: 'On-demand usage manual',
    readOnlyHint: true,
    openWorldHint: false,
  },
  description:
    'Detailed usage manual for this server, served on demand instead of front-loaded into the session. Call with topic: "search" (query formulation, fileType, path filters), "exact" (grep/list/retrieve), "store", "scratchpad", "branches" (worktrees, agent branches, mutations), "graph", "collections", or "http" (cwd/project detection) for the full chapter; call without a topic for the index. Response hints and error messages may reference these topics.',
  inputSchema: {
    type: 'object' as const,
    properties: {
      topic: {
        type: 'string',
        description: 'Topic id from the index (e.g. "branches"). Omit to list all topics.',
      },
    },
  },
};
