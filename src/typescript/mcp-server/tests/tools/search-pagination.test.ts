/**
 * Tests for paginateRanked — the post-fusion page slice backing search `offset`
 * pagination (P1.5 part C). The load-bearing property: consecutive pages over
 * the same ranked list never overlap, and `hasMore` drives `next_offset`.
 */

import { describe, it, expect } from 'vitest';
import { paginateRanked, reconcileNextOffset } from '../../src/tools/search-helpers.js';

describe('paginateRanked', () => {
  const ranked = Array.from({ length: 25 }, (_, i) => i); // 0..24

  it('slices the [offset, offset+limit) window', () => {
    expect(paginateRanked(ranked, 0, 10).page).toEqual([0, 1, 2, 3, 4, 5, 6, 7, 8, 9]);
    expect(paginateRanked(ranked, 10, 10).page).toEqual([10, 11, 12, 13, 14, 15, 16, 17, 18, 19]);
  });

  it('produces non-overlapping consecutive pages', () => {
    const p1 = paginateRanked(ranked, 0, 10).page;
    const p2 = paginateRanked(ranked, 10, 10).page;
    expect(p1.filter((x) => p2.includes(x))).toEqual([]);
  });

  it('reports hasMore until the last page', () => {
    expect(paginateRanked(ranked, 0, 10).hasMore).toBe(true);
    expect(paginateRanked(ranked, 10, 10).hasMore).toBe(true);
    expect(paginateRanked(ranked, 20, 10).hasMore).toBe(false); // [20..24], nothing beyond
  });

  it('returns a short final page with no more', () => {
    const { page, hasMore } = paginateRanked(ranked, 20, 10);
    expect(page).toEqual([20, 21, 22, 23, 24]);
    expect(hasMore).toBe(false);
  });

  it('clamps negative offset and limit', () => {
    expect(paginateRanked(ranked, -5, 10).page).toEqual([0, 1, 2, 3, 4, 5, 6, 7, 8, 9]);
    expect(paginateRanked(ranked, 0, -1).page).toEqual([]);
  });

  it('is empty past the end', () => {
    const { page, hasMore } = paginateRanked(ranked, 100, 10);
    expect(page).toEqual([]);
    expect(hasMore).toBe(false);
  });
});

describe('reconcileNextOffset (byte-budget × pagination)', () => {
  it('returns undefined when no more code and nothing was dropped', () => {
    expect(reconcileNextOffset(0, 10, 10, false)).toBeUndefined();
  });

  it('stays at offset+limit when more code exists and nothing was dropped', () => {
    expect(reconcileNextOffset(0, 10, 10, true)).toBe(10);
    expect(reconcileNextOffset(10, 10, 10, true)).toBe(20);
  });

  it('points at the first dropped code hit when the budget trimmed the page', () => {
    // 10 code hits, budget kept 6 → next page resumes at 6 (not 10), so the 4
    // dropped hits are reachable rather than skipped.
    expect(reconcileNextOffset(0, 10, 6, true)).toBe(6);
    // even with no "more beyond the page", a budget drop must expose next_offset
    expect(reconcileNextOffset(0, 8, 5, false)).toBe(5);
    // resumes after the kept code hits when paginating
    expect(reconcileNextOffset(10, 10, 7, true)).toBe(17);
  });
});
