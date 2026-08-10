/**
 * Branch-field collapse — the shared helper that keeps grep / exact / semantic /
 * retrieve from shipping the daemon's full `file_metadata.branches` mirror (a
 * ~60-name, ~1.5 KB dump on every hit in a many-branch repo; field feedback
 * 2026-08-10). The rules live in one place (branch-scope.ts) so every read
 * surface collapses identically.
 */

import { describe, it, expect } from 'vitest';
import {
  BRANCH_SMALL_SET_MAX,
  collapseBranchSet,
  collapseMetadataBranchField,
  collapseResultBranchFields,
  normalizeBranchList,
} from '../../src/tools/branch-scope.js';
import { collapseGrepBranchField, type GrepMatch } from '../../src/tools/grep.js';

// A branch set wider than the small-set threshold — the noise case.
const MANY = Array.from({ length: 60 }, (_, i) => (i === 0 ? 'main' : `feature/b${i}`));

describe('normalizeBranchList', () => {
  it('accepts a comma string (FTS surfaces) and an array (Qdrant payload)', () => {
    expect(normalizeBranchList('main,feature/x')).toEqual(['main', 'feature/x']);
    expect(normalizeBranchList(['main', 'feature/x'])).toEqual(['main', 'feature/x']);
  });

  it('trims whitespace, drops empties, and coerces non-string entries', () => {
    expect(normalizeBranchList(' main , , feature/x ')).toEqual(['main', 'feature/x']);
    expect(normalizeBranchList([1, 'main'])).toEqual(['1', 'main']);
    expect(normalizeBranchList(undefined)).toEqual([]);
    expect(normalizeBranchList('')).toEqual([]);
  });
});

describe('collapseBranchSet', () => {
  it('omits the field when the queried branch is in the set (redundant)', () => {
    expect(collapseBranchSet(MANY, 'main')).toEqual({});
    expect(collapseBranchSet(['main'], 'main')).toEqual({});
  });

  it('shows a small set verbatim (the disambiguation payload of a sweep)', () => {
    expect(collapseBranchSet(['feature/x', 'feature/y'], undefined)).toEqual({
      branch: 'feature/x,feature/y',
    });
    // At the threshold it is still shown in full.
    const atMax = MANY.slice(0, BRANCH_SMALL_SET_MAX);
    expect(collapseBranchSet(atMax, undefined)).toEqual({ branch: atMax.join(',') });
  });

  it('collapses a wide fan-out to "*" + the real count', () => {
    expect(collapseBranchSet(MANY, undefined)).toEqual({ branch: '*', branch_count: 60 });
    // A concrete query NOT in the (wide) set still collapses — signal without dump.
    expect(collapseBranchSet(MANY, 'not-in-set')).toEqual({ branch: '*', branch_count: 60 });
  });

  it('returns {} for an empty set', () => {
    expect(collapseBranchSet([], 'main')).toEqual({});
    expect(collapseBranchSet([], undefined)).toEqual({});
  });
});

describe('collapseGrepBranchField', () => {
  const match = (branch: string | undefined): GrepMatch => ({
    file: 'src/foo.ts',
    line: 1,
    content: 'x',
    context_before: [],
    context_after: [],
    ...(branch !== undefined ? { branch } : {}),
  });

  it('drops the branch on a concrete grep whose hit carries the queried branch', () => {
    const m = match(MANY.join(','));
    collapseGrepBranchField([m], 'main');
    expect(m.branch).toBeUndefined();
    expect(m.branch_count).toBeUndefined();
  });

  it('collapses the branch to "*" + count on a sweep over a wide fan-out', () => {
    const m = match(MANY.join(','));
    collapseGrepBranchField([m], undefined);
    expect(m.branch).toBe('*');
    expect(m.branch_count).toBe(60);
  });

  it('keeps a small branch-exclusive set verbatim and leaves branchless matches alone', () => {
    const exclusive = match('feature/x');
    const none = match(undefined);
    collapseGrepBranchField([exclusive, none], 'main');
    expect(exclusive.branch).toBe('feature/x');
    expect(none.branch).toBeUndefined();
  });
});

describe('collapseMetadataBranchField / collapseResultBranchFields', () => {
  it('collapses a Qdrant payload branch array in place', () => {
    const metadata: Record<string, unknown> = { file_path: 'a.ts', branch: MANY };
    collapseMetadataBranchField(metadata, undefined);
    expect(metadata['branch']).toBe('*');
    expect(metadata['branch_count']).toBe(60);
    // Non-branch metadata is untouched.
    expect(metadata['file_path']).toBe('a.ts');
  });

  it('omits the branch array when the queried branch is a member', () => {
    const metadata: Record<string, unknown> = { branch: MANY, branch_count: 99 };
    collapseMetadataBranchField(metadata, 'main');
    expect('branch' in metadata).toBe(false);
    // A stale branch_count must not survive the omit.
    expect('branch_count' in metadata).toBe(false);
  });

  it('is a no-op when there is no branch key', () => {
    const metadata: Record<string, unknown> = { file_path: 'a.ts' };
    collapseMetadataBranchField(metadata, 'main');
    expect(metadata).toEqual({ file_path: 'a.ts' });
  });

  it('walks a result list and tolerates null/absent metadata', () => {
    const results = [
      { metadata: { branch: MANY } },
      { metadata: null },
      {},
    ];
    collapseResultBranchFields(results, undefined);
    expect(results[0].metadata?.branch).toBe('*');
    expect(results[0].metadata?.branch_count).toBe(60);
  });
});
