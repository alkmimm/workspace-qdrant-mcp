/**
 * The rule that makes a base-branch fallback safe, in both shapes it is needed:
 *
 *  - {@link mergeFallbackByMissingPath} for the surfaces that run TWO queries
 *    (FTS: grep, search exact) and concatenate the results;
 *  - {@link dropFallbackDuplicatesByPath} for the vector lane, which runs ONE
 *    query with an OR filter and so cannot express the rule in the query.
 *
 * Both say the same thing: the caller's branch OWNS any path it carries, and the
 * fallback may only fill gaps. `list` has always done this in SQL ("rows on
 * `branch` plus rows on `fallbackBranch` whose `relative_path` is not already
 * present"); the other surfaces concatenated instead, so a file changed on the
 * caller's branch came back twice — current content and the base branch's older
 * generation — and an agent could read the pre-edit version of a line it had
 * just fixed.
 */

import { describe, it, expect } from 'vitest';
import {
  dropFallbackDuplicatesByPath,
  mergeFallbackByMissingPath,
} from '../../src/tools/branch-scope.js';

interface Hit {
  path?: string;
  branch?: unknown;
  tag: string;
}

const pathOf = (h: Hit) => h.path;
const branchOf = (h: Hit) => h.branch;

describe('mergeFallbackByMissingPath', () => {
  it('keeps the scoped hit and drops the fallback copy of the same path', () => {
    const scoped: Hit[] = [{ path: 'a.rs', tag: 'current' }];
    const fallback: Hit[] = [{ path: 'a.rs', tag: 'stale' }];

    expect(mergeFallbackByMissingPath(scoped, fallback, pathOf)).toEqual([
      { path: 'a.rs', tag: 'current' },
    ]);
  });

  it('appends fallback hits for paths the scoped result lacks', () => {
    const scoped: Hit[] = [{ path: 'a.rs', tag: 'current' }];
    const fallback: Hit[] = [
      { path: 'a.rs', tag: 'stale' },
      { path: 'b.rs', tag: 'unchanged-on-branch' },
    ];

    expect(mergeFallbackByMissingPath(scoped, fallback, pathOf).map((h) => h.tag)).toEqual([
      'current',
      'unchanged-on-branch',
    ]);
  });

  it('keeps several scoped hits for one path (many lines in one file)', () => {
    const scoped: Hit[] = [
      { path: 'a.rs', tag: 'line-10' },
      { path: 'a.rs', tag: 'line-20' },
    ];

    expect(mergeFallbackByMissingPath(scoped, [{ path: 'a.rs', tag: 'stale' }], pathOf)).toHaveLength(
      2
    );
  });

  it('returns a copy of the scoped result when there is no fallback', () => {
    const scoped: Hit[] = [{ path: 'a.rs', tag: 'current' }];
    const out = mergeFallbackByMissingPath(scoped, [], pathOf);

    expect(out).toEqual(scoped);
    expect(out).not.toBe(scoped);
  });

  it('keeps a fallback hit whose path cannot be read rather than losing it', () => {
    const out = mergeFallbackByMissingPath(
      [{ path: 'a.rs', tag: 'current' }],
      [{ tag: 'no-path' }],
      pathOf
    );

    expect(out.map((h) => h.tag)).toEqual(['current', 'no-path']);
  });
});

describe('dropFallbackDuplicatesByPath', () => {
  it('drops the base-branch generation of a path the caller branch answered', () => {
    const hits: Hit[] = [
      { path: 'a.rs', branch: 'main', tag: 'stale' },
      { path: 'a.rs', branch: 'feature/x', tag: 'current' },
    ];

    expect(
      dropFallbackDuplicatesByPath(hits, 'feature/x', 'main', pathOf, branchOf).map((h) => h.tag)
    ).toEqual(['current']);
  });

  it('keeps base-branch hits for paths the caller branch never carried', () => {
    const hits: Hit[] = [
      { path: 'a.rs', branch: 'feature/x', tag: 'changed-here' },
      { path: 'b.rs', branch: 'main', tag: 'unchanged-here' },
    ];

    expect(
      dropFallbackDuplicatesByPath(hits, 'feature/x', 'main', pathOf, branchOf).map((h) => h.tag)
    ).toEqual(['changed-here', 'unchanged-here']);
  });

  it('reads a comma-joined branch set, the display form the daemon returns', () => {
    const hits: Hit[] = [
      { path: 'a.rs', branch: 'main', tag: 'stale' },
      { path: 'a.rs', branch: 'feature/x,main', tag: 'identical-on-both' },
    ];

    expect(
      dropFallbackDuplicatesByPath(hits, 'feature/x', 'main', pathOf, branchOf).map((h) => h.tag)
    ).toEqual(['identical-on-both']);
  });

  it('reads an array branch set too', () => {
    const hits: Hit[] = [
      { path: 'a.rs', branch: ['main'], tag: 'stale' },
      { path: 'a.rs', branch: ['feature/x'], tag: 'current' },
    ];

    expect(
      dropFallbackDuplicatesByPath(hits, 'feature/x', 'main', pathOf, branchOf).map((h) => h.tag)
    ).toEqual(['current']);
  });

  it('is a no-op when no fallback branch is in play', () => {
    const hits: Hit[] = [
      { path: 'a.rs', branch: 'main', tag: 'one' },
      { path: 'a.rs', branch: 'other', tag: 'two' },
    ];

    expect(dropFallbackDuplicatesByPath(hits, 'main', undefined, pathOf, branchOf)).toHaveLength(2);
  });

  it('is a no-op for a cross-branch sweep (branch "*")', () => {
    const hits: Hit[] = [
      { path: 'a.rs', branch: 'main', tag: 'one' },
      { path: 'a.rs', branch: 'feature/x', tag: 'two' },
    ];

    expect(dropFallbackDuplicatesByPath(hits, '*', 'main', pathOf, branchOf)).toHaveLength(2);
  });

  it('is a no-op when the fallback equals the effective branch', () => {
    const hits: Hit[] = [{ path: 'a.rs', branch: 'main', tag: 'one' }];

    expect(dropFallbackDuplicatesByPath(hits, 'main', 'main', pathOf, branchOf)).toHaveLength(1);
  });

  it('keeps everything when the caller branch answered nothing', () => {
    const hits: Hit[] = [
      { path: 'a.rs', branch: 'main', tag: 'one' },
      { path: 'b.rs', branch: 'main', tag: 'two' },
    ];

    expect(dropFallbackDuplicatesByPath(hits, 'feature/x', 'main', pathOf, branchOf)).toHaveLength(
      2
    );
  });

  it('keeps a hit whose branch metadata is missing rather than losing it', () => {
    const hits: Hit[] = [
      { path: 'a.rs', branch: 'feature/x', tag: 'current' },
      { path: 'a.rs', tag: 'unknown-branch' },
    ];

    expect(
      dropFallbackDuplicatesByPath(hits, 'feature/x', 'main', pathOf, branchOf).map((h) => h.tag)
    ).toEqual(['current', 'unknown-branch']);
  });
});
