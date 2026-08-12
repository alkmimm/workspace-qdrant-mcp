/**
 * Paired A/B comparison of two semantic-search benchmark reports.
 *
 * WHY THIS EXISTS — see docs/plans/2026-08-10-contextual-chunk-headers-plan.md §5 R3.
 * The bundled dataset holds 46 queries, so a binary hit@10 comparison moves in
 * quanta of 2.17pp and, under McNemar, needs 5+ queries to flip with ZERO
 * regressions before reaching p<0.05 — a minimum detectable effect around
 * +11pp. Retrieval changes worth shipping are routinely smaller than that, so
 * gating on hit-rate deltas at this dataset size mostly measures noise while
 * looking like a measurement.
 *
 * This module gates on the CONTINUOUS paired signal instead: the per-query MRR
 * delta (a gold file moving from rank 7 to rank 3 counts here, and is entirely
 * invisible to hit@10), tested with Wilcoxon signed-rank. Same 46 queries, far
 * more signal extracted from each one.
 *
 * `recallAt10` is still reported, for continuity with the historical baselines,
 * but carries NO decision power.
 *
 * A positive delta always means "the variant is better".
 *
 * Pure computation — no filesystem, no network. Callers parse the reports and
 * own the IO, which keeps this testable against fixtures while the stack is
 * down or busy.
 */

import type { BenchmarkMode } from './semantic-search.js';

/**
 * Above this many non-zero pairs the exact null distribution is abandoned for
 * the normal approximation. The DP itself stays cheap well past this, but the
 * subset counts reach 2^n, and beyond 2^53 float64 stops counting exactly.
 * The bundled dataset (46 queries) sits comfortably below.
 */
const EXACT_MAX_N = 50;

/** The two evaluation fields the comparison consumes, per query and mode. */
export interface EvaluationLike {
  mrr: number;
  firstRelevantRank: number | undefined;
}

/**
 * Minimal structural contract for a comparable report. The harness' full
 * `SemanticSearchBenchmarkReport` satisfies it as-is; adapters (see
 * compare-io.ts) can build it from thinner sources — e.g. a saved search_eval
 * tool response — without pretending to be a full report.
 */
export interface ComparableQueryRun {
  id: string;
  query: string;
  modes: Partial<Record<BenchmarkMode, { evaluation: EvaluationLike }>>;
}

export interface ComparableReport {
  queries: readonly ComparableQueryRun[];
  summary: { modes: Partial<Record<BenchmarkMode, { recallAt10: number }>> };
}

/** Default rank-regression tolerance for {@link compareBenchmarkReports}. */
const DEFAULT_MAX_RANK_REGRESSION = 3;

export interface PairedQueryDelta {
  id: string;
  query: string;
  baselineMrr: number;
  variantMrr: number;
  /** variant − baseline. Positive = the variant ranks the first gold hit higher. */
  mrrDelta: number;
  baselineRank: number | undefined;
  variantRank: number | undefined;
  /**
   * variant − baseline in rank positions, so POSITIVE means WORSE (the gold hit
   * moved further down). `null` when the two sides are not comparable — i.e.
   * one of them found no gold file at all; see `lostEntirely`/`gainedEntirely`.
   */
  rankDelta: number | null;
  /** Baseline surfaced a gold file within top-k and the variant did not. */
  lostEntirely: boolean;
  /** Variant surfaced a gold file within top-k and the baseline did not. */
  gainedEntirely: boolean;
}

export interface WilcoxonResult {
  /** Non-zero pairs actually tested — zero deltas are dropped, per Wilcoxon. */
  n: number;
  /** Pairs dropped for having exactly zero delta. */
  zeroPairs: number;
  wPlus: number;
  wMinus: number;
  /** Two-sided p-value. */
  pValue: number;
  method: 'exact' | 'normal-approximation';
  /**
   * Matched-pairs rank-biserial correlation, (W+ − W−)/(W+ + W−): −1..1, where
   * positive favours the variant. Reported because a p-value alone says nothing
   * about magnitude at this sample size.
   */
  effectSize: number;
}

