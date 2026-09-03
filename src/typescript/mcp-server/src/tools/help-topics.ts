/**
 * Static topical manual served by the `help` tool.
 *
 * These chapters hold the long-tail guidance evicted from the server's MCP
 * `instructions` block (issue #357): the always-on instructions keep only a
 * short behavioral kernel (server-instructions.ts), and guidance an agent
 * needs occasionally lives here, fetched on demand.
 *
 * Editorial rule: a chapter carries only the delta the other channels cannot
 * hold — behavior that spans tools, failure modes, and discipline. Parameter
 * semantics belong in the tool's own `description`/inputSchema (lazily loaded
 * by clients that defer schemas); durable conventions belong in the seeded
 * DEFAULT_RULES (rule-seeder.ts). Where a rule already has an authoritative
 * string, interpolate it (see RETRIEVE_ID_FILTER_HINT) instead of paraphrasing.
 *
 * The topic-id record is the single source: the tool description, the enum in
 * its inputSchema, and the kernel's topic list are all DERIVED from
 * HELP_TOPIC_IDS — add a chapter here and every advertisement updates itself.
 */

import { RETRIEVE_ID_FILTER_HINT, RETRIEVE_LOCATION_HINT } from './retrieve-hints.js';

export interface HelpTopicBody {
  /** One-line summary shown in the topic index. */
  readonly summary: string;
  /** Full chapter text. */
  readonly content: string;
}

