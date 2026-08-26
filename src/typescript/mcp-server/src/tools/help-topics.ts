/**
 * Static topical manual served by the `help` tool.
 *
 * These chapters hold the long-tail guidance evicted from the server's MCP
 * `instructions` block (issue #357): the always-on instructions keep only a
 * short behavioral kernel (server-instructions.ts), and guidance an agent
 * needs occasionally lives here, fetched on demand. Response hints and error
 * messages may reference a topic by id (e.g. `see help("branches")`) instead
 * of embedding a paragraph.
 *
 * This file — not the instructions — is where detailed usage guidance
 * belongs. A chapter may restate what a tool `description` or a seeded
 * DEFAULT_RULES entry says, but the authoritative copy of a rule stays in
 * exactly one channel; keep chapters in sync when those change.
 */

export interface HelpTopic {
  /** Stable topic id agents pass as `topic`. */
  id: string;
  /** One-line summary shown in the topic index. */
  summary: string;
  /** Full chapter text. */
  content: string;
}

export const HELP_TOPICS: ReadonlyArray<HelpTopic> = [
  {
    id: 'search',
    summary: 'Query formulation, modes, fileType taxonomy, path filters, scope discipline.',
    content: [
      'Modes: "semantic" (default, strongest here) and "hybrid" — there is no keyword mode; for a known',
      'identifier or exact string use exact=true (FTS5 substring) or the `grep` tool instead.',
      'Write queries in ENGLISH regardless of the conversation language: the embedding model is',
      'multilingual, but code identifiers and comments are overwhelmingly English and cross-lingual',
      'recall for code is weak — a non-English query matches same-language prose/docs instead of the',
      'code and recall collapses. Use vocabulary close to the expected identifiers/comments.',
      'When you want the implementation rather than docs or tests, add fileType="code" (other values:',
      'text, config, data, docs, web, slides, build) or a pathGlob like "src/**/*.rs" — documentation',
      'and test files often outrank the implementation otherwise. Note: prose documentation and',
      'Markdown are classified "text"; fileType="docs" is for binary document formats (PDF, Office),',
      'so to bias toward project docs use "text", not "docs". Test hits carry is_test:true and',
      'excludeTests:true filters them server-side.',
      'When a legacy, vendored, or generated tree pollutes results (e.g. a dead "old_project/"), pass',
      'pathExclude — a glob that is the hard opposite of pathGlob — on `search`, `grep`, or `list` to',
      'drop it (it floats, so "old_project/**" removes that dir at the repo root and any nested',
      'depth); a deployment may also demote such paths automatically in semantic ranking via',
      'WQM_SEARCH_DERANK, so results already sink without you asking.',
      'Scope discipline: default to scope="project" with small limits (10); widen to scope="all" or',
      'includeLibraries=true only after a project-scoped query returns nothing useful, never silently.',
    ].join(' '),
  },
  {
    id: 'exact',
    summary: 'grep for exact/regex lookups, list for layout, retrieve by point id.',
    content: [
      'Use `grep` for regex / exact substring across the project — faster and cheaper than `search`',
      'with exact=true for known strings; page big sweeps by passing the response next_offset back as',
      'offset. Use `list` (start with format="summary") to understand layout before drilling in.',
      'Use `retrieve` when you already know the Qdrant point id from a `search`/`list` result or a',
      'metadata filter. The `documentId` argument must be the result `id` field, not',
      'metadata.document_id; if you only have the metadata hash, use filter: { document_id: "..." }.',
      'For exact-search hits, pass filePath + lineNumber from the result metadata. When `retrieve`',
      'returns success:false, read its `hint` before retrying.',
    ].join(' '),
  },
  {
    id: 'store',
    summary: 'What store writes (scratchpad, libraries, url, project) and what it never writes.',
    content: [
      '`store` writes to `scratchpad` (notes, snippets) or `libraries` (only when the user explicitly',
      'asks), and type="url" ingests a page. The server does NOT write project code to the `projects`',
      'collection — that is daemon-owned via file watching. To register/activate a project directory,',
      'use `store` with type="project". A bare {content} with no type signal is ambiguous and errors',
      'rather than silently defaulting.',
    ].join(' '),
  },
  {
    id: 'scratchpad',
    summary: 'Project memory: what to record, provenance stamps, automatic resurfacing.',
    content: [
      'As you work, proactively record durable project knowledge with `store` type="scratchpad":',
      'decisions and their rationale, non-obvious gotchas, conventions, in-flight task state other',
      'agents/sessions may need, and anything worth recalling in a later session. Keep each note',
      'self-contained. Each write is stamped with provenance (origin_branch/origin_cwd/',
      'origin_worktree — pass `branch` explicitly when writing from a git worktree the server cannot',
      'see) so notes are attributable to the branch/worktree that produced them; provenance is',
      'attribution only, never a read filter. Notes are project-scoped and resurface AUTOMATICALLY —',
      'the project-scoped `search` recall lane appends the most relevant notes after the code hits,',
      'so you need not query the scratchpad explicitly. To revise or remove a note, use the',
      '`scratchpad` tool (update/delete, by point `id` from `scratchpad list` or by verbatim content)',
      'rather than creating near-duplicates.',
    ].join(' '),
  },
  {
    id: 'branches',
    summary: 'Branch scoping, agent-branch lifecycle, worktrees, mutation opt-in, multi-clone.',
    content: [
      'Project registration is automatic on session start; the server tracks the current branch via',
      'heartbeat. Use `workspace_index` for observability (read-only actions: list_projects,',
      'project_status, status_all, list_branches, agent_branch_status, observe_*,',
      'incremental_check*). `search` defaults to the current branch; pass branch="<name>" or',
      'branch="*" to widen explicitly — do not widen silently. When working on an agent/feature',
      'branch (especially in a parallel worktree), register it via start_agent_branch with',
      'branchName, purpose, createdBy, and useWorktree=true if applicable; close out with',
      'finish_agent_branch (merged) or abandon_agent_branch (discarded). Mutating actions require',
      'DOUBLE opt-in (allowMutation:true AND WQM_INDEX_MANAGER_ALLOW_MUTATION=1) and explicit user',
      'confirmation, because they affect persistent shared state. sync_current_branch is for git',
      'hooks only — agents must not call it. Multi-clone: tenant_ids are stable per clone; if',
      'results come from the wrong clone, pass projectId explicitly.',
    ].join(' '),
  },
  {
    id: 'graph',
    summary: 'Relationship and impact queries: what calls X, blast radius before refactors.',
    content: [
      'For relationship questions ("what calls X", "what breaks if I change Y") and before',
      'refactoring or renaming a widely-used symbol, use the `graph` tool (`impact` for blast',
      'radius; relationship queries for callers/dependencies) rather than guessing — it surfaces',
      'edges that `search`/`grep` miss.',
    ].join(' '),
  },
  {
    id: 'collections',
    summary: 'The four canonical collections and the low-level embedding helper.',
    content: [
      'Collections: projects (indexed code, daemon-written), libraries (reference docs), rules',
      '(behavioral rules), scratchpad (ad-hoc notes). Budget: default to scope="project" with small',
      'limits; escalate only when needed. `embedding` is a low-level helper reporting the active',
      'embedding provider; prefer `search` unless you specifically need provider introspection.',
    ].join(' '),
  },
  {
    id: 'http',
    summary: 'Project detection over HTTP: the cwd argument and its precedence chain.',
    content: [
      '`search`, `grep`, `list`, `retrieve`, and `rules` auto-detect the current project from your',
      'working directory. Over HTTP the server cannot observe it, so pass your absolute working',
      'directory in the `cwd` argument on each such call (unless you already pass an explicit',
      'projectId). Omitting both can yield "Could not detect project". Resolution precedence:',
      'transport header (x-mcp-host-cwd) > body `cwd` > session-sticky cwd (remembered from your',
      'last call that carried one) > server-side defaults.',
    ].join(' '),
  },
];
