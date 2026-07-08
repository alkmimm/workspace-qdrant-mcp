/**
 * Grep response shaping (spec 20 §3.2): per-line cap + global response byte
 * budget, plus the shared applyByteBudget helper both search and grep use.
 */

import { describe, it, expect } from 'vitest';
import {
  shapeGrepMatches,
  DEFAULT_GREP_MAX_BYTES_PER_LINE,
  type GrepMatch,
} from '../../src/tools/grep.js';
import { applyByteBudget } from '../../src/tools/response-budget.js';

function match(overrides: Partial<GrepMatch> = {}): GrepMatch {
  return {
    file: 'src/foo.ts',
    line: 1,
    content: 'const answer = 42;',
    context_before: [],
    context_after: [],
    ...overrides,
  };
}

describe('applyByteBudget', () => {
  const sizeOf = (s: string): number => s.length;

  it('returns everything when under budget or when the budget is disabled', () => {
    expect(applyByteBudget(['aa', 'bb'], sizeOf, 100)).toEqual({ kept: ['aa', 'bb'], dropped: 0 });
    expect(applyByteBudget(['aa', 'bb'], sizeOf, 0)).toEqual({ kept: ['aa', 'bb'], dropped: 0 });
  });

  it('drops trailing items once the running total would exceed the budget', () => {
    const { kept, dropped } = applyByteBudget(['aaaa', 'bbbb', 'cccc'], sizeOf, 8);
    expect(kept).toEqual(['aaaa', 'bbbb']);
    expect(dropped).toBe(1);
  });

  it('always keeps at least one item', () => {
    const { kept, dropped } = applyByteBudget(['a'.repeat(100), 'b'], sizeOf, 10);
    expect(kept).toHaveLength(1);
    expect(dropped).toBe(1);
  });

  it('handles an empty list', () => {
    expect(applyByteBudget([], sizeOf, 10)).toEqual({ kept: [], dropped: 0 });
  });
});

describe('shapeGrepMatches', () => {
  it('passes normal line-scoped matches through untouched (mode truncate, zero cuts)', () => {
    const matches = [match(), match({ line: 2, context_before: ['a'], context_after: ['b'] })];
    const shaped = shapeGrepMatches(matches, {});
    expect(shaped.matches).toEqual(matches);
    expect(shaped.hitsTruncated).toBe(0);
    expect(shaped.dropped).toBe(0);
    expect(shaped.shapeMode).toBe('truncate');
  });

  it('caps an over-long content line with a compact +N marker', () => {
    const long = 'x'.repeat(DEFAULT_GREP_MAX_BYTES_PER_LINE + 700);
    const shaped = shapeGrepMatches([match({ content: long })], {});
    const content = shaped.matches[0]!.content;
    expect(content).toContain('…[+700 chars]');
    expect(content.length).toBeLessThan(long.length);
    expect(shaped.hitsTruncated).toBe(1);
  });

  it('caps context lines too and counts the match once', () => {
    const long = 'y'.repeat(2000);
    const shaped = shapeGrepMatches(
      [match({ context_before: [long], context_after: [long, 'short'] })],
      {}
    );
    const m = shaped.matches[0]!;
    expect(m.context_before[0]!.length).toBeLessThan(2000);
    expect(m.context_after[0]!.length).toBeLessThan(2000);
    expect(m.context_after[1]).toBe('short');
    expect(shaped.hitsTruncated).toBe(1);
  });

  it('honors an explicit per-line cap and 0-disables', () => {
    const long = 'z'.repeat(300);
    const capped = shapeGrepMatches([match({ content: long })], { maxBytesPerLine: 100 });
    expect(capped.matches[0]!.content).toContain('…[+200 chars]');
    const uncapped = shapeGrepMatches([match({ content: long })], {
      maxBytesPerLine: 0,
      maxResponseBytes: 0,
    });
    expect(uncapped.matches[0]!.content).toBe(long);
    expect(uncapped.shapeMode).toBe('none');
  });

  it('enforces the response byte budget over the capped matches (>=1 kept)', () => {
    const matches = [1, 2, 3, 4].map((i) => match({ line: i, content: 'q'.repeat(400) }));
    const shaped = shapeGrepMatches(matches, { maxResponseBytes: 900 });
    expect(shaped.matches).toHaveLength(2);
    expect(shaped.dropped).toBe(2);
  });

  it('applies the budget to post-cap sizes (capping frees budget for more matches)', () => {
    // Two 10k-char one-liners: uncapped they'd blow a 2k budget after the
    // first; with the 500-char cap both fit.
    const matches = [1, 2].map((i) => match({ line: i, content: 'm'.repeat(10_000) }));
    const shaped = shapeGrepMatches(matches, { maxResponseBytes: 2000 });
    expect(shaped.matches).toHaveLength(2);
    expect(shaped.dropped).toBe(0);
    expect(shaped.hitsTruncated).toBe(2);
  });
});
