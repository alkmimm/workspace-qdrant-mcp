import { describe, expect, it } from 'vitest';

import {
  assessComparison,
  compareBenchmarkReports,
  wilcoxonSignedRank,
  type ComparableQueryRun,
  type ComparableReport,
} from '../../src/benchmarks/compare.js';
import type { BenchmarkMode } from '../../src/benchmarks/semantic-search.js';

function queryRun(
  id: string,
  mrr: number,
  firstRelevantRank: number | undefined,
  mode: BenchmarkMode = 'semantic'
): ComparableQueryRun {
  return {
    id,
    query: `query for ${id}`,
    modes: { [mode]: { evaluation: { mrr, firstRelevantRank } } },
  };
}

function report(
  queries: ComparableQueryRun[],
  recallAt10 = 0,
  mode: BenchmarkMode = 'semantic'
): ComparableReport {
  return {
    queries,
    summary: { modes: { [mode]: { recallAt10 } } },
  };
}

describe('wilcoxonSignedRank', () => {
  // Hand-verifiable: |d| = 1..5 so ranks are 1..5 with no ties.
  // W+ = 1+3+5 = 9, W- = 2+4 = 6. Over the 32 subsets of {1..5} the null
  // distribution is symmetric with counts
  //   w:  0 1 2 3 4 5 6 7 8 9 10 11 12 13 14 15
  //   c:  1 1 1 2 2 3 3 3 3 3  3  2  2  1  1  1   (sums to 32)
  // so P(W+ >= 9) = 3+3+2+2+1+1+1 = 13/32 and the two-sided p is 26/32.
  it('computes the exact two-sided p-value against a hand-checked distribution', () => {
    const result = wilcoxonSignedRank([1, -2, 3, -4, 5]);

    expect(result.n).toBe(5);
    expect(result.zeroPairs).toBe(0);
    expect(result.wPlus).toBe(9);
    expect(result.wMinus).toBe(6);
    expect(result.method).toBe('exact');
    expect(result.pValue).toBeCloseTo(26 / 32, 10);
    expect(result.effectSize).toBeCloseTo((9 - 6) / 15, 10);
  });

  // The floor this whole module exists to expose: five queries ALL improving,
  // with zero regressions, still lands at p = 2/32 = 0.0625 two-sided.
  it('does not reach significance with five all-positive pairs', () => {
    const result = wilcoxonSignedRank([1, 2, 3, 4, 5]);

    expect(result.wPlus).toBe(15);
    expect(result.wMinus).toBe(0);
    expect(result.pValue).toBeCloseTo(2 / 32, 10);
    expect(result.pValue).toBeGreaterThan(0.05);
    expect(result.effectSize).toBe(1);
  });

  it('reaches significance at six all-positive pairs', () => {
    const result = wilcoxonSignedRank([1, 2, 3, 4, 5, 6]);

    expect(result.wPlus).toBe(21);
    expect(result.pValue).toBeCloseTo(2 / 64, 10);
    expect(result.pValue).toBeLessThan(0.05);
  });

  it('drops zero differences and reports how many', () => {
    const result = wilcoxonSignedRank([0, 1, 2, 0, 3, 4, 5, 0]);

    expect(result.n).toBe(5);
    expect(result.zeroPairs).toBe(3);
    // Identical to the all-positive five-pair case above.
    expect(result.wPlus).toBe(15);
    expect(result.pValue).toBeCloseTo(2 / 32, 10);
  });

  // Ties stay on the EXACT path via the ×2 rank scaling. Hand-verifiable:
  // |d| = [1,1,2,3] → average ranks [1.5, 1.5, 3, 4] → scaled [3,3,6,8].
  // W+ = 1.5+1.5+4 = 7 (scaled 14). The 16 sign-subsets of {3,3,6,8} sum to
  //   0:1  3:2  6:2  8:1  9:2  11:2  12:1  14:2  17:2  20:1
  // so P(W ≥ 14) = (2+2+1)/16 = 5/16 and the two-sided p is 10/16.
  it('keeps tied magnitudes on the exact path with average ranks', () => {
    const result = wilcoxonSignedRank([1, 1, -2, 3]);

    expect(result.method).toBe('exact');
    expect(result.wPlus).toBeCloseTo(7, 10);
    expect(result.wMinus).toBeCloseTo(3, 10);
    expect(result.pValue).toBeCloseTo(10 / 16, 10);
  });

  it('switches to the normal approximation only past the exact-size cap', () => {
    const deltas = Array.from({ length: 51 }, (_, i) => i + 1);
    const result = wilcoxonSignedRank(deltas);

    expect(result.method).toBe('normal-approximation');
    expect(result.pValue).toBeLessThan(0.001);
    expect(result.effectSize).toBe(1);
  });

  it('returns p = 1 when every pair is unchanged', () => {
    const result = wilcoxonSignedRank([0, 0, 0]);

    expect(result.n).toBe(0);
    expect(result.zeroPairs).toBe(3);
    expect(result.pValue).toBe(1);
    expect(result.effectSize).toBe(0);
  });

  it('is symmetric under sign inversion', () => {
    const positive = wilcoxonSignedRank([1, -2, 3, -4, 5]);
    const negated = wilcoxonSignedRank([-1, 2, -3, 4, -5]);

    expect(negated.pValue).toBeCloseTo(positive.pValue, 10);
    expect(negated.wPlus).toBeCloseTo(positive.wMinus, 10);
    expect(negated.effectSize).toBeCloseTo(-positive.effectSize, 10);
  });
});

