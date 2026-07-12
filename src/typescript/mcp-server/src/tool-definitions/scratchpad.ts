/**
 * MCP tool schema definition for the 'scratchpad' tool.
 */

export const scratchpadToolDefinition = {
  name: 'scratchpad',
  annotations: {
    title: 'Manage scratchpad notes (list/update/delete)',
    openWorldHint: false,
    destructiveHint: true, // 'delete' removes a note
    idempotentHint: false, // 'update' replaces note content in place
  },
  description:
    'Manage existing scratchpad notes: list, update, or delete. Create notes with store(type:"scratchpad"). Notes are project-scoped — pass projectId (the tenant_id seen in a search/list result) to target a specific project, or cwd to auto-detect it. update/delete identify a note by its point `id` (from `scratchpad list` or a search hit — the easy path) OR by its CURRENT content, which must match VERBATIM — get it from `scratchpad list` (returns full, untruncated content), NOT from a `search` hit (whose content may be truncated). Ids are content-derived: an update changes the note\'s id, so re-list before chaining mutations. If no entry matches, the op fails with a clear error instead of silently doing nothing. Required args per action: update → (id or content) + newContent; delete → id or content; list → none. Boundary: to CREATE a note use store(type:"scratchpad"); this tool never creates. Examples — list: {action:"list"}; delete: {action:"delete", id:"<point id>"}; update: {action:"update", id:"<point id>", newContent:"<new>"}.',
  inputSchema: {
    type: 'object' as const,
    properties: {
      action: {
        type: 'string',
        enum: ['list', 'update', 'delete'],
        description: 'Action to perform: list entries, update one, or delete one.',
      },
      id: {
        type: 'string',
        description:
          'For update/delete: the point id of the note to target (the `id` from `scratchpad list` or a search hit). Alternative to `content`. Ids are content-derived — a prior update changes the note\'s id.',
      },
      content: {
        type: 'string',
        description:
          'For update/delete: the CURRENT text of the note to target (its identity). Must match VERBATIM — get it from `scratchpad list` (full content), not a `search` hit (may be truncated). Prefer `id` when you have it.',
      },
      newContent: {
        type: 'string',
        description: 'For update: the replacement text.',
      },
      title: {
        type: 'string',
        description: 'For update: the new title (optional).',
      },
      tags: {
        type: 'array',
        items: { type: 'string' },
        description: 'For update: the new tags (optional).',
      },
      projectId: {
        type: 'string',
        description:
          'Tenant the note belongs to (takes precedence over cwd). Pass the project_id / tenant_id seen in a search or list result; use "global" for notes not tied to a project.',
      },
      cwd: {
        type: 'string',
        description:
          'Absolute path of your current working directory — used to auto-detect the project when projectId is omitted (the server cannot otherwise observe it over HTTP).',
      },
      limit: {
        type: 'number',
        description: 'For list: maximum entries to return (default: 50).',
      },
    },
    required: ['action'],
  },
};
