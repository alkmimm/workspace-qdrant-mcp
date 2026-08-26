/**
 * Size-budget and content contract for the always-on server instructions.
 *
 * The instructions block is injected into every client session's system
 * prompt and real clients truncate it mid-block, so it must stay a SHORT
 * behavioral kernel (issue #357): detail belongs in tool descriptions, the
 * seeded DEFAULT_RULES, or the `help` tool's chapters. These tests fail the
 * moment someone grows the kernel back into a manual, and pin the truncation
 * ordering that makes progressive disclosure survive a cut: the `help`
 * pointer must sit near the top, never last.
 *
 * The topic list itself is DERIVED from HELP_TOPIC_IDS (no parity test
 * needed — it cannot drift by construction).
 */

import { describe, it, expect } from 'vitest';

import { SERVER_INSTRUCTIONS, SERVER_INSTRUCTION_LINES } from '../src/server-instructions.js';
import { HELP_TOPIC_IDS } from '../src/tools/help-topics.js';

const KERNEL_BYTE_BUDGET = 2000;

describe('server instructions kernel', () => {
  it(`stays within the ${KERNEL_BYTE_BUDGET}-byte budget`, () => {
    // If this fails, do NOT raise the budget: move the new guidance into a
    // tool description, a DEFAULT_RULES entry, or a help-topics.ts chapter —
    // the kernel stays O(short) even as chapters accumulate, because only the
    // derived topic-id list grows with them (~10 bytes per chapter).
    expect(Buffer.byteLength(SERVER_INSTRUCTIONS, 'utf8')).toBeLessThanOrEqual(KERNEL_BYTE_BUDGET);
  });

  it('keeps the load-bearing behavioral nudges', () => {
    // The kernel earns its always-on cost only through lines that change
    // agent behavior. Each of these has a measured failure mode when absent.
    expect(SERVER_INSTRUCTIONS).toContain('`search` FIRST');
    expect(SERVER_INSTRUCTIONS).toContain('ENGLISH');
    expect(SERVER_INSTRUCTIONS).toContain('`cwd`');
    expect(SERVER_INSTRUCTIONS).toContain('branch="*"');
    expect(SERVER_INSTRUCTIONS).toContain('`rules`');
    expect(SERVER_INSTRUCTIONS).toContain('scratchpad');
    expect(SERVER_INSTRUCTIONS).toContain('`grep`');
    expect(SERVER_INSTRUCTIONS).toContain('`graph`');
    expect(SERVER_INSTRUCTIONS).toContain('feedback');
  });

  it('places the help pointer early — truncation must not cut it', () => {
    const helpLine = SERVER_INSTRUCTION_LINES.findIndex((l) => l.includes('`help`'));
    expect(helpLine, 'kernel must advertise the help tool').toBeGreaterThanOrEqual(0);
    // Clients truncate from the END. The pointer is what makes every evicted
    // chapter recoverable, so it must live in the top half of the kernel and
    // never be the final line.
    expect(helpLine).toBeLessThan(SERVER_INSTRUCTION_LINES.length - 1);
    expect(helpLine).toBeLessThanOrEqual(Math.floor(SERVER_INSTRUCTION_LINES.length / 2));
  });

  it('derives the advertised topic list from HELP_TOPIC_IDS', () => {
    // Sanity that the derivation is actually wired (not a hand-copied list).
    expect(SERVER_INSTRUCTIONS).toContain(`(topics: ${HELP_TOPIC_IDS.join(', ')})`);
  });
});
