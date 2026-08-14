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
import { logDebug } from '../utils/logger.js';
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
  env: NodeJS.ProcessEnv = process.env
): Promise<TranslatedQueryDecision> {
  if (!translatedLegEnabled(env)) return { query: null, reason: 'disabled' };
  if (!translator) return { query: null, reason: 'no-translator' };

  // Local gate before the network hop: an English query — the common case —
  // must not pay for an SLM round-trip to be told it is already English.
  const verdict = classifyQueryLanguage(query);
  if (!verdict.isLikelyNonEnglish) return { query: null, reason: 'already-english' };

  const translated = await translator.translateToEnglish(query);
  if (!translated) {
    logDebug(`Translated leg skipped: translator returned nothing for "${query}"`);
    return { query: null, reason: 'translation-failed' };
  }

  logDebug(`Translated leg: "${query}" -> "${translated}" (${verdict.reason})`);
  return { query: translated, reason: 'translated' };
}
