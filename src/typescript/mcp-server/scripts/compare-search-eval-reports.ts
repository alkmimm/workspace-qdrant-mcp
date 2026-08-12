#!/usr/bin/env node

/**
 * Paired A/B comparison of two saved search-eval runs (baseline vs variant).
 *
 * Each side accepts either input shape, auto-detected:
 *  - a full benchmark report JSON (output of scripts/benchmark-semantic-search.ts)
 *  - a saved `search_eval` MCP tool response JSON (has `perQuery`)
 *
 * The decision criterion is the paired per-query MRR delta under Wilcoxon
 * signed-rank with a tail-regression guard — see
 * docs/plans/2026-08-10-contextual-chunk-headers-plan.md §5 R3 for why hit-rate
 * deltas cannot gate at this dataset size.
 *
 * Usage:
 *   tsx scripts/compare-search-eval-reports.ts <baseline.json> <variant.json> \
 *     [--mode semantic|hybrid|exact] [--max-rank-regression 3] [--alpha 0.05]
 *
 * Exits 0 whichever way the verdict goes — a measurement is not a failure.
 * Exits 2 on unusable input.
 */

import { readFileSync } from 'node:fs';
import { parseArgs } from 'node:util';

import {
  assessComparison,
  compareBenchmarkReports,
  type ComparableReport,
} from '../src/benchmarks/compare.js';
import {
  formatBenchmarkComparison,
  isSearchEvalResponse,
  searchEvalResponseToReport,
  unwrapToolEnvelope,
  SEARCH_EVAL_RANKED_MODES,
  type SearchEvalRankedMode,
} from '../src/benchmarks/compare-io.js';
import type { BenchmarkMode } from '../src/benchmarks/semantic-search.js';

function usageError(message: string): never {
  console.error(`Error: ${message}`);
  console.error(
    'Usage: tsx scripts/compare-search-eval-reports.ts <baseline.json> <variant.json> ' +
      '[--mode semantic|hybrid|exact] [--max-rank-regression N] [--alpha 0.05]'
  );
  process.exit(2);
}

/** Load one side, adapting a tool response when detected. */
function loadSide(path: string, mode: BenchmarkMode): ComparableReport {
  let parsed: unknown;
  try {
    parsed = unwrapToolEnvelope(JSON.parse(readFileSync(path, 'utf8')));
  } catch (error) {
    usageError(`Cannot read/parse ${path}: ${error instanceof Error ? error.message : error}`);
  }

  if (isSearchEvalResponse(parsed)) {
    if (!(SEARCH_EVAL_RANKED_MODES as readonly string[]).includes(mode)) {
      usageError(
        `${path} is a search_eval tool response, which carries per-query ranks only for ` +
          `${SEARCH_EVAL_RANKED_MODES.join('/')} — mode "${mode}" needs a full report JSON.`
      );
    }
    return searchEvalResponseToReport(parsed, mode as SearchEvalRankedMode);
  }

  const asReport = parsed as ComparableReport;
  if (!Array.isArray(asReport?.queries)) {
    usageError(`${path} is neither a benchmark report (queries[]) nor a tool response (perQuery[]).`);
  }
  return asReport;
}

function main(): void {
  const { values, positionals } = parseArgs({
    allowPositionals: true,
    options: {
      mode: { type: 'string', default: 'semantic' },
      'max-rank-regression': { type: 'string' },
      alpha: { type: 'string' },
    },
  });

  if (positionals.length !== 2) {
    usageError('Expected exactly two positional arguments: <baseline.json> <variant.json>.');
  }
  const mode = values.mode as BenchmarkMode;
  if (!['semantic', 'hybrid', 'exact'].includes(mode)) {
    usageError(`Unknown --mode "${values.mode}".`);
  }

  const maxRankRegression =
    values['max-rank-regression'] === undefined
      ? undefined
      : Number.parseInt(values['max-rank-regression'], 10);
  if (maxRankRegression !== undefined && (!Number.isInteger(maxRankRegression) || maxRankRegression < 0)) {
    usageError('--max-rank-regression must be a non-negative integer.');
  }

  const alpha = values.alpha === undefined ? undefined : Number.parseFloat(values.alpha);
  if (alpha !== undefined && !(alpha > 0 && alpha < 1)) {
    usageError('--alpha must be a number strictly between 0 and 1.');
  }

  const baseline = loadSide(positionals[0]!, mode);
  const variant = loadSide(positionals[1]!, mode);

  const comparison = compareBenchmarkReports(baseline, variant, {
    mode,
    ...(maxRankRegression !== undefined ? { maxRankRegression } : {}),
  });
  const assessment = assessComparison(comparison, {
    ...(alpha !== undefined ? { alpha } : {}),
  });

  console.log(formatBenchmarkComparison(comparison, assessment));
}

main();
