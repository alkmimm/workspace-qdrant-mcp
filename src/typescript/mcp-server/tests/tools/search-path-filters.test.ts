/**
 * Tests for the path-based result shaping shared by search and grep:
 *   - matchesPathExclude   — the floated exclude-glob matcher
 *   - filterResultsByPathExclude — hard per-call drop (`pathExclude`)
 *   - applyPathDerank / resolveDerankConfig — soft WQM_SEARCH_DERANK penalty
 *
 * The exclude floats (root AND nested), never drops a path-less hit, and the
 * de-rank only reorders (multiplies score, preserves relative order, never
 * removes) — both are the invariants the deployment relies on for a legacy dir
 * like `old_project/` to stop polluting results without hiding a needed file.
 */

import { describe, it, expect } from 'vitest';
import { matchesPathExclude } from '../../src/utils/path-glob.js';
import {
  filterResultsByPathExclude,
  applyPathDerank,
} from '../../src/tools/search-path-filters.js';
import { resolveDerankConfig, DEFAULT_DERANK_PENALTY } from '../../src/tools/search-types.js';
import type { SearchResult } from '../../src/tools/search-types.js';

function result(relativePath: string | undefined, score = 0.5, id = relativePath ?? 'x'): SearchResult {
  const metadata: Record<string, unknown> = {};
  if (relativePath !== undefined) metadata['relative_path'] = relativePath;
  return { id, score, collection: 'projects', content: '', metadata };
}

describe('matchesPathExclude', () => {
  it('floats: drops the dir at the repo root AND at any nested depth', () => {
    expect(matchesPathExclude('old_project/legacy.php', 'old_project/**')).toBe(true);
    expect(matchesPathExclude('pkg/old_project/legacy.php', 'old_project/**')).toBe(true);
    expect(matchesPathExclude('old_project/a/b/c.php', 'old_project/**')).toBe(true);
  });

  it('does not match unrelated paths', () => {
    expect(matchesPathExclude('src/old_project_helper.ts', 'old_project/**')).toBe(false);
    expect(matchesPathExclude('src/main.rs', 'old_project/**')).toBe(false);
  });

  it('normalizes backslashes so Windows-style paths still match', () => {
    expect(matchesPathExclude('old_project\\legacy.php', 'old_project/**')).toBe(true);
  });
});

describe('filterResultsByPathExclude', () => {
  it('drops matching results and keeps the rest', () => {
    const results = [
      result('old_project/a.php'),
      result('src/main.rs'),
      result('lib/old_project/b.php'),
    ];
    const kept = filterResultsByPathExclude(results, 'old_project/**');
    expect(kept.map((r) => r.metadata['relative_path'])).toEqual(['src/main.rs']);
  });

  it('is a no-op when pathExclude is unset', () => {
    const results = [result('old_project/a.php')];
    expect(filterResultsByPathExclude(results, undefined)).toBe(results);
  });

  it('KEEPS a result with no resolvable path (cannot match → never a false drop)', () => {
    const pathless = result(undefined);
    const kept = filterResultsByPathExclude([pathless], 'old_project/**');
    expect(kept).toEqual([pathless]);
  });

  it('falls back to the absolute file_path when relative_path is absent', () => {
    const abs: SearchResult = {
      id: 'a',
      score: 0.5,
      collection: 'projects',
      content: '',
      metadata: { file_path: '/home/u/repo/old_project/a.php' },
    };
    expect(filterResultsByPathExclude([abs], 'old_project/**')).toEqual([]);
  });
});

describe('resolveDerankConfig', () => {
  it('parses comma-separated substrings, trims, and drops empties', () => {
    const cfg = resolveDerankConfig({ WQM_SEARCH_DERANK: 'old_project/, /generated/ ,, docs/archive/' });
    expect(cfg.substrings).toEqual(['old_project/', '/generated/', 'docs/archive/']);
  });

  it('defaults the penalty and returns no substrings when unset', () => {
    const cfg = resolveDerankConfig({});
    expect(cfg.substrings).toEqual([]);
    expect(cfg.penalty).toBe(DEFAULT_DERANK_PENALTY);
  });

  it('accepts a valid penalty in [0,1)', () => {
    expect(resolveDerankConfig({ WQM_SEARCH_DERANK_PENALTY: '0.05' }).penalty).toBe(0.05);
    expect(resolveDerankConfig({ WQM_SEARCH_DERANK_PENALTY: '0' }).penalty).toBe(0);
  });

  it('rejects penalty >= 1, negative, or garbage (keeps the default)', () => {
    expect(resolveDerankConfig({ WQM_SEARCH_DERANK_PENALTY: '1' }).penalty).toBe(DEFAULT_DERANK_PENALTY);
    expect(resolveDerankConfig({ WQM_SEARCH_DERANK_PENALTY: '1.5' }).penalty).toBe(DEFAULT_DERANK_PENALTY);
    expect(resolveDerankConfig({ WQM_SEARCH_DERANK_PENALTY: '-0.2' }).penalty).toBe(DEFAULT_DERANK_PENALTY);
    expect(resolveDerankConfig({ WQM_SEARCH_DERANK_PENALTY: 'abc' }).penalty).toBe(DEFAULT_DERANK_PENALTY);
  });
});

describe('applyPathDerank', () => {
  it('multiplies the score of matched paths and leaves others untouched', () => {
    const legacy = result('old_project/a.php', 0.8);
    const live = result('src/main.rs', 0.6);
    applyPathDerank([legacy, live], { substrings: ['old_project/'], penalty: 0.2 });
    expect(legacy.score).toBeCloseTo(0.16, 6);
    expect(live.score).toBe(0.6);
  });

  it('sinks a legacy hit below live code that started lower', () => {
    const legacy = result('old_project/a.php', 0.9);
    const live = result('src/main.rs', 0.5);
    applyPathDerank([legacy, live], { substrings: ['old_project/'], penalty: 0.2 });
    expect(legacy.score).toBeLessThan(live.score); // 0.18 < 0.5
  });

  it('preserves relative order AMONG de-ranked hits (uniform multiplier)', () => {
    const a = result('old_project/a.php', 0.8);
    const b = result('old_project/b.php', 0.6);
    applyPathDerank([a, b], { substrings: ['old_project/'], penalty: 0.2 });
    expect(a.score).toBeGreaterThan(b.score);
  });

  it('is a no-op with no substrings or a penalty >= 1', () => {
    const r1 = result('old_project/a.php', 0.8);
    applyPathDerank([r1], { substrings: [], penalty: 0.2 });
    expect(r1.score).toBe(0.8);
    const r2 = result('old_project/a.php', 0.8);
    applyPathDerank([r2], { substrings: ['old_project/'], penalty: 1 });
    expect(r2.score).toBe(0.8);
  });
});
