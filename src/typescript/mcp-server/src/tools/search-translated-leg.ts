/**
 * Decides whether a search should run a second, translated leg — and produces
 * the translated query when it should.
 *
 * Kept out of search.ts (already past the file-size guideline) and free of any
 * search machinery, so the whole decision is testable without a Qdrant client,
 * a daemon, or an index.
 *
 * See docs/plans/2026-08-11-query-translation-plan.md. Three gates, cheapest
 * first, because the common path is an English query that must pay nothing:
 *
 *   1. the feature flag (off by default — search stays byte-identical);
 *   2. the LOCAL language gate, no network (phase 1);
 *   3. the SLM round-trip, only for what survives both.
 */

import type { QueryTranslator } from '../clients/query-translator.js';
import { recordQueryLanguage, recordTranslationOutcome } from '../telemetry/metrics.js';
import { logDebug, logInfo } from '../utils/logger.js';
import { classifyQueryLanguage } from '../utils/query-language.js';

/** Env var that turns the translated leg on. Anything but "1" leaves it off. */
export const QUERY_TRANSLATE_ENV = 'WQM_QUERY_TRANSLATE';

/**
 * Whether the translated leg is enabled. Read per call rather than cached so a
 * container recreate is enough to flip it, matching how the other search knobs
 * (WQM_SEARCH_RERANK, WQM_SEARCH_DERANK) behave.
 */
export function translatedLegEnabled(env: NodeJS.ProcessEnv = process.env): boolean {
  return (env[QUERY_TRANSLATE_ENV] ?? '').trim() === '1';
}

export interface TranslatedQueryDecision {
  /** The English query to run as a second leg, or null for "single leg". */
  query: string | null;
  /** Why — for logs and for the response's observability fields. */
  reason: 'disabled' | 'no-translator' | 'already-english' | 'translation-failed' | 'translated';
}

/**
 * Resolve the translated query for `query`, or null when the search should run
 * with a single leg.
 *
 * Never throws: the translator itself fails open to null, and every other exit
 * here is a decision rather than an error. A search must never fail, or slow
 * down measurably, because translation was unavailable.
 */
export async function resolveTranslatedQuery(
  query: string,
  translator: QueryTranslator | null,
  env: NodeJS.ProcessEnv = process.env,
  actor?: string,
  isBenchmark = false
): Promise<TranslatedQueryDecision> {
  // Classify FIRST, even when the feature is off, and record it.
  //
  // The ordering is the whole point of the instrumentation: the language
  // verdict is what answers "would enabling this touch real traffic?", and it
  // has to be collected while the feature is still disabled or the question can
  // only ever be answered by turning it on and finding out. The gate is pure
  // and local, so this costs no I/O — just the classification the enabled path
  // would run anyway.
  const verdict = classifyQueryLanguage(query);
  recordQueryLanguage(verdict.isLikelyNonEnglish, actor, isBenchmark);

  /**
   * One structured line per ranked search, shipped to Loki by promtail.
   *
   * The metrics answer HOW MUCH; this answers WHICH — the actual query texts
   * behind the counters, which no metric can carry (query text as a Prometheus
   * label would be unbounded cardinality). Reading the real phrasings is what
   * tells you whether a non-English share is worth acting on, and lets a bad
   * translation be spotted next to the query that produced it.
   *
   * `query_text` is already persisted in search_events, so this is not a new
   * exposure — it is the same data on a channel Grafana can table.
   */
  const observe = (
    reason: TranslatedQueryDecision['reason'],
    translated?: string,
    elapsedMs?: number
  ): void => {
    logInfo('[query-language]', {
      verdict: verdict.isLikelyNonEnglish ? 'non_english' : 'english',
      reason,
      actor: isBenchmark ? 'benchmark' : (actor ?? 'other'),
      query: query.slice(0, 300),
      ...(translated !== undefined ? { translated: translated.slice(0, 300) } : {}),
      ...(elapsedMs !== undefined ? { translate_ms: elapsedMs } : {}),
      signal: verdict.reason,
    });
  };

  const decide = (reason: TranslatedQueryDecision['reason']): TranslatedQueryDecision => {
    recordTranslationOutcome(reason.replace(/-/g, '_'));
    observe(reason);
    return { query: null, reason };
  };

  if (!translatedLegEnabled(env)) return decide('disabled');
  if (!translator) return decide('no-translator');
  // An English query — the common case — must not pay for an SLM round-trip to
  // be told it is already English.
  if (!verdict.isLikelyNonEnglish) return decide('already-english');

  const startedAt = Date.now();
  const translated = await translator.translateToEnglish(query);
  const elapsedSeconds = (Date.now() - startedAt) / 1000;

  if (!translated) {
    logDebug(`Translated leg skipped: translator returned nothing for "${query}"`);
    recordTranslationOutcome('translation_failed', elapsedSeconds);
    observe('translation-failed', undefined, Math.round(elapsedSeconds * 1000));
    return { query: null, reason: 'translation-failed' };
  }

  recordTranslationOutcome('translated', elapsedSeconds);
  observe('translated', translated, Math.round(elapsedSeconds * 1000));
  return { query: translated, reason: 'translated' };
}