export interface BenchmarkComparison {
  mode: BenchmarkMode;
  pairedQueries: number;
  /**
   * Query ids present in only one of the reports. Surfaced rather than dropped
   * silently: a shrinking pair set is the difference between "no effect" and
   * "we compared different things".
   */
  unpairedBaselineIds: string[];
  unpairedVariantIds: string[];
  deltas: PairedQueryDelta[];
  improved: number;
  regressed: number;
  unchanged: number;
  meanMrrDelta: number;
  medianMrrDelta: number;
  wilcoxon: WilcoxonResult;
  /**
   * Tail guard, independent of the aggregate: queries that lost a gold hit
   * outright or fell more than `maxRankRegression` positions. A positive mean
   * with a bad tail is a regression wearing a disguise.
   */
  regressions: PairedQueryDelta[];
  /** Reported for continuity with historical baselines. NOT a decision gate. */
  reportOnly: {
    baselineRecallAt10: number;
    variantRecallAt10: number;
    recallAt10Delta: number;
  };
}

export interface CompareOptions {
  /** Mode to compare on both sides. Defaults to `semantic`. */
  mode?: BenchmarkMode;
  /** Rank positions a query may fall before counting as a regression. */
  maxRankRegression?: number;
}

/**
 * Compare a variant run against a baseline run, pairing queries by id.
 *
 * Throws when the requested mode is absent from either report — a silent
 * fallback to another mode would compare two different things and report it as
 * a result.
 */
export function compareBenchmarkReports(
  baseline: ComparableReport,
  variant: ComparableReport,
  options: CompareOptions = {}
): BenchmarkComparison {
  const mode = options.mode ?? 'semantic';
  const maxRankRegression = options.maxRankRegression ?? DEFAULT_MAX_RANK_REGRESSION;

  const baselineById = indexById(baseline.queries, 'baseline');
  const variantById = indexById(variant.queries, 'variant');

  const deltas: PairedQueryDelta[] = [];
  for (const [id, baselineRun] of baselineById) {
    const variantRun = variantById.get(id);
    if (!variantRun) continue;

    const baselineMode = baselineRun.modes[mode];
    const variantMode = variantRun.modes[mode];
    if (!baselineMode || !variantMode) {
      throw new Error(
        `Query "${id}" has no "${mode}" mode run on ${!baselineMode ? 'the baseline' : 'the variant'} ` +
          `report. Re-run both sides with the same modes before comparing.`
      );
    }

    deltas.push(buildDelta(id, baselineRun.query, baselineMode.evaluation, variantMode.evaluation));
  }

  const mrrDeltas = deltas.map((d) => d.mrrDelta);

  return {
    mode,
    pairedQueries: deltas.length,
    unpairedBaselineIds: [...baselineById.keys()].filter((id) => !variantById.has(id)),
    unpairedVariantIds: [...variantById.keys()].filter((id) => !baselineById.has(id)),
    deltas,
    improved: deltas.filter((d) => d.mrrDelta > 0).length,
    regressed: deltas.filter((d) => d.mrrDelta < 0).length,
    unchanged: deltas.filter((d) => d.mrrDelta === 0).length,
    meanMrrDelta: mean(mrrDeltas),
    medianMrrDelta: median(mrrDeltas),
    wilcoxon: wilcoxonSignedRank(mrrDeltas),
    regressions: deltas.filter((d) => isRegression(d, maxRankRegression)),
    reportOnly: {
      baselineRecallAt10: baseline.summary.modes[mode]?.recallAt10 ?? 0,
      variantRecallAt10: variant.summary.modes[mode]?.recallAt10 ?? 0,
      recallAt10Delta:
        (variant.summary.modes[mode]?.recallAt10 ?? 0) -
        (baseline.summary.modes[mode]?.recallAt10 ?? 0),
    },
  };
}

