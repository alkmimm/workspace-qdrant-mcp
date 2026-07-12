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
    'Manage existing scratchpad notes: list, update, or delete. Create notes with store(type:"scratchpad"). Notes are project-scoped — pass projectId (the tenant_id seen in a search/list result) to target a specific project, or cwd to auto-detect it. `list` returns SUMMARY entries by default (id, title, tags, timestamps, content_length + a short preview) under the shared response byte budget (maxResponseBytes, default ~24k) with cursor pagination — pass the response\'s next_cursor back as cursor for the next page. For one note\'s FULL text use retrieve (collection:"scratchpad", documentId:<point id>) or pass summary:false (budget still applies); to FIND notes by content use search (collection:"scratchpad", optionally exact:true) — both are token-capped. update/delete identify a note by its point `id` (from `scratchpad list` or a search hit — the easy path) OR by its CURRENT content, which must match VERBATIM — fetch it via retrieve by point id or `list` with summary:false, NOT from a `search` hit (whose content may be truncated). Ids are content-derived: an update changes the note\'s id, so re-list before chaining mutations. If no entry matches, the op fails with a clear error instead of silently doing nothing. Required args per action: update → (id or content) + newContent; delete → id or content; list → none. Boundary: to CREATE a note use store(type:"scratchpad"); this tool never creates. Examples — list: {action:"list"}; delete: {action:"delete", id:"<point id>"}; update: {action:"update", id:"<point id>", newContent:"<new>"}.',
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
          'For update/delete: the CURRENT text of the note to target (its identity). Must match VERBATIM — fetch it via retrieve by point id (collection:"scratchpad") or `scratchpad list` with summary:false, not a `search` hit (may be truncated). Prefer `id` when you have it.',
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
        description: 'For list: maximum entries to return per page (default: 50).',
      },
      summary: {
        type: 'boolean',
        description:
          'For list: return summary entries — id, title, tags, timestamps, content_length and a short preview — instead of full note bodies (default: true). Pass false for full bodies (the response byte budget still applies). For ONE note\'s full text prefer retrieve (collection:"scratchpad", documentId:<point id>).',
      },
      maxResponseBytes: {
        type: 'number',
        description:
          'For list: cap on total response chars (default ~24000, the same budget as search/grep). Entries beyond it are dropped (>=1 kept), budget_truncated.dropped reports how many, and next_cursor resumes at the first dropped entry. 0 disables.',
      },
      cursor: {
        type: 'string',
        description:
          'For list: opaque pagination cursor — pass the next_cursor from a previous list response to fetch the next page.',
      },
    },
    required: ['action'],
  },
};
