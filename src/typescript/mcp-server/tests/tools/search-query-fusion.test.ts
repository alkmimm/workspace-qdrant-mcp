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

/** Opt back into rrf for the tests that pin ITS behaviour; best-rank is the default. */
const RRF = { mode: 'rrf' as const };

describe('fuseQueryLegs', () => {
  it('defaults to best-rank, the measured-better mode', () => {
    // Pins the shipped default. Under rrf the rescued hit lands at the tail;
    // under best-rank it lands at the front — the whole reason best-rank won.
    const original = Array.from({ length: 10 }, (_, i) => hit(`wrong${i}`));
    const defaulted = ids(fuseQueryLegs(original, [hit('gold')], { limit: 11 }));

    expect(defaulted.indexOf('gold')).toBe(1);
  });

  it('returns the original leg untouched when there is no translated leg', () => {
    const original = [hit('a'), hit('b')];
    expect(ids(fuseQueryLegs(original, []))).toEqual(['a', 'b']);
  });

  it('returns the translated leg when the original found nothing', () => {
    expect(ids(fuseQueryLegs([], [hit('x')]))).toEqual(['x']);
  });

  it('promotes a hit both legs agree on', () => {
    // rrf-specific: agreement ACCUMULATES score there, which is what carries
    // `b` over the leg-specific tops. best-rank has no accumulation — it only
    // takes the better position — so this property is not shared.
    const original = [hit('a'), hit('b')];
    const translated = [hit('c'), hit('b')];

    expect(ids(fuseQueryLegs(original, translated, RRF))[0]).toBe('b');
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
      const fused = fuseQueryLegs([hit('orig')], [hit('trans')], { ...RRF, translatedWeight });
      expect(ids(fused)[0]).toBe('orig');
    }
  });

  it('clamps a translated weight above 1 instead of trusting it', () => {
    // Misconfiguration must not be able to invert the D3 floor.
    const fused = fuseQueryLegs([hit('orig')], [hit('trans')], { ...RRF, translatedWeight: 99 });
    expect(ids(fused)[0]).toBe('orig');
  });

  it('keeps a hit only the original leg found — rrf', () => {
    // The pt-chunking-arvore shape: the original ranks the gold, the
    // translation misses it entirely. Under rrf the translated leg is
    // down-weighted, so the original's tail survives.
    const original = [hit('noise1'), hit('noise2'), hit('noise3'), hit('gold')];
    const translated = [hit('other1'), hit('other2'), hit('other3')];

    expect(ids(fuseQueryLegs(original, translated, RRF))).toContain('gold');
  });

  it('best-rank can push the original TAIL out of a full page — the known cost', () => {
    // Not a bug and not a test written to pass: interleaving by best rank into
    // a FIXED-SIZE page means something has to leave. Here the original's 4th
    // hit is displaced by translated hits with better positions.
    //
    // This is the same mechanism behind the one live regression the A/B found
    // (pt-debounce-eventos, rank 5 -> 9) and the reason the comparison tool's
    // tail guard fired on best-rank. It is a real debit, accepted because the
    // paired measurement was still significantly positive (p=0.0119, MRR
    // 0.593 -> 0.627). WQM_TRANSLATE_FUSION=rrf trades it back.
    const original = [hit('noise1'), hit('noise2'), hit('noise3'), hit('gold')];
    const translated = [hit('other1'), hit('other2'), hit('other3')];

    expect(ids(fuseQueryLegs(original, translated))).not.toContain('gold');
    // The original's HEAD is still intact — only the tail pays.
    expect(ids(fuseQueryLegs(original, translated))[0]).toBe('noise1');
    // Give the page room and nothing is lost at all.
    expect(ids(fuseQueryLegs(original, translated, { limit: 7 }))).toContain('gold');
  });

  it('lets the translated leg reach the tail but not the head at the rrf default weight', () => {
    // The property the 0.9 default is chosen for, and the reason 0.7 was inert:
    // a translated top hit must outrank the original's TAIL (so a query whose
    // gold the original never surfaced gets rescued) while leaving the
    // original's HEAD alone.
    const original = Array.from({ length: 10 }, (_, i) => hit(`orig${i}`));
    const fused = ids(fuseQueryLegs(original, [hit('rescued')], { ...RRF, limit: 11 }));

    const rescuedAt = fused.indexOf('rescued');
    expect(rescuedAt).toBeGreaterThan(3); // head untouched
    expect(rescuedAt).toBeLessThan(10); // but it does get in
  });

  it('shows why a low weight is inert', () => {
    // At 0.7 the translated top hit falls below the original's LAST hit, so a
    // disjoint leg contributes nothing at all. Kept as executable rationale for
    // why the rrf weight sat where it did rather than a comment to be trusted.
    const original = Array.from({ length: 10 }, (_, i) => hit(`orig${i}`));
    const fused = ids(
      fuseQueryLegs(original, [hit('ignored')], { ...RRF, translatedWeight: 0.7, limit: 11 })
    );

    expect(fused.indexOf('ignored')).toBe(10);
  });

  it('scores by RRF over both legs', () => {
    const fused = fuseQueryLegs([hit('a')], [hit('a')], { ...RRF, translatedWeight: 0.5 });

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

  describe('best-rank mode', () => {
    const BEST = { mode: 'best-rank' as const };

    it('rescues a gold the original never surfaced, near the rank the translation gave it', () => {
      // The whole point of the mode: the original returns 10 wrong hits, the
      // translation puts the gold first. RRF at any weight <= 1 buries the gold
      // below the original's ENTIRE list; best-rank lifts it to the front.
      //
      // Not literally first: the gold ties the original's rank-0 hit and the
      // tie-break keeps the original ahead (D3). Second is the correct answer
      // here — rescued without displacing.
      const original = Array.from({ length: 10 }, (_, i) => hit(`wrong${i}`));
      const fused = ids(fuseQueryLegs(original, [hit('gold')], { ...BEST, limit: 11 }));

      expect(fused.indexOf('gold')).toBe(1);
      // Contrast with rrf at the 0.9 weight, where the same gold only just
      // clears the original's tail — the difference between "rescued" and
      // "rescued into a position anyone reads".
      const viaRrf = ids(fuseQueryLegs(original, [hit('gold')], { ...RRF, limit: 11 }));
      expect(viaRrf.indexOf('gold')).toBe(7);
      // ...and at 0.7 it does not clear the tail at all.
      const viaLowRrf = ids(
        fuseQueryLegs(original, [hit('gold')], { ...RRF, limit: 11, translatedWeight: 0.7 })
      );
      expect(viaLowRrf.indexOf('gold')).toBe(10);
    });

    it('keeps the original ahead on an equal rank', () => {
      // D3 in this mode: the translated leg can reach a position the original
      // never gave a chunk, but never displaces it at the same rank.
      const fused = ids(fuseQueryLegs([hit('orig')], [hit('trans')], BEST));
      expect(fused[0]).toBe('orig');
    });

    it('takes the better of the two positions for a shared hit', () => {
      const original = [hit('a'), hit('b'), hit('shared')];
      const translated = [hit('shared'), hit('c')];

      const fused = fuseQueryLegs(original, translated, { ...BEST, limit: 5 });
      const order = ids(fused);
      // `shared` was 3rd in the original and 1st in the translation, so it rises
      // to the translation's rank — behind `a`, which ties it and wins on the
      // original tie-break, but ahead of `b` which it used to trail.
      expect(order.indexOf('shared')).toBe(1);
      expect(order.indexOf('shared')).toBeLessThan(order.indexOf('b'));
      expect(fused[order.indexOf('shared')]!.metadata['_query_legs']).toBe('both');
    });

    it('never demotes a chunk the original ranked higher', () => {
      // pt-chunking-arvore shape inverted: original has it at 0, translation at 4.
      const fused = ids(
        fuseQueryLegs([hit('gold'), hit('x')], [hit('y'), hit('z'), hit('w'), hit('v'), hit('gold')], {
          ...BEST,
          limit: 6,
        })
      );
      expect(fused[0]).toBe('gold');
    });

    it('keeps score monotonic with the ordering', () => {
      const fused = fuseQueryLegs([hit('a'), hit('b')], [hit('c')], { ...BEST, limit: 3 });
      for (let i = 1; i < fused.length; i++) {
        expect(fused[i]!.score).toBeLessThanOrEqual(fused[i - 1]!.score);
      }
    });

    it('still returns the original leg alone when there is nothing to fuse', () => {
      expect(ids(fuseQueryLegs([hit('a'), hit('b')], [], BEST))).toEqual(['a', 'b']);
    });
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
