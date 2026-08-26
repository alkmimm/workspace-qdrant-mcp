/**
 * Behavior contract for the `help` tool (progressive disclosure, issue #357).
 *
 * The tool is the retrieval half of the slimmed server instructions: every
 * advertised topic must resolve to a substantive chapter, and every miss must
 * return the index so an agent can self-correct in one round-trip.
 */

import { describe, it, expect } from 'vitest';

import { handleHelp, helpTopicIds } from '../../src/tools/help.js';
import { HELP_TOPICS } from '../../src/tools/help-topics.js';
import { helpToolDefinition } from '../../src/tool-definitions/help.js';

describe('help topics catalog', () => {
  it('has unique ids', () => {
    const ids = helpTopicIds();
    expect(new Set(ids).size).toBe(ids.length);
  });

  it('every chapter is substantive (summary + real content)', () => {
    for (const t of HELP_TOPICS) {
      expect(t.summary.length, `${t.id} summary`).toBeGreaterThan(20);
      // A chapter shorter than this is a stub — either write it or drop the topic.
      expect(t.content.length, `${t.id} content`).toBeGreaterThan(200);
    }
  });

  it('every topic id is advertised in the tool description', () => {
    // Agents pick topics from the description before ever seeing the index —
    // an unadvertised chapter is effectively unreachable.
    for (const id of helpTopicIds()) {
      expect(helpToolDefinition.description).toContain(`"${id}"`);
    }
  });
});

describe('handleHelp', () => {
  it('returns the full chapter for each known topic', () => {
    for (const t of HELP_TOPICS) {
      const result = handleHelp({ topic: t.id });
      expect(result.success).toBe(true);
      expect(result.topic).toBe(t.id);
      expect(result.content).toBe(t.content);
      expect(result.topics).toBeUndefined();
    }
  });

  it('is case-insensitive and trims the topic', () => {
    const result = handleHelp({ topic: '  BRANCHES ' });
    expect(result.success).toBe(true);
    expect(result.topic).toBe('branches');
  });

  it('returns the index when called without a topic', () => {
    for (const args of [undefined, {}, { topic: '' }]) {
      const result = handleHelp(args as Record<string, unknown> | undefined);
      expect(result.success).toBe(true);
      expect(result.topics?.map((t) => t.topic)).toEqual(helpTopicIds());
      expect(result.hint).toBeTruthy();
    }
  });

  it('returns success:false plus the index on an unknown topic', () => {
    const result = handleHelp({ topic: 'no-such-topic' });
    expect(result.success).toBe(false);
    expect(result.hint).toContain('no-such-topic');
    expect(result.topics?.map((t) => t.topic)).toEqual(helpTopicIds());
  });

  it('rejects a non-string topic without throwing', () => {
    const result = handleHelp({ topic: 42 });
    expect(result.success).toBe(false);
    expect(result.topics?.length).toBeGreaterThan(0);
  });
});
