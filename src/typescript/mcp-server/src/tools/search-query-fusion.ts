/**
 * Multi-query fusion: combine the rankings of the original query and its
 * translation into one list.
 *
 * WHY THIS IS A SEPARATE AXIS FROM {@link applyRRFFusion} — see
 * docs/plans/2026-08-11-query-translation-plan.md D1. That function balances
 * the DENSE leg against the KEYWORD leg with weights whose own comment records
 * that getting them wrong dropped hybrid recall from 58.7% to 38%. Multi-query
 * is orthogonal: each query variant runs the whole pipeline — including that
 * internal fusion — independently, and only the FINAL rankings meet here. The
 * tuned dense/sparse balance is never touched.
 *
 * WHY FUSE INSTEAD OF SUBSTITUTE (D2/D3), measured 2026-08-12 on the 71-query
 * benchmark with an ideal translation:
 *
 *   today (Portuguese leg only)   pt 24/33 (72.7%)   MRR 0.607
 *   substitution (English only)   pt 30/33 (90.9%)   MRR 0.670
 *   oracle fusion (best of both)  pt 31/33 (93.9%)   MRR 0.685
 *
 * Substitution regressed 5 queries, one of which lost its gold hit outright
 * (pt-chunking-arvore, rank 4 -> miss) — enough for the comparison tool's tail
 * guard to veto the run as a REGRESSION despite p=0.0244 and +9.8pp recall@10.
 * The original leg is what protects those, so it is never dropped and never
 * outweighed.
 */

import { RRF_K, tuningFromEnv } from './search-types.js';
import type { SearchResult } from './search-types.js';

/**
 * Weight of the TRANSLATED leg relative to the original, which is fixed at 1.
 *
 * Capped at 1 so the translated leg can never outweigh the original (D3): a
 * mistranslation must degrade toward "no worse than today", never displace good
 * results. The 0.7 default is an UNMEASURED starting point — phase 5 sweeps it,
 * and the tail guard on regressions is the gate that decides.
 */
export const TRANSLATED_LEG_WEIGHT = Math.min(
  tuningFromEnv('WQM_TRANSLATE_WEIGHT', 0.7),
  1
);

/** Stable identity of a hit across the two legs. */
function hitKey(result: SearchResult): string {
  return `${result.collection}:${result.id}`;
}

export interface QueryLegFusionOptions {
  /** Weight of the translated leg (original is 1). Defaults to {@link TRANSLATED_LEG_WEIGHT}. */
  translatedWeight?: number;
  /** Cap on the returned list. Defaults to the length of the original leg. */
  limit?: number;
}

/**
 * Fuse the original-query ranking with the translated-query ranking by RRF.
 *
 * Both legs are already fully ranked, deduped, and reranked by the pipeline, so
 * this only reconciles the two orderings. A hit found by BOTH legs accumulates
 * both contributions, which is the signal we want: agreement across languages
 * is stronger evidence than either leg alone.
 *
 * Returns the original leg untouched when there is no translated leg to fuse —
 * the fail-open path, and byte-identical to today's behaviour.
 */
export function fuseQueryLegs(
  original: readonly SearchResult[],
  translated: readonly SearchResult[],
  options: QueryLegFusionOptions = {}
): SearchResult[] {
  if (translated.length === 0) return [...original];
  if (original.length === 0) return [...translated];

  const translatedWeight = Math.min(options.translatedWeight ?? TRANSLATED_LEG_WEIGHT, 1);
  const limit = options.limit ?? original.length;

  const fused = new Map<string, { score: number; result: SearchResult; legs: number }>();

  const accumulate = (results: readonly SearchResult[], weight: number): void => {
    results.forEach((result, rank) => {
      const key = hitKey(result);
      const contribution = weight / (RRF_K + rank + 1);
      const existing = fused.get(key);
      if (existing) {
        existing.score += contribution;
        existing.legs += 1;
        return;
      }
      fused.set(key, { score: contribution, result: { ...result }, legs: 1 });
    });
  };

  // Original first so its object (and its metadata) wins on a tie of identity —
  // the two legs describe the same chunk, but the original leg's payload is the
  // one the caller's query actually matched.
  accumulate(original, 1);
  accumulate(translated, translatedWeight);

  return Array.from(fused.values())
    .sort((a, b) => b.score - a.score)
    .slice(0, limit)
    .map(({ score, result, legs }) => ({
      ...result,
      score,
      metadata: {
        ...result.metadata,
        // Observability: which legs produced this hit. `both` is the
        // cross-language agreement case worth being able to count later.
        _query_legs: legs > 1 ? 'both' : undefined,
      },
    }));
}