/**
 * Index query runs by id, refusing duplicates. Pairing is by id; letting a
 * duplicate overwrite an earlier run would silently corrupt a measurement —
 * against this module's whole reason to exist.
 */
function indexById(
  queries: readonly ComparableQueryRun[],
  side: 'baseline' | 'variant'
): Map<string, ComparableQueryRun> {
  const byId = new Map<string, ComparableQueryRun>();
  for (const query of queries) {
    if (byId.has(query.id)) {
      throw new Error(
        `Duplicate query id "${query.id}" in the ${side} report — ` +
          'pairing is by id, so a duplicate would silently overwrite a measurement.'
      );
    }
    byId.set(query.id, query);
  }
  return byId;
}

function buildDelta(
  id: string,
  query: string,
  baseline: EvaluationLike,
  variant: EvaluationLike
): PairedQueryDelta {
  const baselineRank = baseline.firstRelevantRank;
  const variantRank = variant.firstRelevantRank;
  const bothRanked = baselineRank !== undefined && variantRank !== undefined;

  return {
    id,
    query,
    baselineMrr: baseline.mrr,
    variantMrr: variant.mrr,
    mrrDelta: variant.mrr - baseline.mrr,
    baselineRank,
    variantRank,
    rankDelta: bothRanked ? variantRank - baselineRank : null,
    lostEntirely: baselineRank !== undefined && variantRank === undefined,
    gainedEntirely: baselineRank === undefined && variantRank !== undefined,
  };
}

function isRegression(delta: PairedQueryDelta, maxRankRegression: number): boolean {
  if (delta.lostEntirely) return true;
  return delta.rankDelta !== null && delta.rankDelta > maxRankRegression;
}

export interface ComparisonAssessment {
  decision: 'improvement' | 'regression' | 'inconclusive';
  reasons: string[];
}

export interface AssessOptions {
  /** Significance level for the two-sided Wilcoxon p. Defaults to 0.05. */
  alpha?: number;
}

/**
 * The decision rule from docs/plans/2026-08-10-contextual-chunk-headers-plan.md
 * §5 R3, as code:
 *
 * 1. The tail guard fires first and is independent of the aggregate — a
 *    significant mean improvement does not buy back queries that lost their
 *    gold hit or fell past the tolerance.
 * 2. Otherwise a significant Wilcoxon result decides, by its direction.
 * 3. Anything else is INCONCLUSIVE — deliberately not "no effect": at n=46 the
 *    all-positive two-sided floor is 6 pairs, so a small true effect and noise
 *    are indistinguishable. Saying "no effect" here would be the exact
 *    overclaim this module exists to prevent.
 */
export function assessComparison(
  comparison: BenchmarkComparison,
  options: AssessOptions = {}
): ComparisonAssessment {
  const alpha = options.alpha ?? 0.05;
  const { wilcoxon } = comparison;

  if (comparison.regressions.length > 0) {
    return {
      decision: 'regression',
      reasons: comparison.regressions.map((r) =>
        r.lostEntirely
          ? `"${r.id}" lost its gold hit entirely (was rank ${r.baselineRank})`
          : `"${r.id}" fell from rank ${r.baselineRank} to rank ${r.variantRank}`
      ),
    };
  }

  if (wilcoxon.pValue < alpha) {
    return {
      decision: wilcoxon.effectSize > 0 ? 'improvement' : 'regression',
      reasons: [
        `Wilcoxon p=${wilcoxon.pValue.toFixed(4)} (${wilcoxon.method}, n=${wilcoxon.n}), ` +
          `effect size ${wilcoxon.effectSize.toFixed(2)}, ` +
          `mean ΔMRR ${formatSigned(comparison.meanMrrDelta)}`,
      ],
    };
  }

  // A fully flat result is a NULL result, not an underpowered one. Reporting
  // it as "cannot distinguish from noise" would misdiagnose it: nothing moved
  // at all, which is a much stronger statement than a non-significant test.
  if (wilcoxon.n === 0) {
    return {
      decision: 'inconclusive',
      reasons: [
        `No query changed rank — all ${wilcoxon.zeroPairs} pairs identical. ` +
          'Null result, not an underpowered one.',
      ],
    };
  }

  return {
    decision: 'inconclusive',
    reasons: [
      `Wilcoxon p=${wilcoxon.pValue.toFixed(4)} ≥ α=${alpha} over n=${wilcoxon.n} ` +
        `non-zero pairs (${wilcoxon.zeroPairs} unchanged) — cannot distinguish a ` +
        'small true effect from noise at this sample size',
    ],
  };
}

