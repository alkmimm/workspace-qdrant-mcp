/**
 * Input/presentation edges for the paired benchmark comparison.
 *
 * `compare.ts` stays pure computation over {@link ComparableReport}s; this
 * module owns the two edges around it:
 *
 * - adapting a saved `search_eval` MCP tool RESPONSE into that shape. The tool
 *   response is thinner than a full benchmark report: it carries
 *   `perQuery[].firstRelevantRank` but no mrr (recovered as 1/rank, exactly
 *   how the harness computes it), and its summary rates are shaped as 0–100
 *   PERCENTAGES (`shapeModeSummary`) where the full report uses 0–1 rates —
 *   a silent unit mismatch if consumed unnormalized;
 * - rendering a comparison + assessment as terminal text.
 *
 * Kept apart so the comparator never learns about tool-response quirks.
 */

import type {
  BenchmarkComparison,
  ComparableQueryRun,
  ComparableReport,
  ComparisonAssessment,
} from './compare.js';
import { formatSigned } from './compare.js';

/** Ranked modes the search_eval response carries per-query ranks for. */
export const SEARCH_EVAL_RANKED_MODES = ['semantic', 'hybrid'] as const;

export type SearchEvalRankedMode = (typeof SEARCH_EVAL_RANKED_MODES)[number];

interface SearchEvalPerQueryMode {
  firstRelevantRank: number | null;
}

/** The slice of a search_eval tool response this adapter consumes. */
export interface SearchEvalResponseLike {
  perQuery: Array<
    {
      id: string;
      query: string;
    } & Record<SearchEvalRankedMode, SearchEvalPerQueryMode>
  >;
  /** Mode summaries with PERCENTAGE-scaled rates (0–100). */
  modes?: Partial<Record<SearchEvalRankedMode, { recallAt10?: number }>>;
}

/**
 * Distinguish a saved search_eval tool response from a full benchmark report:
 * the tool response has `perQuery`, the report has `queries`.
 */
export function isSearchEvalResponse(value: unknown): value is SearchEvalResponseLike {
  return (
    typeof value === 'object' &&
    value !== null &&
    Array.isArray((value as { perQuery?: unknown }).perQuery)
  );
}

/**
 * Rebuild the minimal comparable shape from a saved search_eval tool response.
 *
 * MRR is recovered as `1/firstRelevantRank` — the exact formula the harness
 * uses (semantic-search.ts, `mrr = firstRelevantRank ? 1/firstRelevantRank :
 * 0`) — so an adapted comparison is identical to one over full reports. The
 * percentage-scaled `recallAt10` is normalized back to a 0–1 rate to match
 * the full-report scale.
 */
export function searchEvalResponseToReport(
  response: SearchEvalResponseLike,
  mode: SearchEvalRankedMode
): ComparableReport {
  const queries: ComparableQueryRun[] = response.perQuery.map((q) => {
    const rank = q[mode]?.firstRelevantRank ?? undefined;
    return {
      id: q.id,
      query: q.query,
      modes: {
        [mode]: {
          evaluation: {
            mrr: rank ? 1 / rank : 0,
            firstRelevantRank: rank ?? undefined,
          },
        },
      },
    };
  });

  const recallPct = response.modes?.[mode]?.recallAt10;
  return {
    queries,
    summary: {
      modes: {
        [mode]: { recallAt10: (recallPct ?? 0) / 100 },
      },
    },
  };
}

/** Render a 0–1 rate as a percentage with one decimal. */
function pct(rate: number): string {
  return `${(rate * 100).toFixed(1)}%`;
}

/** Render a comparison + assessment as compact terminal text. */
export function formatBenchmarkComparison(
  comparison: BenchmarkComparison,
  assessment: ComparisonAssessment
): string {
  const w = comparison.wilcoxon;
  const lines: string[] = [];

  lines.push(
    `Paired A/B — mode=${comparison.mode} · ${comparison.pairedQueries} queries paired · ` +
      `${comparison.improved} improved / ${comparison.regressed} regressed / ` +
      `${comparison.unchanged} unchanged`
  );
  lines.push(
    `ΔMRR: mean ${formatSigned(comparison.meanMrrDelta)} · ` +
      `median ${formatSigned(comparison.medianMrrDelta)}`
  );
  lines.push(
    `Wilcoxon: W+=${w.wPlus} W−=${w.wMinus} · n=${w.n} (${w.zeroPairs} zero) · ` +
      `p=${w.pValue.toFixed(4)} (${w.method}) · effect ${formatSigned(w.effectSize, 2)}`
  );
  lines.push(
    `recall@10 (report-only, no decision power): ` +
      `${pct(comparison.reportOnly.baselineRecallAt10)} → ` +
      `${pct(comparison.reportOnly.variantRecallAt10)} ` +
      `(Δ ${formatSigned(comparison.reportOnly.recallAt10Delta * 100, 1)}pp)`
  );

  if (comparison.regressions.length > 0) {
    lines.push(`Tail regressions (${comparison.regressions.length}):`);
    for (const r of comparison.regressions) {
      lines.push(
        r.lostEntirely
          ? `  - ${r.id}: lost gold hit (was rank ${r.baselineRank})`
          : `  - ${r.id}: rank ${r.baselineRank} → ${r.variantRank}`
      );
    }
  } else {
    lines.push('Tail regressions: none');
  }

  if (comparison.unpairedBaselineIds.length > 0 || comparison.unpairedVariantIds.length > 0) {
    lines.push(
      `⚠ Unpaired queries — baseline-only: [${comparison.unpairedBaselineIds.join(', ')}] · ` +
        `variant-only: [${comparison.unpairedVariantIds.join(', ')}] — the runs do not cover ` +
        'the same dataset; treat every number above with suspicion.'
    );
  }

  lines.push(`Verdict: ${assessment.decision.toUpperCase()}`);
  for (const reason of assessment.reasons) {
    lines.push(`  ${reason}`);
  }

  return lines.join('\n');
}
