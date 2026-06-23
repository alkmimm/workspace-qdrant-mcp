/**
 * MCP tool schema definition for the 'store' tool
 */

export const storeToolDefinition = {
  name: 'store',
  annotations: {
    title: 'Store note/snippet/library/project',
    openWorldHint: false,
    destructiveHint: false, // additive only; no delete path
    idempotentHint: false, // re-storing creates/updates content
  },
  description:
    'Store content or register a project. `type` is REQUIRED — pass it explicitly: "scratchpad" for ad-hoc/persistent notes and snippets (the right target for working notes; project-scoped, surface automatically in project-scoped search), "library" to store reference documentation, "url" to fetch and ingest a web page, or "project" to register a project directory for file watching and ingestion. Required by type: library → libraryName (+ content); url → url; scratchpad → content; project → path. Boundary: this tool only CREATES/updates — to edit or DELETE an existing scratchpad note use the `scratchpad` tool. Examples — note: {type:"scratchpad", content:"...", cwd:"/abs/repo"}; library: {type:"library", libraryName:"tokio", content:"...", title:"Tokio"}; project: {type:"project", path:"/abs/repo"}; url: {type:"url", url:"https://docs.rs/tokio"}.',
  inputSchema: {
    type: 'object' as const,
    properties: {
      type: {
        type: 'string',
        enum: ['library', 'url', 'scratchpad', 'project'],
        description:
          'What to store: "scratchpad" for ad-hoc/persistent notes & snippets (project-scoped), "library" for reference docs (requires libraryName), "url" to fetch and ingest a web page, "project" to register a project directory',
      },
      content: {
        type: 'string',
        description: 'Content to store — required for type "library" and type "scratchpad".',
      },
      projectId: {
        type: 'string',
        description:
          'For type "scratchpad": tenant the note belongs to. The reliable way to tag a note with a project (takes precedence over cwd) — pass the project_id returned by store(type:"project") or seen in search results. Without it (and without a resolvable cwd) the note falls back to the global tenant.',
      },
      cwd: {
        type: 'string',
        description:
          'Absolute path of your current working directory. For type "scratchpad", a best-effort fallback to projectId: the server tries to detect the project from it so the note is tagged correctly (works when the path resolves to a registered project inside the server). Prefer projectId when you have it.',
      },
      libraryName: {
        type: 'string',
        description: 'Library name (required for type "library" unless forProject is true)',
      },
      forProject: {
        type: 'boolean',
        description:
          'When true, store to libraries collection scoped to the current project. libraryName becomes optional (defaults to "project-refs").',
      },
      path: {
        type: 'string',
        description: 'Project directory path (required for type "project")',
      },
      name: {
        type: 'string',
        description:
          'Project display name (optional for type "project", defaults to directory name)',
      },
      title: {
        type: 'string',
        description: 'Content title — applies to type "library" and type "scratchpad" (optional).',
      },
      url: {
        type: 'string',
        description: 'Source URL (for web content)',
      },
      filePath: {
        type: 'string',
        description: 'Source file path',
      },
      tags: {
        type: 'array',
        items: { type: 'string' },
        description: 'Tags — applies to type "scratchpad" only (ignored for other types).',
      },
      sourceType: {
        type: 'string',
        enum: ['user_input', 'web', 'file', 'scratchbook', 'note'],
        description: 'Source type (default: user_input)',
      },
      metadata: {
        type: 'object',
        additionalProperties: { type: 'string' },
        description: 'Additional metadata',
      },
    },
    required: ['type'],
  },
};
