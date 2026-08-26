/**
 * Size-budget and content contract for the always-on server instructions.
 *
 * The instructions block is injected into every client session's system
 * prompt and real clients truncate it mid-block, so it must stay a SHORT
 * behavioral kernel (issue #357): detail belongs in tool descriptions, the
 * seeded DEFAULT_RULES, or the `help` tool's chapters. These tests fail the
 * moment someone grows the kernel back into a manual, and pin the pointers
 * that make progressive disclosure work (every advertised help topic exists).
 */

import { describe, it, expect } from 'vitest';

import { SERVER_INSTRUCTIONS } from '../src/server-instructions.js';
import { helpTopicIds } from '../src/tools/help.js';

const KERNEL_BYTE_BUDGET = 2000;

describe('server instructions kernel', () => {
  it(`stays within the ${KERNEL_BYTE_BUDGET}-byte budget`, () => {
    // If this fails, do NOT raise the budget: move the new guidance into a
    // tool description, a DEFAULT_RULES entry, or a help-topics.ts chapter.
    expect(Buffer.byteLength(SERVER_INSTRUCTIONS, 'utf8')).toBeLessThanOrEqual(KERNEL_BYTE_BUDGET);
  });

  it('keeps the load-bearing behavioral nudges', () => {
    // The kernel earns its always-on cost only through lines that change
    // agent behavior. Each of these has a measured failure mode when absent.
    expect(SERVER_INSTRUCTIONS).toContain('`search` FIRST');
    expect(SERVER_INSTRUCTIONS).toContain('ENGLISH');
    expect(SERVER_INSTRUCTIONS).toContain('`cwd`');
    expect(SERVER_INSTRUCTIONS).toContain('`rules`');
    expect(SERVER_INSTRUCTIONS).toContain('scratchpad');
    expect(SERVER_INSTRUCTIONS).toContain('`grep`');
    expect(SERVER_INSTRUCTIONS).toContain('`graph`');
  });

  it('advertises the help tool and only topics that actually exist', () => {
    expect(SERVER_INSTRUCTIONS).toContain('`help`');
    const advertised = SERVER_INSTRUCTIONS.match(/`help` \(topics: ([^)]+)\)/);
    expect(advertised, 'kernel must list the help topics').not.toBeNull();
    const listed = advertised![1]!.split(',').map((t) => t.trim());
    const real = new Set(helpTopicIds());
    for (const topic of listed) {
      expect(real.has(topic), `kernel advertises unknown help topic "${topic}"`).toBe(true);
    }
    // And the inverse: a chapter added to help-topics.ts should be advertised.
    for (const topic of real) {
      expect(listed, `help topic "${topic}" missing from the kernel's list`).toContain(topic);
    }
  });
});
