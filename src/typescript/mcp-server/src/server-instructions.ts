/**
 * Always-on MCP server instructions — the behavioral kernel.
 *
 * Deliberately SHORT (budget: ≤2000 bytes, pinned by
 * tests/server-instructions.test.ts). Clients inject this block into every
 * session's system prompt, and real clients truncate it mid-block, so:
 *  (1) only guidance that changes agent behavior belongs here, ordered by
 *      importance — truncation must only ever cost the least critical lines;
 *  (2) detail lives in the other three channels: per-tool `description`s
 *      (lazily loaded by clients that defer schemas), the seeded
 *      DEFAULT_RULES (rule-seeder.ts, loaded via `rules` list), and the
 *      `help` tool's topical chapters (tools/help-topics.ts).
 * Before adding a line here, ask which of those channels it belongs in
 * instead. Issue #357 has the paragraph-by-paragraph disposition of the old
 * long-form manual.
 */

export const SERVER_INSTRUCTIONS: string = [
  "This server exposes the user's indexed codebase, libraries, behavioral rules, scratchpad, and project/branch registry.",
  'Call `search` FIRST for any question about this codebase, project structure, or library docs — do not answer from training data. Write queries in ENGLISH with vocabulary close to the expected identifiers; add fileType="code" or a pathGlob to bias implementation over docs/tests. Default to scope="project" with small limits; widen only when a project-scoped query returns nothing useful.',
  'Routing: `grep` for a known identifier / exact substring / regex; `list` (format="summary") for layout; `graph` for relationship and impact questions ("what calls X"); `retrieve` for a known point id.',
  'Over HTTP the server cannot see your working directory — pass your absolute `cwd` on each call (or an explicit projectId); omitting both can fail project detection.',
  'Start of session: call `rules` action="list" to load behavioral preferences; record durable conventions with action="add". Record durable task/project knowledge with `store` type="scratchpad" — notes resurface automatically in project-scoped search.',
  'The `projects` collection is daemon-owned (file watching): `store` writes only scratchpad/libraries, and store type="project" registers/activates a project.',
  'Details on demand: call `help` (topics: search, exact, store, scratchpad, branches, graph, collections, http) instead of guessing parameter semantics — response hints may reference these topics.',
].join(' ');