/** Format with an explicit sign so a delta always reads as a direction. */
export function formatSigned(value: number, digits = 4): string {
  // Normalize -0 so it never renders as "-0.0000".
  const normalized = value === 0 ? 0 : value;
  const text = normalized.toFixed(digits);
  return normalized > 0 ? `+${text}` : text;
}

/**
 * Wilcoxon signed-rank test over paired differences.
 *
 * Zero differences are dropped (Wilcoxon's original handling) and reported via
 * `zeroPairs` — at this dataset size the count of untouched queries is itself
 * informative. Ties among |d| get average ranks and STAY on the exact path
 * (see {@link exactTwoSidedP}); the normal approximation only enters past
 * {@link EXACT_MAX_N} non-zero pairs.
 */
export function wilcoxonSignedRank(differences: readonly number[]): WilcoxonResult {
  const nonZero = differences.filter((d) => d !== 0);
  const zeroPairs = differences.length - nonZero.length;
  const n = nonZero.length;

  if (n === 0) {
    return {
      n: 0,
      zeroPairs,
      wPlus: 0,
      wMinus: 0,
      pValue: 1,
      method: 'exact',
      effectSize: 0,
    };
  }

  const magnitudes = nonZero.map((d) => Math.abs(d));
  const { ranks, tieGroupSizes } = averageRanks(magnitudes);

  let wPlus = 0;
  let wMinus = 0;
  nonZero.forEach((d, i) => {
    if (d > 0) wPlus += ranks[i]!;
    else wMinus += ranks[i]!;
  });

  const useExact = n <= EXACT_MAX_N;
  const pValue = useExact
    ? exactTwoSidedP(wPlus, ranks)
    : normalApproxTwoSidedP(wPlus, n, tieGroupSizes);

  const total = wPlus + wMinus;
  return {
    n,
    zeroPairs,
    wPlus,
    wMinus,
    pValue,
    method: useExact ? 'exact' : 'normal-approximation',
    effectSize: total > 0 ? (wPlus - wMinus) / total : 0,
  };
}

/**
 * Ascending ranks with ties averaged, plus the size of each tie group (needed
 * for the variance correction in the normal approximation).
 */
function averageRanks(values: readonly number[]): { ranks: number[]; tieGroupSizes: number[] } {
  const order = values.map((value, index) => ({ value, index }));
  order.sort((a, b) => a.value - b.value);

  const ranks = new Array<number>(values.length);
  const tieGroupSizes: number[] = [];

  let i = 0;
  while (i < order.length) {
    let j = i;
    while (j + 1 < order.length && order[j + 1]!.value === order[i]!.value) j++;

    const groupSize = j - i + 1;
    tieGroupSizes.push(groupSize);
    // Ranks are 1-based; the shared rank is the mean of the positions spanned.
    const sharedRank = (i + 1 + (j + 1)) / 2;
    for (let k = i; k <= j; k++) ranks[order[k]!.index] = sharedRank;

    i = j + 1;
  }

  return { ranks, tieGroupSizes };
}

