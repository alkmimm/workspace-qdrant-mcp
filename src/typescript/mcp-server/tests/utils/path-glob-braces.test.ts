/**
 * Brace alternation in the shared TypeScript path matcher.
 *
 * The daemon's FTS glob (`expand_braces`, text_search/escaping.rs) has always
 * expanded `{a,b}`; the TypeScript engines did not. So `pathGlob:"**\/*.{rs,ts}"`
 * answered 28 matches from `grep` and ZERO — silently — from `list` and
 * semantic `search` (measured on this repo, 2026-09-05). These tests pin the
 * expansion itself and the two floating matchers built on it; the SQLite `GLOB`
 * clause `list` uses is covered in tests/clients/tracked-files-queries-list.
 */

import { describe, it, expect } from 'vitest';

import {
  expandBraces,
  matchesPathInclude,
  matchesPathExclude,
} from '../../src/utils/path-glob.js';

describe('expandBraces', () => {
  it('expands one group into one pattern per alternative, in order', () => {
    expect(expandBraces('*.{rs,toml}')).toEqual(['*.rs', '*.toml']);
  });

  it('returns a brace-free pattern untouched', () => {
    expect(expandBraces('src/**/*.rs')).toEqual(['src/**/*.rs']);
  });

  it('expands multiple groups combinatorially', () => {
    expect(expandBraces('{src,tests}/*.{rs,ts}')).toEqual([
      'src/*.rs',
      'src/*.ts',
      'tests/*.rs',
      'tests/*.ts',
    ]);
  });

  it('splits only on top-level commas, so nesting works', () => {
    expect(expandBraces('{a,{b,c}}')).toEqual(['a', 'b', 'c']);
  });

  it('trims alternatives, matching the daemon expansion', () => {
    // A human-typed "{rs, ts}" must not become the unmatchable "*. ts".
    expect(expandBraces('*.{rs, ts}')).toEqual(['*.rs', '*.ts']);
  });

  it('handles an empty alternative (the `{,/**}` shape the normalizer emits)', () => {
    expect(expandBraces('**/src/tools{,/**}')).toEqual(['**/src/tools', '**/src/tools/**']);
  });

  it('leaves an unbalanced brace literal', () => {
    expect(expandBraces('*.{rs')).toEqual(['*.{rs']);
  });

  it('falls back to the raw pattern past the expansion ceiling', () => {
    // 2^9 = 512 combinations, over the 256 cap: returned verbatim, so the
    // braces match literally — a miss, never a wrong match.
    const pathological = '{a,b}'.repeat(9);
    expect(expandBraces(pathological)).toEqual([pathological]);
  });
});

describe('matchesPathInclude / matchesPathExclude with braces', () => {
  it('includes a path matching any alternative', () => {
    expect(matchesPathInclude('src/utils/helpers.rs', '**/*.{rs,ts}')).toBe(true);
    expect(matchesPathInclude('src/server.ts', '**/*.{rs,ts}')).toBe(true);
  });

  it('excludes nothing that matches no alternative', () => {
    expect(matchesPathInclude('README.md', '**/*.{rs,ts}')).toBe(false);
  });

  it('directory-shapes each wildcard-free alternative', () => {
    // `{src,tests}` must scope to both SUBTREES, exactly as the two literals
    // would separately — not just to files literally named src / tests.
    expect(matchesPathInclude('src/lib.rs', '{src,tests}')).toBe(true);
    expect(matchesPathInclude('tests/test_main.rs', '{src,tests}')).toBe(true);
    expect(matchesPathInclude('docs/guide.md', '{src,tests}')).toBe(false);
  });

  it('floats braced alternatives to any depth, like every other pattern', () => {
    expect(matchesPathExclude('pkg/vendor/dep.rs', '{vendor,third_party}')).toBe(true);
    expect(matchesPathExclude('src/lib.rs', '{vendor,third_party}')).toBe(false);
  });

  it('matches absolute paths through the same floated form', () => {
    expect(matchesPathInclude('/home/u/repo/src/main.rs', '**/*.{rs,ts}')).toBe(true);
  });
});
