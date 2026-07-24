/**
 * MCP tool schema definition for the 'rules' tool
 */

export const rulesToolDefinition = {
  name: 'rules',
  annotations: {
    title: 'Behavioral rules (list/add/update/remove)',
    openWorldHint: false,
    destructiveHint: true, // 'remove' deletes a rule
    idempotentHint: false, // 'add' of a new label changes state
  },
  description:
    "Manage behavioral rules (add, update, remove, list). Check active rules at the start of each session to load the user's behavioral preferences. Rules persist across sessions and guide how you should work. Required args per action: add/update → label + content; remove → label; list → none.",
  inputSchema: {
    type: 'object' as const,
    properties: {
      action: {
        type: 'string',
        enum: ['add', 'update', 'remove', 'list'],
        description: 'Action to perform',
      },
      content: {
        type: 'string',
        description: 'Rule content (required for add/update)',
      },
      label: {
        type: 'string',
        description:
          'Rule label (max 15 chars, format: word-word-word, e.g., "prefer-uv", "use-pytest"). Required for add/update/remove.',
      },
      scope: {
        type: 'string',
        enum: ['global', 'project'],
        description:
          'Rule scope (default: project). "project" ties the rule to the current project (resolved from cwd/projectId); "global" applies it across all projects. Pass scope:"global" explicitly for cross-project rules.',
      },
      projectId: {
        type: 'string',
        description: 'Project ID for project-scoped rules',
      },
      cwd: {
        type: 'string',
        description:
          'Absolute path of your current working directory. Pass this so the server can auto-detect the project over HTTP (it cannot otherwise observe your location). Ignored when projectId is provided.',
      },
      title: {
        type: 'string',
        description: 'Rule title (max 50 chars)',
      },
      tags: {
        type: 'array',
        items: { type: 'string' },
        description: 'Tags for categorization (max 5 tags, max 20 chars each)',
      },
      priority: {
        type: 'number',
        description: 'Rule priority (higher = more important)',
      },
      limit: {
        type: 'number',
        description: 'Max rules to return for list (default: 50)',
      },
      summary: {
        type: 'boolean',
        description:
          'For list: return compact summaries (label/title/scope/owner/priority/tags + a content preview and content_length) instead of full rule bodies. Default true — the whole rule set stays cheap to load at session start. Pass summary:false for full text (still byte-budgeted with a cursor), or read one rule with retrieve (collection:"rules", documentId:<id>).',
      },
      maxResponseBytes: {
        type: 'number',
        description:
          'For list: cap on the rendered rules payload (default ~24000, the same budget as search/grep/scratchpad). Trailing rules beyond it are dropped (>=1 kept), budget_truncated.dropped reports how many, and next_cursor resumes at the first dropped rule — lossless. 0 disables.',
      },
      cursor: {
        type: 'string',
        description:
          'For list: opaque pagination cursor — pass the next_cursor from a previous list response to fetch the next page.',
      },
      includeGlobal: {
        type: 'boolean',
        description:
          'For list with the default scope:"project": also return the global rules, which apply to every project. Default true — a project with no rules of its own would otherwise report zero while global rules exist. Each rule\'s "owner" field says which it is (the project tenant_id, or "global"). Pass false for this project\'s rules only.',
      },
    },
    required: ['action'],
  },
};
