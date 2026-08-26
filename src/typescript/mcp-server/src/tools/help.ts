/**
 * `help` tool — on-demand topical manual (progressive disclosure, issue #357).
 *
 * The handler answers entirely from in-process constants, and the dispatcher
 * skips its session preamble for STATIC_TOOLS (tool-dispatcher.ts), so a help
 * call really does cost no daemon round-trip, no git spawn, and no project
 * detection. Deliberately NOT in OP_EVENT_TOOLS — the search_events op CHECK
 * (schema v48) does not accept a `help` op, and a static lookup has no
 * latency/outcome worth a daemon write; the JSONL tool-call log still records
 * usage.
 */

import { HELP_TOPICS, HELP_TOPIC_IDS } from './help-topics.js';

export interface HelpResult {
  success: boolean;
  topic?: string;
  content?: string;
  topics?: ReadonlyArray<{ topic: string; summary: string }>;
  hint?: string;
}

const TOPIC_INDEX: ReadonlyArray<{ topic: string; summary: string }> = HELP_TOPIC_IDS.map(
  (id) => ({ topic: id, summary: HELP_TOPICS[id].summary })
);

/** Every miss path returns the same shape: the index plus a corrective hint. */
function indexResponse(success: boolean, hint: string): HelpResult {
  return { success, topics: TOPIC_INDEX, hint };
}

export function handleHelp(args: Record<string, unknown> | undefined): HelpResult {
  const raw = args?.['topic'];
  // Normalize BEFORE the emptiness check so `topic: "  "` and `topic: ""`
  // take the same (index) path, and a padded id still resolves.
  const wanted = raw == null ? '' : String(raw).trim().toLowerCase();
  if (wanted === '') {
    return indexResponse(true, 'Pass topic:"<id>" for the full chapter.');
  }
  // Membership check against the id list, not a bare record index — a
  // prototype key like "toString" must miss, not return Function.prototype.
  if (!(HELP_TOPIC_IDS as ReadonlyArray<string>).includes(wanted)) {
    // Echo capped and JSON-escaped: the value is caller-supplied and lands in
    // model-visible text, so never reflect it unbounded or raw.
    const echoed = JSON.stringify(String(raw).slice(0, 64));
    return indexResponse(false, `Unknown topic ${echoed}. Pick an id from \`topics\`.`);
  }
  const match = HELP_TOPICS[wanted as keyof typeof HELP_TOPICS];
  return { success: true, topic: wanted, content: match.content };
}
