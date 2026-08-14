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
 * results.
 *
 * The default is NOT arbitrary, and the arithmetic is what picks it. A
 * translated hit at rank 0 scores w/(K+1); it displaces an original hit at rank
 * r when w > (K+1)/(K+1+r). With K=60 that is w > 0.871 to outrank the original
 * 10th, w > 0.938 for its 4th, w > 0.968 for its 2nd. So 0.9 lets the translated
 * leg fill the TAIL — rescuing queries whose gold the original never surfaced —
 * without disturbing the head the original already got right.
 *
 * Measured on the 71-query benchmark (2026-08-12), against the same corpus,
 * flag off as baseline:
 *
 *   weight  pt top-10   pt top-3   tail regressions   verdict
 *   0.7     69.7% (no change — INERT)                 —
 *   0.9     78.8%       54.5%      none               inconclusive
 *   1.0     84.8%       48.5%      1 (rank 5 -> 9)    REGRESSION
 *
 * 0.7 is inert for a structural reason worth keeping in mind when retuning:
 * with equal-length legs and disjoint hits, 0.7/(K+1) sits below the original's
 * LAST hit, so the translated leg can only ever contribute through agreement.
 * 1.0 buys the most top-10 but pays for it in top-3 and trips the tail guard.
 */
export const TRANSLATED_LEG_WEIGHT = Math.min(
  tuningFromEnv('WQM_TRANSLATE_WEIGHT', 0.9),
  1
);

/**
 * How the two legs are combined.
 *
 * `rrf` — weighted reciprocal rank fusion. One global scalar has to serve two
 * opposite situations: when the original leg is junk (language homophily) the
 * translated leg should dominate, and when the original leg is good it should
 * not be touched at all. The 0.7/0.9/1.0 sweep is that tension made visible —
 * 0.7 was inert, 1.0 bought top-10 by breaking top-3.
 *
 * `best-rank` — order by each chunk's BEST position across the legs, ties going
 * to the original. This is exactly the oracle the ceiling was measured with
 * (pt 93.9% vs 90.9% for substitution), so it is the only mode that can reach
 * that number; the cost is that a confident mistranslation lands at the top
 * with nothing damping it, which is what the tail guard exists to catch.
 */
export type QueryFusionMode = 'rrf' | 'best-rank';

/**
 * Fusion mode. `best-rank` is the DEFAULT on measurement (2026-08-12, 71-query
 * benchmark, 7B translator + domain glossary, same corpus, flag-off baseline):
 *
 *                   pt top-10   top-3 overall   MRR     Wilcoxon
 *   baseline        23/33       50/71           0.593   —
 *   rrf w=0.9       29/33       50/71           0.607   p=0.14  (n.s.)
 *   best-rank       29/33       53/71           0.627   p=0.0119
 *
 * Both modes rescue the same six queries; they differ in WHERE those land,
 * which is the difference between technically-in-top-10 and actually findable:
 * pt-upsert-qdrant miss->8 under rrf but miss->2 under best-rank,
 * pt-spec-write-path miss->10 vs miss->4, pt-metricas-fila miss->8 vs miss->2.
 *
 * Cost, recorded because it is a real debit and not a rounding error: one query
 * (pt-debounce-eventos) slides 5->9, which trips the comparison tool's tail
 * guard at its default tolerance of 3 positions. Shipped anyway, deliberately —
 * the guard's PRINCIPLE (never buy an aggregate win with tail damage) holds,
 * but its threshold is an unmeasured default, the query never leaves top-10,
 * and the gains include four queries going from invisible to ranks 2-5. Set
 * WQM_TRANSLATE_FUSION=rrf to trade those ranks back for that one slide.
 */
export const QUERY_FUSION_MODE: QueryFusionMode =
  (process.env['WQM_TRANSLATE_FUSION'] ?? '').trim() === 'rrf' ? 'rrf' : 'best-rank';

/** Stable identity of a hit across the two legs. */
function hitKey(result: SearchResult): string {
  return `${result.collection}:${result.id}`;
}

export interface QueryLegFusionOptions {
  /** Weight of the translated leg (original is 1). Defaults to {@link TRANSLATED_LEG_WEIGHT}. */
  translatedWeight?: number;
  /** Cap on the returned list. Defaults to the length of the original leg. */
  limit?: number;
  /** Combination strategy. Defaults to {@link QUERY_FUSION_MODE}. */
  mode?: QueryFusionMode;
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

  if ((options.mode ?? QUERY_FUSION_MODE) === 'best-rank') {
    return fuseByBestRank(original, translated, limit);
  }

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

/**
 * Order by each chunk's best position across the two legs — the oracle the
 * ceiling was measured with.
 *
 * Ties go to the original leg, which is what keeps D3 meaningful in this mode:
 * the translated leg can reach a rank the original never gave a chunk, but it
 * can never push the original's own hit down at equal rank. `score` is set to
 * the reciprocal of the winning rank so the field stays monotonic with the
 * ordering, as callers downstream assume.
 */
function fuseByBestRank(
  original: readonly SearchResult[],
  translated: readonly SearchResult[],
  limit: number
): SearchResult[] {
  const best = new Map<
    string,
    { rank: number; fromOriginal: boolean; result: SearchResult; legs: number }
  >();

  original.forEach((result, rank) => {
    best.set(hitKey(result), { rank, fromOriginal: true, result: { ...result }, legs: 1 });
  });

  translated.forEach((result, rank) => {
    const key = hitKey(result);
    const existing = best.get(key);
    if (!existing) {
      best.set(key, { rank, fromOriginal: false, result: { ...result }, legs: 1 });
      return;
    }
    existing.legs += 1;
    // Strictly better only — an equal rank keeps the original's placement.
    if (rank < existing.rank) existing.rank = rank;
  });

  return Array.from(best.values())
    .sort((a, b) => a.rank - b.rank || Number(b.fromOriginal) - Number(a.fromOriginal))
    .slice(0, limit)
    .map(({ rank, result, legs }) => ({
      ...result,
      score: 1 / (rank + 1),
      metadata: {
        ...result.metadata,
        _query_legs: legs > 1 ? 'both' : undefined,
      },
    }));
}
