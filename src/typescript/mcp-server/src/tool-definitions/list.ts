/**
 * MCP tool schema definition for the 'list' tool
 */

export const listToolDefinition = {
  name: 'list',
  annotations: {
    title: 'List indexed project structure',
    readOnlyHint: true,
    openWorldHint: false,
  },
  description:
    'List the project file/folder structure from the index the daemon maintains (only indexed files — excludes gitignored, node_modules, etc; this is the daemon\'s indexed view, not a live filesystem walk). Use format "summary" first to understand project layout, then drill into specific folders with the path parameter. "summary" aggregates directory counts over EVERY matching file in one shot (no cursor) — the reliable layout overview; "tree" and "flat" are paged windows (limit / next_token). Responses echo project_id + project_source (cwd | sticky-cwd | projectId | sole-project | server-default) so you can confirm which project answered.',
  inputSchema: {
    type: 'object' as const,
    properties: {
      path: {
        type: 'string',
        description: 'Subfolder relative to project root (default: root)',
      },
      depth: {
        type: 'number',
        description: 'Max directory depth (default: 3, max: 10)',
      },
      format: {
        type: 'string',
        enum: ['tree', 'summary', 'flat'],
        description:
          'Output format (default: tree). "summary": whole-project directory overview aggregated over ALL matching files, not paged — start here for layout. "tree": paged hierarchical listing (limit / next_token). "flat": paged relative paths.',
      },
      fileType: {
        type: 'string',
        description: 'Filter: "code", "text", "data", "config", "build", "web"',
      },
      language: {
        type: 'string',
        description: 'Filter by programming language (e.g., "rust", "typescript")',
      },
      extension: {
        type: 'string',
        description: 'Filter by file extension (e.g., "rs", "ts")',
      },
      pattern: {
        type: 'string',
        description:
          'Glob on the file path, matched at ANY depth — a bare pattern floats, no leading "**/" needed (e.g. "V*.sql" finds migrations in any folder, "helpers.rs", "**/*.test.ts", "src/**/*.ts"). Use `*` for a path segment, `**` for any depth, and `{a,b}` to alternate (e.g. "**/*.{rs,ts}"). A listing that matches nothing says so in `hint`.',
      },
      pathExclude: {
        type: 'string',
        description:
          'Glob on the file path to EXCLUDE from the listing (hard filter, opposite of `pattern`). Floats the same way: "old_project/**" hides that directory at the repo root AND any nested depth. Use it to drop a legacy/vendored tree from the structure view.',
      },
      includeTests: {
        type: 'boolean',
        description: 'Include test files (default: true)',
      },
      limit: {
        type: 'number',
        description: 'Max entries returned (default: 200, max: 500)',
      },
      pageSize: {
        type: 'number',
        description:
          'Page size for cursor pagination (falls back to limit). Use together with cursor to page through a large listing.',
      },
      cursor: {
        type: 'string',
        description:
          'Opaque pagination cursor — pass the `next_token` from a previous list response to fetch the next page.',
      },
      projectId: {
        type: 'string',
        description: 'Specific project ID (default: current project)',
      },
      branch: {
        type: 'string',
        description:
          'Filter by branch name. Defaults to the current Git branch; use "*" for all branches.',
      },
      cwd: {
        type: 'string',
        description:
          'Absolute path of your current working directory. Pass this so the server can auto-detect the project over HTTP (it cannot otherwise observe your location). The response echoes project_id + project_source (cwd | sticky-cwd | projectId | sole-project | server-default): check they name the repo you meant — a cwd-less call may ride the session sticky cwd and answer from the previous project. Ignored when projectId is provided.',
      },
      component: {
        type: 'string',
        description:
          'Filter by component (dot-separated ID or prefix, e.g. "daemon" or "daemon.core"). Auto-detected from Cargo.toml/package.json workspaces.',
      },
      maxResponseBytes: {
        type: 'number',
        description:
          'Cap on the rendered listing chars (default ~24000, the same budget as search/grep). Trailing page entries beyond it are dropped (>=1 kept), budget_truncated.dropped reports how many, and next_token resumes at the first dropped entry — lossless. 0 disables.',
      },
    },
  },
};
