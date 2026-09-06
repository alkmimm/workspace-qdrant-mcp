/**
 * Behavior contract for the `help` tool (progressive disclosure, issue #357).
 *
 * The tool is the retrieval half of the slimmed server instructions: every
 * chapter must be substantive, every miss must return the index so an agent
 * can self-correct in one round-trip, and the caller-supplied topic must
 * never be reflected unbounded. The advertised topic lists (tool description,
 * input enum, kernel) are DERIVED from HELP_TOPIC_IDS, so no parity tests
 * exist — only the derivation wiring is pinned.
 */

import { describe, it, expect } from 'vitest';

import { handleHelp } from '../../src/tools/help.js';
import { HELP_TOPICS, HELP_TOPIC_IDS, helpRef } from '../../src/tools/help-topics.js';
import { PROJECT_SOURCES } from '../../src/tools/project-echo.js';
import { helpToolDefinition } from '../../src/tool-definitions/help.js';

describe('help topics catalog', () => {
  it('every chapter is substantive (summary + real content)', () => {
    for (const id of HELP_TOPIC_IDS) {
      expect(HELP_TOPICS[id].summary.length, `${id} summary`).toBeGreaterThan(20);
      // A chapter shorter than this is a stub — either write it or drop the topic.
      expect(HELP_TOPICS[id].content.length, `${id} content`).toBeGreaterThan(200);
    }
  });

  it('derives the input enum and description from HELP_TOPIC_IDS', () => {
    const topicSchema = helpToolDefinition.inputSchema.properties.topic as { enum: string[] };
    expect(topicSchema.enum).toEqual([...HELP_TOPIC_IDS]);
    expect(helpToolDefinition.description).toContain(
      HELP_TOPIC_IDS.map((id) => `"${id}"`).join(', ')
    );
  });

  it('helpRef renders a stable pointer shape', () => {
    expect(helpRef('http')).toBe('help("http")');
  });

  // The read surfaces echo `project_id`/`project_path`/`project_source` on
  // every project-scoped answer, but the chapter an agent is pointed at said
  // nothing about it — so a surprising `project_source:"sticky-cwd"` had no
  // documented meaning anywhere.
  it('the http chapter documents the read-side project echo', () => {
    const http = HELP_TOPICS.http.content;
    for (const field of ['project_id', 'project_path', 'project_source']) {
      expect(http, field).toContain(field);
    }
  });

  it('the http chapter documents EVERY project_source rung', () => {
    for (const rung of PROJECT_SOURCES) {
      expect(HELP_TOPICS.http.content, rung).toContain(`"${rung}"`);
    }
  });
});

describe('handleHelp', () => {
  it('returns the full chapter for each known topic', () => {
    for (const id of HELP_TOPIC_IDS) {
      const result = handleHelp({ topic: id });
      expect(result.success).toBe(true);
      expect(result.topic).toBe(id);
      expect(result.content).toBe(HELP_TOPICS[id].content);
      expect(result.topics).toBeUndefined();
    }
  });

  it('is case-insensitive and trims the topic', () => {
    const result = handleHelp({ topic: '  BRANCHES ' });
    expect(result.success).toBe(true);
    expect(result.topic).toBe('branches');
  });

  it('returns the index for missing, empty, and whitespace-only topics alike', () => {
    // Normalization runs BEFORE the emptiness check: '   ' and '' must take
    // the same success path, not diverge into an unknown-topic failure.
    for (const args of [undefined, {}, { topic: '' }, { topic: '   ' }]) {
      const result = handleHelp(args as Record<string, unknown> | undefined);
      expect(result.success).toBe(true);
      expect(result.topics?.map((t) => t.topic)).toEqual([...HELP_TOPIC_IDS]);
      expect(result.hint).toBeTruthy();
    }
  });

  it('returns success:false plus the index on any unknown topic value', () => {
    for (const topic of ['no-such-topic', 42, true, { nested: 1 }]) {
      const result = handleHelp({ topic });
      expect(result.success, `topic ${String(topic)}`).toBe(false);
      expect(result.topics?.map((t) => t.topic)).toEqual([...HELP_TOPIC_IDS]);
      expect(result.hint).toContain('Unknown topic');
    }
  });

  it('misses on Object.prototype keys instead of returning prototype members', () => {
    for (const topic of ['toString', 'constructor', 'hasOwnProperty', '__proto__']) {
      const result = handleHelp({ topic });
      expect(result.success, topic).toBe(false);
      expect(result.content).toBeUndefined();
    }
  });

  it('caps and escapes the echoed topic in the unknown-topic hint', () => {
    const bomb = 'x"`\n'.repeat(10_000);
    const result = handleHelp({ topic: bomb });
    expect(result.success).toBe(false);
    // Echo is sliced to 64 chars and JSON-escaped — no raw quotes/newlines,
    // no unbounded reflection of caller input into model-visible text.
    expect(result.hint!.length).toBeLessThan(200);
    expect(result.hint).not.toContain('\n');
  });
});