/**
 * Exact two-sided p-value from the permutation null distribution of W+,
 * conditioned on the observed |d| magnitudes.
 *
 * Under H0 each pair's sign is an independent fair coin, so W+ is the sum of a
 * uniformly random subset of the observed ranks. Average ranks are integers or
 * half-integers; scaling everything by 2 makes them integers, and the same
 * subset-sum DP applies unchanged — which is what keeps TIED magnitudes on the
 * exact path. That matters here specifically: MRR deltas are differences of
 * reciprocals of small integers, so ties are the norm (1/2−1/3 = 1/3−1/6 =
 * 1/6), and the normal approximation is at its worst precisely at the small n
 * this module exists for.
 *
 * O(n² · n) time worst case — trivially cheap at these sizes. Subset counts
 * reach 2^n ≤ 2^50, still exactly representable in float64 (< 2^53).
 */
function exactTwoSidedP(wPlus: number, ranks: readonly number[]): number {
  // ×2 turns half-integer average ranks into exact integers. Ranks are dyadic,
  // so the float arithmetic below is exact; round only guards dust.
  const scaled = ranks.map((r) => Math.round(2 * r));
  const maxW = scaled.reduce((acc, r) => acc + r, 0);
  // counts[w] = number of subsets of the scaled ranks seen so far summing to w.
  const counts = new Float64Array(maxW + 1);
  counts[0] = 1;
  for (const rank of scaled) {
    for (let w = maxW; w >= rank; w--) {
      counts[w] = counts[w]! + counts[w - rank]!;
    }
  }

  const scaledW = Math.round(2 * wPlus);
  const totalOutcomes = Math.pow(2, scaled.length);
  let atOrBelow = 0;
  for (let w = 0; w <= Math.min(scaledW, maxW); w++) atOrBelow += counts[w]!;
  let atOrAbove = 0;
  for (let w = Math.max(0, scaledW); w <= maxW; w++) atOrAbove += counts[w]!;

  const oneSided = Math.min(atOrBelow, atOrAbove) / totalOutcomes;
  return Math.min(1, 2 * oneSided);
}

/**
 * Normal approximation with continuity correction and the standard tie
 * correction to the variance.
 */
function normalApproxTwoSidedP(
  wPlus: number,
  n: number,
  tieGroupSizes: readonly number[]
): number {
  const mu = (n * (n + 1)) / 4;
  const tieCorrection = tieGroupSizes.reduce((acc, t) => acc + (t * t * t - t), 0);
  const variance = (n * (n + 1) * (2 * n + 1)) / 24 - tieCorrection / 48;

  if (variance <= 0) return 1;

  const diff = wPlus - mu;
  // Continuity correction pulls the statistic toward the mean by half a step.
  const corrected = diff > 0 ? diff - 0.5 : diff + 0.5;
  // Overshooting the mean after correction means the raw difference was under
  // half a step — no evidence at all.
  const z = Math.sign(diff) === Math.sign(corrected) ? corrected / Math.sqrt(variance) : 0;

  return Math.min(1, 2 * (1 - standardNormalCdf(Math.abs(z))));
}

/** Φ(x) via the Abramowitz & Stegun 7.1.26 erf approximation (|ε| < 1.5e-7). */
function standardNormalCdf(x: number): number {
  return 0.5 * (1 + erf(x / Math.SQRT2));
}

function erf(x: number): number {
  const sign = Math.sign(x);
  const absX = Math.abs(x);

  const t = 1 / (1 + 0.3275911 * absX);
  const y =
    1 -
    ((((1.061405429 * t - 1.453152027) * t + 1.421413741) * t - 0.284496736) * t + 0.254829592) *
      t *
      Math.exp(-absX * absX);

  return sign * y;
}

function mean(values: readonly number[]): number {
  if (values.length === 0) return 0;
  return values.reduce((acc, v) => acc + v, 0) / values.length;
}

function median(values: readonly number[]): number {
  if (values.length === 0) return 0;
  const sorted = [...values].sort((a, b) => a - b);
  const mid = Math.floor(sorted.length / 2);
  return sorted.length % 2 === 0 ? (sorted[mid - 1]! + sorted[mid]!) / 2 : sorted[mid]!;
}
