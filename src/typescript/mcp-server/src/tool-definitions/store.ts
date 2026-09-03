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
    'Store content or register a project. Pass `type` explicitly, or it is inferred from an unambiguous arg (libraryName→"library", path→"project", url→"url"); a bare note still needs type:"scratchpad". Types: "scratchpad" for ad-hoc/persistent notes and snippets (the right target for working notes; project-scoped, surface automatically in project-scoped search; each write is stamped with origin_branch/origin_cwd/origin_worktree provenance), "library" for reference documentation, "url" to fetch and ingest a web page, "project" to register a project directory for file watching and ingestion, "feedback" to record product feedback about the workspace-qdrant tooling itself (aggregated in a dedicated bucket; triaged via /feedback-review). Required by type: library → libraryName (+ content); url → url; scratchpad → content; project → path; feedback → content + category. Boundary: this tool only CREATES/updates — to edit or DELETE an existing scratchpad note use the `scratchpad` tool. Examples — note: {type:"scratchpad", content:"...", cwd:"/abs/repo"}; library: {type:"library", libraryName:"tokio", content:"...", title:"Tokio"}; project: {type:"project", path:"/abs/repo"}; url: {type:"url", url:"https://docs.rs/tokio"}.',
  inputSchema: {
    type: 'object' as const,
    properties: {
      type: {
        type: 'string',
        enum: ['library', 'url', 'scratchpad', 'project', 'feedback'],
        description:
          'What to store: "scratchpad" for ad-hoc/persistent notes & snippets (project-scoped), "library" for reference docs (requires libraryName), "url" to fetch and ingest a web page, "project" to register a project directory, "feedback" to record product feedback ABOUT the workspace-qdrant tooling itself — a tool that misled you, a flag that paid off, a missing rule (requires category; aggregated in a dedicated bucket, triaged via /feedback-review)',
      },
      content: {
        type: 'string',
        description: 'Content to store — required for type "library" and type "scratchpad".',
      },
      projectId: {
        type: 'string',
        description:
          'Tenant a project-scoped write belongs to (types "scratchpad" and "url", and "library" with forProject). The most direct way to target a project — it outranks cwd — so pass the project_id returned by store(type:"project") or seen in search results when you know it. Without it (and without a resolvable cwd or session project) the write falls back to the global tenant.',
      },
      cwd: {
        type: 'string',
        description:
          'Absolute path of your current working directory. Over HTTP the server cannot observe it, so pass it: a project-scoped write resolves its tenant from this cwd with the SAME precedence as search/list/grep (explicit projectId > this cwd > the session\'s active project), so the repo you name here wins over whatever project the session last activated. The response echoes the resolved project_id and project_path — check they name the repo you meant.',
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
      branch: {
        type: 'string',
        description:
          'For type "scratchpad" and "feedback": provenance override — the git branch to record as the note\'s origin_branch (useful when writing from a worktree the server cannot see). When omitted the server stamps the session\'s current branch (best-effort). Attribution only: notes are never branch-filtered on read.',
      },
      category: {
        type: 'string',
        enum: ['win', 'friction', 'trap', 'missing-rule', 'other'],
        description:
          'For type "feedback" (REQUIRED): the kind of feedback — "win" (a tool/flag/pattern that paid off; worth a regression guard), "friction" (clunky but worked), "trap" (a response that misled you or caused rework), "missing-rule" (a convention that should be captured as a rule), "other". Used to group and prioritize in /feedback-review.',
      },
      refTool: {
        type: 'string',
        description:
          'For type "feedback" (optional): which workspace-qdrant tool the feedback is about (e.g. "search", "grep", "graph", "list", "store") — used to group feedback by tool in /feedback-review.',
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
  },
};
