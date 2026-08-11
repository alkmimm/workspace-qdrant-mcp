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
import { matchesPathExclude, matchesPathInclude } from '../../src/utils/path-glob.js';
import {
  filterResultsByPathExclude,
  applyPathDerank,
} from '../../src/tools/search-path-filters.js';
import { resolveDerankConfig, DEFAULT_DERANK_PENALTY } from '../../src/tools/search-types.js';
import type { SearchResult } from '../../src/tools/search-types.js';

function result(
  relativePath: string | undefined,
  score = 0.5,
  id = relativePath ?? 'x'
): SearchResult {
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

describe('matchesPathInclude (semantic pathGlob — floats, parity with exclude)', () => {
  it('floats a relative multi-segment glob at any depth (the fix)', () => {
    // Previously the semantic post-filter anchored both ends (matchesGlob), so a
    // relative pattern only matched when the path STARTED with it. It must now
    // float like exclude / grep's daemon-side glob.
    expect(matchesPathInclude('lib/management/test/a/b.dart', 'management/test/**')).toBe(true);
    expect(matchesPathInclude('management/test/x.dart', 'management/test/**')).toBe(true);
    expect(
      matchesPathInclude(
        '/home/u/repo/svc/grpc/AdvertisingGrpcService.java',
        '**/AdvertisingGrpcService.java'
      )
    ).toBe(true);
  });

  it('still constrains: non-matching extension / dir does not match', () => {
    expect(matchesPathInclude('lib/management/test/a.kt', 'management/test/**/*.dart')).toBe(false);
    expect(matchesPathInclude('lib/other/x.dart', 'management/test/**')).toBe(false);
  });

  it('matches the same paths as matchesPathExclude (shared floating semantics)', () => {
    for (const [p, g] of [
      ['old_project/a.php', 'old_project/**'],
      ['pkg/old_project/a.php', 'old_project/**'],
      ['src/main.rs', 'old_project/**'],
    ] as const) {
      expect(matchesPathInclude(p, g)).toBe(matchesPathExclude(p, g));
    }
  });
});

describe('directory-shaped literal (bare name / trailing slash) scopes to the subtree', () => {
  it('a bare directory name matches files UNDER it (the field-reported friction)', () => {
    // Previously `**/tool-builders` end-anchored, so a bare directory matched nothing.
    expect(matchesPathInclude('src/typescript/tool-builders/search.ts', 'tool-builders')).toBe(
      true
    );
    expect(
      matchesPathInclude('/home/u/repo/src/tool-builders/search.ts', 'tool-builders')
    ).toBe(true);
  });

  it('a bare literal still matches an exact file of that name (file OR subtree)', () => {
    expect(matchesPathInclude('scripts/tool-builders', 'tool-builders')).toBe(true);
  });

  it('does not over-match a sibling directory', () => {
    expect(matchesPathInclude('src/typescript/other/search.ts', 'tool-builders')).toBe(false);
  });

  it('a trailing slash is directory-only (subtree yes, same-named file no)', () => {
    expect(matchesPathInclude('src/tool-builders/x.ts', 'tool-builders/')).toBe(true);
    expect(matchesPathInclude('scripts/tool-builders', 'tool-builders/')).toBe(false);
  });

  it('a multi-segment literal scopes to its subtree at the root AND nested', () => {
    expect(matchesPathInclude('src/tools/x.ts', 'src/tools')).toBe(true);
    expect(matchesPathInclude('pkg/src/tools/x.ts', 'src/tools')).toBe(true);
    expect(matchesPathInclude('src/toolsX/x.ts', 'src/tools')).toBe(false);
  });

  it('exclude gets the same directory scoping (shared matcher)', () => {
    expect(matchesPathExclude('a/node_modules/react/index.js', 'node_modules')).toBe(true);
    expect(matchesPathExclude('node_modules/react/index.js', 'node_modules')).toBe(true);
    expect(matchesPathExclude('src/node_modules_shim.ts', 'node_modules')).toBe(false);
  });

  it('regression: a wildcard form keeps its exact meaning (no subtree widening)', () => {
    // `tool-builders/**` is an explicit subtree glob — it must NOT match a bare
    // same-named file, unlike the wildcard-free literal above.
    expect(matchesPathInclude('src/tool-builders/x.ts', 'tool-builders/**')).toBe(true);
    expect(matchesPathInclude('scripts/tool-builders', 'tool-builders/**')).toBe(false);
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

  it('preserves non-worktree results when pathExclude is unset', () => {
    // No pathExclude no longer means a strict identity return: a main-tenant
    // caller still has other-worktree `.claude/worktrees/**` paths dropped
    // (see worktree-path-exclude.test.ts), so the array is rebuilt. A normal
    // path is preserved by value.
    const results = [result('old_project/a.php')];
    expect(filterResultsByPathExclude(results, undefined)).toEqual(results);
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
    const cfg = resolveDerankConfig({
      WQM_SEARCH_DERANK: 'old_project/, /generated/ ,, docs/archive/',
    });
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
    expect(resolveDerankConfig({ WQM_SEARCH_DERANK_PENALTY: '1' }).penalty).toBe(
      DEFAULT_DERANK_PENALTY
    );
    expect(resolveDerankConfig({ WQM_SEARCH_DERANK_PENALTY: '1.5' }).penalty).toBe(
      DEFAULT_DERANK_PENALTY
    );
    expect(resolveDerankConfig({ WQM_SEARCH_DERANK_PENALTY: '-0.2' }).penalty).toBe(
      DEFAULT_DERANK_PENALTY
    );
    expect(resolveDerankConfig({ WQM_SEARCH_DERANK_PENALTY: 'abc' }).penalty).toBe(
      DEFAULT_DERANK_PENALTY
    );
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
