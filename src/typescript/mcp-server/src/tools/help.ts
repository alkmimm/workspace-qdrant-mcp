/**
 * `help` tool — on-demand topical manual (progressive disclosure, issue #357).
 *
 * Purely static and local: no daemon round-trip, no persistent state, no
 * project detection. Deliberately NOT in OP_EVENT_TOOLS — the search_events
 * op CHECK (schema v48) does not accept a `help` op, and a static lookup has
 * no latency/outcome worth a daemon write; the JSONL tool-call log still
 * records usage.
 */

import { HELP_TOPICS } from './help-topics.js';

interface TopicIndexEntry {
  topic: string;
  summary: string;
}

export interface HelpResult {
  success: boolean;
  topic?: string;
  content?: string;
  topics?: TopicIndexEntry[];
  hint?: string;
}

function topicIndex(): TopicIndexEntry[] {
  return HELP_TOPICS.map((t) => ({ topic: t.id, summary: t.summary }));
}

/** List of valid ids, for hints and for tests that pin the advertised set. */
export function helpTopicIds(): string[] {
  return HELP_TOPICS.map((t) => t.id);
}

export function handleHelp(args: Record<string, unknown> | undefined): HelpResult {
  const raw = args?.['topic'];
  if (raw === undefined || raw === null || raw === '') {
    return {
      success: true,
      topics: topicIndex(),
      hint: 'Pass topic:"<id>" for the full chapter.',
    };
  }
  if (typeof raw !== 'string') {
    return {
      success: false,
      topics: topicIndex(),
      hint: '`topic` must be a string id from the index.',
    };
  }
  const wanted = raw.trim().toLowerCase();
  const match = HELP_TOPICS.find((t) => t.id === wanted);
  if (!match) {
    return {
      success: false,
      topics: topicIndex(),
      hint: `Unknown topic "${raw}". Pick an id from \`topics\`.`,
    };
  }
  return { success: true, topic: match.id, content: match.content };
}