describe('compareBenchmarkReports', () => {
  it('pairs queries by id and orients deltas so positive favours the variant', () => {
    const baseline = report([queryRun('a', 1 / 5, 5), queryRun('b', 1 / 2, 2)]);
    const variant = report([queryRun('a', 1 / 2, 2), queryRun('b', 1 / 4, 4)]);

    const comparison = compareBenchmarkReports(baseline, variant);

    expect(comparison.pairedQueries).toBe(2);
    expect(comparison.improved).toBe(1);
    expect(comparison.regressed).toBe(1);

    const [a, b] = comparison.deltas;
    // 'a' moved from rank 5 to rank 2 — better, so a POSITIVE mrr delta and a
    // NEGATIVE rank delta (rank counts downward).
    expect(a!.mrrDelta).toBeCloseTo(1 / 2 - 1 / 5, 10);
    expect(a!.rankDelta).toBe(-3);
    // 'b' fell from 2 to 4.
    expect(b!.mrrDelta).toBeCloseTo(1 / 4 - 1 / 2, 10);
    expect(b!.rankDelta).toBe(2);
  });

  it('surfaces unpaired query ids on both sides instead of dropping them', () => {
    const baseline = report([queryRun('shared', 0.5, 2), queryRun('only-baseline', 0.5, 2)]);
    const variant = report([queryRun('shared', 0.5, 2), queryRun('only-variant', 0.5, 2)]);

    const comparison = compareBenchmarkReports(baseline, variant);

    expect(comparison.pairedQueries).toBe(1);
    expect(comparison.unpairedBaselineIds).toEqual(['only-baseline']);
    expect(comparison.unpairedVariantIds).toEqual(['only-variant']);
  });

  it('throws on duplicate query ids instead of silently overwriting', () => {
    const baseline = report([queryRun('dup', 0.5, 2), queryRun('dup', 1, 1)]);
    const variant = report([queryRun('dup', 0.5, 2)]);

    expect(() => compareBenchmarkReports(baseline, variant)).toThrow(
      /Duplicate query id "dup" in the baseline report/
    );
  });

  it('flags a query that lost its gold hit entirely as a regression', () => {
    const baseline = report([queryRun('a', 1 / 3, 3)]);
    const variant = report([queryRun('a', 0, undefined)]);

    const comparison = compareBenchmarkReports(baseline, variant);
    const [delta] = comparison.deltas;

    expect(delta!.lostEntirely).toBe(true);
    expect(delta!.gainedEntirely).toBe(false);
    // Not comparable as a position shift — the variant has no position at all.
    expect(delta!.rankDelta).toBeNull();
    expect(comparison.regressions).toHaveLength(1);
  });

  it('does not count a newly-found gold hit as a regression', () => {
    const baseline = report([queryRun('a', 0, undefined)]);
    const variant = report([queryRun('a', 1 / 3, 3)]);

    const comparison = compareBenchmarkReports(baseline, variant);

    expect(comparison.deltas[0]!.gainedEntirely).toBe(true);
    expect(comparison.regressions).toHaveLength(0);
  });

  it('applies the rank-regression tolerance to the tail guard only', () => {
    // Falls 2 positions — inside the default tolerance of 3.
    const baseline = report([queryRun('a', 1 / 2, 2)]);
    const variant = report([queryRun('a', 1 / 4, 4)]);

    expect(compareBenchmarkReports(baseline, variant).regressions).toHaveLength(0);
    // ...but still counts as regressed in the aggregate.
    expect(compareBenchmarkReports(baseline, variant).regressed).toBe(1);
    // Tightening the tolerance promotes it to a tail regression.
    expect(
      compareBenchmarkReports(baseline, variant, { maxRankRegression: 1 }).regressions
    ).toHaveLength(1);
  });

  it('catches a positive mean hiding a bad tail', () => {
    const baseline = report([
      queryRun('a', 1 / 4, 4),
      queryRun('b', 1 / 4, 4),
      queryRun('c', 1 / 1, 1),
    ]);
    const variant = report([
      queryRun('a', 1 / 1, 1),
      queryRun('b', 1 / 1, 1),
      queryRun('c', 0, undefined),
    ]);

    const comparison = compareBenchmarkReports(baseline, variant);

    expect(comparison.meanMrrDelta).toBeGreaterThan(0);
    expect(comparison.regressions.map((r) => r.id)).toEqual(['c']);
  });

  it('reports recall@10 without letting it gate anything', () => {
    const baseline = report([queryRun('a', 0.5, 2)], 0.7);
    const variant = report([queryRun('a', 0.5, 2)], 0.9);

    const comparison = compareBenchmarkReports(baseline, variant);

    expect(comparison.reportOnly.recallAt10Delta).toBeCloseTo(0.2, 10);
    // recall moved, but no query changed rank — the decision signal is flat.
    expect(comparison.wilcoxon.pValue).toBe(1);
  });

  it('throws rather than silently comparing different modes', () => {
    const baseline = report([queryRun('a', 0.5, 2, 'semantic')]);
    const variant = report([queryRun('a', 0.5, 2, 'hybrid')]);

    expect(() => compareBenchmarkReports(baseline, variant)).toThrow(/no "semantic" mode run/);
  });
});

