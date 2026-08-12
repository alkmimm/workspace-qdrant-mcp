import { describe, expect, it } from 'vitest';

import {
  formatBenchmarkComparison,
  isSearchEvalResponse,
  searchEvalResponseToReport,
  unwrapToolEnvelope,
  type SearchEvalResponseLike,
} from '../../src/benchmarks/compare-io.js';
import { assessComparison, compareBenchmarkReports } from '../../src/benchmarks/compare.js';

function toolResponse(
  entries: Array<{ id: string; semanticRank: number | null; hybridRank?: number | null }>,
  semanticRecallPct = 80
): SearchEvalResponseLike {
  return {
    perQuery: entries.map((e) => ({
      id: e.id,
      query: `query for ${e.id}`,
      semantic: { firstRelevantRank: e.semanticRank },
      hybrid: { firstRelevantRank: e.hybridRank ?? null },
    })),
    modes: { semantic: { recallAt10: semanticRecallPct } },
  };
}

describe('isSearchEvalResponse', () => {
  it('recognizes a tool response by its perQuery array', () => {
    expect(isSearchEvalResponse(toolResponse([]))).toBe(true);
  });

  it('rejects full reports, null, and scalars', () => {
    expect(isSearchEvalResponse({ queries: [], summary: {} })).toBe(false);
    expect(isSearchEvalResponse(null)).toBe(false);
    expect(isSearchEvalResponse('perQuery')).toBe(false);
  });
});

describe('unwrapToolEnvelope', () => {
  it('descends into result for an admin tools/invoke envelope', () => {
    const inner = toolResponse([{ id: 'a', semanticRank: 1 }]);
    const envelope = { ok: true, tool: 'search_eval', latencyMs: 1234, result: inner };

    expect(unwrapToolEnvelope(envelope)).toBe(inner);
    expect(isSearchEvalResponse(unwrapToolEnvelope(envelope))).toBe(true);
  });

  it('leaves a bare tool response and a full report untouched', () => {
    const bare = toolResponse([{ id: 'a', semanticRank: 1 }]);
    expect(unwrapToolEnvelope(bare)).toBe(bare);

    const asReport = { queries: [], summary: { modes: {} } };
    expect(unwrapToolEnvelope(asReport)).toBe(asReport);
  });

  it('does not unwrap a payload that carries its own result key', () => {
    // A tool response that happens to have `result` must not be mistaken for
    // an envelope — the perQuery guard is what prevents losing the payload.
    const withResult = { ...toolResponse([{ id: 'a', semanticRank: 1 }]), result: 'noise' };
    expect(unwrapToolEnvelope(withResult)).toBe(withResult);
  });

  it('passes through non-objects', () => {
    expect(unwrapToolEnvelope(null)).toBeNull();
    expect(unwrapToolEnvelope('x')).toBe('x');
  });
});

describe('searchEvalResponseToReport', () => {
  it('recovers mrr as 1/firstRelevantRank, matching the harness formula', () => {
    const adapted = searchEvalResponseToReport(
      toolResponse([
        { id: 'ranked', semanticRank: 4 },
        { id: 'missed', semanticRank: null },
      ]),
      'semantic'
    );

    const [ranked, missed] = adapted.queries;
    expect(ranked!.modes.semantic!.evaluation).toEqual({ mrr: 0.25, firstRelevantRank: 4 });
    expect(missed!.modes.semantic!.evaluation).toEqual({ mrr: 0, firstRelevantRank: undefined });
  });

  it('normalizes the percentage-scaled recall back to a 0–1 rate', () => {
    const adapted = searchEvalResponseToReport(toolResponse([], 78.3), 'semantic');

    expect(adapted.summary.modes.semantic!.recallAt10).toBeCloseTo(0.783, 10);
  });

  it('adapts the hybrid ranks when asked for hybrid', () => {
    const adapted = searchEvalResponseToReport(
      toolResponse([{ id: 'a', semanticRank: 9, hybridRank: 2 }]),
      'hybrid'
    );

    expect(adapted.queries[0]!.modes.hybrid!.evaluation.mrr).toBe(0.5);
    expect(adapted.queries[0]!.modes.semantic).toBeUndefined();
  });

  it('produces reports the comparator consumes end-to-end', () => {
    const baseline = searchEvalResponseToReport(
      toolResponse([{ id: 'a', semanticRank: 5 }], 70),
      'semantic'
    );
    const variant = searchEvalResponseToReport(
      toolResponse([{ id: 'a', semanticRank: 2 }], 90),
      'semantic'
    );

    const comparison = compareBenchmarkReports(baseline, variant);

    expect(comparison.deltas[0]!.mrrDelta).toBeCloseTo(1 / 2 - 1 / 5, 10);
    expect(comparison.reportOnly.recallAt10Delta).toBeCloseTo(0.2, 10);
  });
});

describe('formatBenchmarkComparison', () => {
  it('renders the verdict, the paired counts, and flags unpaired ids', () => {
    const baseline = searchEvalResponseToReport(
      toolResponse([
        { id: 'a', semanticRank: 5 },
        { id: 'baseline-only', semanticRank: 1 },
      ]),
      'semantic'
    );
    const variant = searchEvalResponseToReport(toolResponse([{ id: 'a', semanticRank: 2 }]), 'semantic');

    const comparison = compareBenchmarkReports(baseline, variant);
    const text = formatBenchmarkComparison(comparison, assessComparison(comparison));

    expect(text).toContain('1 queries paired');
    expect(text).toContain('Verdict: INCONCLUSIVE');
    expect(text).toContain('baseline-only');
    expect(text).toContain('report-only, no decision power');
  });

  it('lists tail regressions with their rank movement', () => {
    const baseline = searchEvalResponseToReport(toolResponse([{ id: 'a', semanticRank: 2 }]), 'semantic');
    const variant = searchEvalResponseToReport(toolResponse([{ id: 'a', semanticRank: null }]), 'semantic');

    const comparison = compareBenchmarkReports(baseline, variant);
    const text = formatBenchmarkComparison(comparison, assessComparison(comparison));

    expect(text).toContain('Tail regressions (1):');
    expect(text).toContain('a: lost gold hit (was rank 2)');
    expect(text).toContain('Verdict: REGRESSION');
  });
});
