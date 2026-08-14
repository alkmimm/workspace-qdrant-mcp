import { describe, expect, it } from 'vitest';

import { fuseQueryLegs } from '../../src/tools/search-query-fusion.js';
import { RRF_K } from '../../src/tools/search-types.js';
import type { SearchResult } from '../../src/tools/search-types.js';

function hit(id: string, score = 0.5): SearchResult {
  return {
    id,
    score,
    collection: 'projects',
    content: `content of ${id}`,
    metadata: { relative_path: `src/${id}.ts` },
  } as SearchResult;
}

function ids(results: SearchResult[]): string[] {
  return results.map((r) => String(r.id));
}

describe('fuseQueryLegs', () => {
  it('returns the original leg untouched when there is no translated leg', () => {
    const original = [hit('a'), hit('b')];
    expect(ids(fuseQueryLegs(original, []))).toEqual(['a', 'b']);
  });

  it('returns the translated leg when the original found nothing', () => {
    expect(ids(fuseQueryLegs([], [hit('x')]))).toEqual(['x']);
  });

  it('promotes a hit both legs agree on', () => {
    // `b` is second in each leg but is the only one both legs return, so the
    // accumulated contribution should carry it over the leg-specific tops.
    const original = [hit('a'), hit('b')];
    const translated = [hit('c'), hit('b')];

    expect(ids(fuseQueryLegs(original, translated))[0]).toBe('b');
  });

  it('marks cross-language agreement in metadata', () => {
    const fused = fuseQueryLegs([hit('a'), hit('b')], [hit('b')]);

    const both = fused.find((r) => r.id === 'b');
    const onlyOriginal = fused.find((r) => r.id === 'a');
    expect(both?.metadata['_query_legs']).toBe('both');
    expect(onlyOriginal?.metadata['_query_legs']).toBeUndefined();
  });

  it('never lets the translated leg outweigh the original at equal rank', () => {
    // Same rank in both legs, disjoint hits: the original must come first for
    // any weight <= 1. This is the D3 floor.
    for (const translatedWeight of [0.1, 0.5, 0.7, 1]) {
      const fused = fuseQueryLegs([hit('orig')], [hit('trans')], { translatedWeight });
      expect(ids(fused)[0]).toBe('orig');
    }
  });

  it('clamps a translated weight above 1 instead of trusting it', () => {
    // Misconfiguration must not be able to invert the D3 floor.
    const fused = fuseQueryLegs([hit('orig')], [hit('trans')], { translatedWeight: 99 });
    expect(ids(fused)[0]).toBe('orig');
  });

  it('keeps a hit only the original leg found — the regression guard', () => {
    // The pt-chunking-arvore shape: the original ranks the gold, the
    // translation misses it entirely. Fusion must not drop it.
    const original = [hit('noise1'), hit('noise2'), hit('noise3'), hit('gold')];
    const translated = [hit('other1'), hit('other2'), hit('other3')];

    expect(ids(fuseQueryLegs(original, translated))).toContain('gold');
  });

  it('scores by RRF over both legs', () => {
    const fused = fuseQueryLegs([hit('a')], [hit('a')], { translatedWeight: 0.5 });

    // Rank 0 in both legs: 1/(K+1) + 0.5/(K+1).
    expect(fused[0]!.score).toBeCloseTo(1 / (RRF_K + 1) + 0.5 / (RRF_K + 1), 12);
  });

  it('caps the fused list at the requested limit', () => {
    const original = [hit('a'), hit('b')];
    const translated = [hit('c'), hit('d')];

    expect(fuseQueryLegs(original, translated, { limit: 3 })).toHaveLength(3);
  });

  it('defaults the cap to the original leg length so fusion cannot inflate a page', () => {
    const fused = fuseQueryLegs([hit('a'), hit('b')], [hit('c'), hit('d')]);
    expect(fused).toHaveLength(2);
  });

  it('preserves the original leg payload when both legs return the same chunk', () => {
    const original = [{ ...hit('a'), content: 'from original leg' } as SearchResult];
    const translated = [{ ...hit('a'), content: 'from translated leg' } as SearchResult];

    expect(fuseQueryLegs(original, translated)[0]!.content).toBe('from original leg');
  });

  it('does not mutate either input leg', () => {
    const original = [hit('a')];
    const translated = [hit('a')];
    const originalScore = original[0]!.score;

    fuseQueryLegs(original, translated);

    expect(original[0]!.score).toBe(originalScore);
    expect(original[0]!.metadata['_query_legs']).toBeUndefined();
  });
});
