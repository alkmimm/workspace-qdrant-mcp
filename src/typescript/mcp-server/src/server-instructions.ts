/**
 * Always-on MCP server instructions — the behavioral kernel.
 *
 * Deliberately SHORT (budget: ≤2000 bytes, pinned by
 * tests/server-instructions.test.ts). Clients inject this block into every
 * session's system prompt, and real clients truncate it mid-block, so:
 *  (1) only guidance that changes agent behavior belongs here, ORDERED BY
 *      IMPORTANCE — truncation must only ever cost the least critical lines.
 *      In particular the `help` pointer sits near the top: it is what makes
 *      every evicted chapter recoverable, so it must survive truncation.
 *  (2) detail lives in the other three channels: per-tool `description`s
 *      (lazily loaded by clients that defer schemas), the seeded
 *      DEFAULT_RULES (rule-seeder.ts, loaded via `rules` list), and the
 *      `help` tool's topical chapters (tools/help-topics.ts).
 * The topic list below is DERIVED from HELP_TOPIC_IDS — never hand-edit it.
 * Before adding a line here, ask which channel it belongs in instead.
 * Issue #357 has the paragraph-by-paragraph disposition of the old manual.
 */

import { readFileSync } from 'node:fs';

import { HELP_TOPIC_IDS } from './tools/help-topics.js';

/**
 * Env var: path of a file whose contents REPLACE the built-in instructions.
 * An empty file means "send no instructions". Unset = the built-in text.
 *
 * Why: the built-in block is a behavioural prompt ("call `search` FIRST",
 * "record notes with `store`", "call `rules` list at session start") that is
 * right for the interactive product and cannot be conditioned per deployment.
 * A measurement that compares retrieval surfaces needs the prompt to match the
 * surface (`WQM_MCP_TOOLS`) — an instruction to call a tool that is not
 * exposed is a confound, not guidance. The file is read ONCE at startup and a
 * missing file is a hard error: an experiment that misconfigures this must
 * fail to start, not silently run with the default prompt.
 */
export const MCP_INSTRUCTIONS_FILE_ENV = 'WQM_MCP_INSTRUCTIONS_FILE';

/**
 * The instructions this deployment sends on `initialize`, or `undefined` for
 * none. Reads `WQM_MCP_INSTRUCTIONS_FILE` when set; throws when that file
 * cannot be read.
 */
export function resolveServerInstructions(
  env: Record<string, string | undefined> = process.env,
  readFile: (path: string) => string = (path) => readFileSync(path, 'utf-8')
): string | undefined {
  const file = (env[MCP_INSTRUCTIONS_FILE_ENV] ?? '').trim();
  if (file.length === 0) return SERVER_INSTRUCTIONS;
  let text: string;
  try {
    text = readFile(file);
  } catch (err) {
    throw new Error(
      `${MCP_INSTRUCTIONS_FILE_ENV}=${file} could not be read: ` +
        `${err instanceof Error ? err.message : String(err)}`
    );
  }
  const trimmed = text.trim();
  return trimmed.length === 0 ? undefined : trimmed;
}

export const SERVER_INSTRUCTION_LINES: ReadonlyArray<string> = [
  "This server exposes the user's indexed codebase, libraries, behavioral rules, scratchpad, and project/branch registry.",
  'Call `search` FIRST for any question about this codebase, project structure, or library docs — do not answer from training data. Write queries in ENGLISH with vocabulary close to the expected identifiers; add fileType="code" or a pathGlob to bias implementation over docs/tests. Default to scope="project" with small limits; widen only when a project-scoped query returns nothing useful.',
  'Routing: `grep` for a known identifier / exact substring / regex; `list` (format="summary") for layout; `graph` for relationship and impact questions ("what calls X"); `retrieve` for a known point id.',
  `Details on demand: call \`help\` (topics: ${HELP_TOPIC_IDS.join(', ')}) instead of guessing parameter semantics — error hints reference chapters as help("<topic>").`,
  'Over HTTP the server cannot see your working directory — pass your absolute `cwd` (or an explicit projectId) on the first call of a session; the session then remembers it. Omitting both can fail project detection.',
  'Reads scope to the current branch by default; pass branch="<name>" or branch="*" to widen explicitly — never silently.',
  'Start of session: call `rules` action="list" to load behavioral preferences; record durable conventions with action="add". Record durable task/project knowledge with `store` type="scratchpad" — notes resurface automatically in project-scoped search.',
  'The `projects` collection is daemon-owned (file watching) and never a store target. `store` writes scratchpad notes, libraries (explicit user request only), url captures, and feedback reports; store type="project" registers/activates a project.',
];

export const SERVER_INSTRUCTIONS: string = SERVER_INSTRUCTION_LINES.join(' ');