export const HELP_TOPICS = {
  search: {
    summary: 'Scope ladder (project/global/all), widening discipline, where param detail lives.',
    content: [
      'Scope ladder: scope="project" (default) searches the current repo only, and REFUSES rather',
      'than silently widening when no project resolves. scope="global" searches the projects',
      'collection across every indexed repo (code only, no libraries/scratchpad). scope="all"',
      'searches every repo plus libraries and scratchpad. Widen only after a project-scoped query',
      'returns nothing useful, and never silently — say so when you do. includeLibraries=true adds',
      'reference docs to a project-scoped query without going cross-tenant. Modes are "semantic"',
      '(default, strongest) and "hybrid" only; for a known identifier or exact string use the',
      '`grep` tool or exact=true, not a mode. Parameter-level detail — the ENGLISH-query rule, the',
      'fileType taxonomy and its text-vs-docs gotcha, pathGlob/pathExclude semantics, excludeTests',
      '— lives in the `search` tool\'s own parameter descriptions; read those rather than guessing.',
    ].join(' '),
  },
  exact: {
    summary: 'grep paging, list layout, retrieve by point id vs metadata filter.',
    content: [
      'Use `grep` for regex / exact substring across the project — faster and cheaper than `search`',
      'with exact=true for known strings; page big sweeps by passing the response\'s next_offset',
      'back as offset. Use `list` (start with format="summary") to understand layout before',
      'drilling in. `retrieve` fetches a known point: pass the result `id` field as `documentId`',
      `(NOT metadata.document_id). ${RETRIEVE_ID_FILTER_HINT} ${RETRIEVE_LOCATION_HINT}`,
      'When `retrieve` returns success:false, read its `hint` before retrying.',
    ].join(' '),
  },
  store: {
    summary: 'The store types (scratchpad, library, url, feedback, project) and what is daemon-owned.',
    content: [
      '`store` infers `type` from an unambiguous arg (libraryName → library, path → project,',
      'url → url); a bare {content} with no signal errors rather than silently defaulting. Types:',
      '"scratchpad" (notes — see the scratchpad topic), "library" (reference docs, only when the',
      'user explicitly asks), "url" (ingest a web page), "feedback" (friction reports about this',
      'tooling — requires a category; triaged later, so do NOT put these in the scratchpad), and',
      '"project" (register/activate a project directory). The `projects` collection itself is',
      'daemon-owned via file watching — the server never writes project code.',
    ].join(' '),
  },
  scratchpad: {
    summary: 'Project memory: what to record, provenance stamps, automatic resurfacing.',
    content: [
      'Proactively record durable project knowledge with `store` type="scratchpad": decisions and',
      'their rationale, non-obvious gotchas, conventions, in-flight task state other',
      'agents/sessions may need. Keep each note self-contained. Writes are stamped with provenance',
      '(origin_branch, origin_cwd, origin_worktree — pass `branch` explicitly when writing from a',
      'git worktree the server cannot see); provenance is attribution only, never a read filter.',
      'Notes are project-scoped and resurface AUTOMATICALLY — the project-scoped `search` recall',
      'lane appends the most relevant notes after the code hits. Tenanting follows the same',
      'precedence as the read surfaces — explicit `projectId` > the cwd you passed > the session',
      "project > GLOBAL — and the response echoes back the resolved tenant AND that project's",
      'path, so check it names the repo you meant. Over HTTP, a cwd-less write with no session',
      'project files the note under the GLOBAL tenant, where project-scoped search never',
      'resurfaces it — so pass `cwd` (or `projectId`). To revise or remove a note, use the',
      '`scratchpad` tool (update/delete) rather than creating near-duplicates.',
    ].join(' '),
  },
  branches: {
    summary: 'Branch scoping, agent-branch lifecycle, worktrees, mutation opt-in, multi-clone.',
    content: [
      'Project registration is automatic on session start; the server tracks the current branch',
      'via heartbeat. Reads scope to the current branch by default; pass branch="<name>" or',
      'branch="*" to widen explicitly — never silently. `workspace_index` is the observability',
      'surface; its schema documents which actions are read-only and which mutate. Mutating',
      'actions require DOUBLE opt-in (allowMutation:true AND WQM_INDEX_MANAGER_ALLOW_MUTATION=1)',
      'plus explicit user confirmation, because they affect persistent shared state. When working',
      'on an agent/feature branch (especially in a parallel worktree), register it via',
      'start_agent_branch (branchName, purpose, createdBy, useWorktree=true if applicable) and',
      'close out with finish_agent_branch (merged) or abandon_agent_branch (discarded).',
      'sync_current_branch is for git hooks only — agents must not call it. Multi-clone:',
      'tenant_ids are stable per clone; if results come from the wrong clone, pass `projectId`',
      'explicitly.',
    ].join(' '),
  },
  graph: {
    summary: 'Relationship and impact queries: what calls X, blast radius before refactors.',
    content: [
      'For relationship questions ("what calls X", "what breaks if I change Y") and before',
      'refactoring or renaming a widely-used symbol, use the `graph` tool (`impact` for blast',
      'radius; relationship queries for callers/dependencies) rather than guessing — it surfaces',
      'edges that `search`/`grep` miss.',
    ].join(' '),
  },
  collections: {
    summary: 'The four canonical collections and the low-level embedding helper.',
    content: [
      'Collections: projects (indexed code, daemon-written), libraries (reference docs), rules',
      '(behavioral rules), scratchpad (ad-hoc notes). Budget: default to scope="project" with small',
      'limits; escalate only when needed. `embedding` is a low-level helper reporting the active',
      'embedding provider; prefer `search` unless you specifically need provider introspection.',
    ].join(' '),
  },
  http: {
    summary: 'Project detection over HTTP: the cwd argument and its precedence chain.',
    content: [
      'Every project-scoped tool accepts a `cwd` argument. Over HTTP the server cannot observe',
      'your working directory, so pass your absolute cwd (or an explicit `projectId`) — front-load',
      'it on the FIRST call of a session. Resolution precedence: transport header (x-mcp-host-cwd)',
      '> body `cwd` > session-sticky cwd (remembered from your last call that carried one) >',
      'server-side defaults. The project-scoped WRITES (store scratchpad/url, store library with',
      'forProject, scratchpad update/delete) resolve their tenant from that same chain, so a cwd',
      'you pass wins over whatever project the session last activated — and each write echoes the',
      'resolved tenant and project path back. Read tools refuse with "Could not detect project"',
      'when nothing resolves; a write instead lands in the GLOBAL tenant, where project-scoped',
      'search never resurfaces it (see the scratchpad topic).',
    ].join(' '),
  },
  rules: {
    summary: 'Behavioral rules: session-start load, when to add, rules-vs-scratchpad boundary.',
    content: [
      '`rules` stores durable behavioral conventions — build/test commands, preferred libraries,',
      'patterns to follow or avoid — global or per-project. Load them with action="list" at the',
      'start of a session, before non-trivial work. Record a newly discovered convention with',
      'action="add": keep it short and imperative, and update an existing rule instead of adding a',
      'near-duplicate. Boundary: rules are for conventions that should shape EVERY future session;',
      'one-off, task-specific context belongs in the scratchpad instead (see the scratchpad topic).',
    ].join(' '),
  },
} as const satisfies Record<string, HelpTopicBody>;

export type HelpTopicId = keyof typeof HELP_TOPICS;

export const HELP_TOPIC_IDS = Object.keys(HELP_TOPICS) as ReadonlyArray<HelpTopicId>;

/** Typed pointer to a chapter, for response hints and error messages —
 *  `helpRef('http')` → `help("http")`. Using this instead of a string literal
 *  makes a renamed/removed chapter a compile error at every referencing hint. */
export function helpRef(topic: HelpTopicId): string {
  return `help("${topic}")`;
}