describe('assessComparison', () => {
  /** N queries improving rank 4 → 2 (distinct-magnitude deltas need jitter). */
  function improvingPair(n: number): { baseline: ComparableReport; variant: ComparableReport } {
    const ids = Array.from({ length: n }, (_, i) => `q${i}`);
    return {
      // Distinct baseline ranks give distinct |Δ| so the case stays tie-free.
      baseline: report(ids.map((id, i) => queryRun(id, 1 / (i + 2), i + 2))),
      variant: report(ids.map((id) => queryRun(id, 1, 1))),
    };
  }

  it('stays inconclusive at five all-improving pairs — the sample-size floor', () => {
    const { baseline, variant } = improvingPair(5);

    const assessment = assessComparison(compareBenchmarkReports(baseline, variant));

    expect(assessment.decision).toBe('inconclusive');
    expect(assessment.reasons[0]).toMatch(/cannot distinguish/);
  });

  it('declares improvement at six all-improving pairs', () => {
    const { baseline, variant } = improvingPair(6);

    const assessment = assessComparison(compareBenchmarkReports(baseline, variant));

    expect(assessment.decision).toBe('improvement');
    expect(assessment.reasons[0]).toMatch(/p=0\.0313/);
  });

  it('lets the tail guard veto an otherwise significant improvement', () => {
    const { baseline, variant } = improvingPair(6);
    const baselineWithTail = report([...baseline.queries, queryRun('tail', 1 / 2, 2)]);
    const variantWithTail = report([...variant.queries, queryRun('tail', 0, undefined)]);

    const assessment = assessComparison(
      compareBenchmarkReports(baselineWithTail, variantWithTail)
    );

    expect(assessment.decision).toBe('regression');
    expect(assessment.reasons[0]).toMatch(/"tail" lost its gold hit/);
  });

  it('detects a significant consistent slide as regression without any tail event', () => {
    // Six queries each slipping rank 1 → 2: within the tail tolerance, all
    // deltas tied at −1/2 — also exercises the tied exact path end-to-end.
    const ids = ['a', 'b', 'c', 'd', 'e', 'f'];
    const baseline = report(ids.map((id) => queryRun(id, 1, 1)));
    const variant = report(ids.map((id) => queryRun(id, 1 / 2, 2)));

    const comparison = compareBenchmarkReports(baseline, variant);
    const assessment = assessComparison(comparison);

    expect(comparison.regressions).toHaveLength(0);
    expect(comparison.wilcoxon.method).toBe('exact');
    expect(assessment.decision).toBe('regression');
  });

  it('calls a fully flat comparison a null result, not an underpowered one', () => {
    // The A-vs-A case (re-running the same eval twice). Distinguishing this
    // from "underpowered" matters: nothing moved is a stronger statement than
    // a non-significant test, and the two point at different next actions.
    const queries = ['a', 'b', 'c'].map((id) => queryRun(id, 1 / 2, 2));
    const comparison = compareBenchmarkReports(report(queries), report(queries));

    const assessment = assessComparison(comparison);

    expect(comparison.wilcoxon.n).toBe(0);
    expect(assessment.decision).toBe('inconclusive');
    expect(assessment.reasons[0]).toMatch(/all 3 pairs identical/);
    expect(assessment.reasons[0]).toMatch(/Null result/);
    expect(assessment.reasons[0]).not.toMatch(/cannot distinguish/);
  });

  it('honours a custom alpha', () => {
    const { baseline, variant } = improvingPair(5);

    const assessment = assessComparison(compareBenchmarkReports(baseline, variant), {
      alpha: 0.1,
    });

    // p = 0.0625 < 0.1 — significant at the looser level.
    expect(assessment.decision).toBe('improvement');
  });
});
