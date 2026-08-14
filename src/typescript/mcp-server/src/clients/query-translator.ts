/**
 * Query translation client (PT->EN) for the search path.
 *
 * WHY — docs/plans/2026-08-11-query-translation-plan.md. A Portuguese query
 * against this English code corpus loses to language homophily: Portuguese
 * docs are 14.5x over-represented in its top-10. Measured ceiling of feeding
 * the search an ideal English translation: the `pt-` bucket goes 72.7% -> 90.9%
 * top-10, and an additive fusion of both legs reaches 93.9%.
 *
 * That last number is why this returns a translation to be searched ALONGSIDE
 * the original rather than instead of it: substituting outright regressed 5
 * queries, one of which lost its gold hit entirely (`pt-chunking-arvore`,
 * rank 4 -> miss). The original leg is what protects those.
 *
 * Speaks the OpenAI-compatible `/v1/chat/completions` of the `llm-gpu` sidecar,
 * mirroring the transport conventions of the daemon's RemoteReranker:
 * non-blocking construction, `{base_url}/v1/<route>` derivation, bounded
 * timeout, no Authorization header (this targets the unauthenticated in-stack
 * sidecar, not a public SaaS).
 *
 * FAIL-OPEN IS THE CONTRACT. Every failure path — unset config, HTTP error,
 * timeout, malformed body, empty or implausible output — returns `null`, and
 * the caller searches with the original query alone, exactly as today. Search
 * must never block on, or fail because of, translation. This runs at QUERY
 * TIME, so the timeout is deliberately short.
 */

import { logDebug } from '../utils/logger.js';
import { classifyQueryLanguage } from '../utils/query-language.js';

/** Env var that activates translation (endpoint base URL, no path). */
export const TRANSLATE_BASE_URL_ENV = 'WQM_TRANSLATE_BASE_URL';

/** Env var selecting the served model id. */
export const TRANSLATE_MODEL_ENV = 'WQM_TRANSLATE_MODEL';

/**
 * Request timeout. A 1.5B model emitting ~15 tokens answers in well under a
 * second on GPU; this only guards against a wedged endpoint. Kept short
 * because every millisecond here is added to a p50 ~90ms search.
 */
const DEFAULT_TIMEOUT_MS = 3000;

/**
 * Upper bound on an accepted translation, relative to the input. A model that
 * starts explaining itself, or echoes the prompt, produces something far
 * longer than a translated query — cheaper to reject than to embed.
 */
const MAX_LENGTH_RATIO = 3;

/** Absolute ceiling regardless of input length. */
const MAX_TRANSLATION_CHARS = 400;

const SYSTEM_PROMPT =
  'You translate software-engineering search queries into English. ' +
  'Reply with the translation ONLY — no quotes, no explanation, no preamble. ' +
  'Keep code identifiers, file paths, and technical terms exactly as written.';

/** Minimal shape consumed from an OpenAI-compatible chat completion. */
interface ChatCompletionResponse {
  choices?: Array<{ message?: { content?: unknown } }>;
}

/** Injectable for tests; defaults to global fetch. */
export type FetchLike = (url: string, init: RequestInit) => Promise<Response>;

export interface QueryTranslatorOptions {
  baseUrl: string;
  model: string;
  timeoutMs?: number;
  fetchImpl?: FetchLike;
}

export class QueryTranslator {
  private readonly endpoint: string;
  private readonly model: string;
  private readonly timeoutMs: number;
  private readonly fetchImpl: FetchLike;

  constructor(options: QueryTranslatorOptions) {
    this.endpoint = `${options.baseUrl.replace(/\/+$/, '')}/v1/chat/completions`;
    this.model = options.model;
    this.timeoutMs = options.timeoutMs ?? DEFAULT_TIMEOUT_MS;
    this.fetchImpl = options.fetchImpl ?? ((url, init) => fetch(url, init));
  }

  /** Full endpoint URL (for logs/labels). */
  get endpointUrl(): string {
    return this.endpoint;
  }

  /**
   * Translate `query` to English, or return `null` when translation is
   * unavailable or its output is not usable. Never throws.
   */
  async translateToEnglish(query: string): Promise<string | null> {
    const trimmed = query.trim();
    if (trimmed.length === 0) return null;

    const controller = new AbortController();
    const timer = setTimeout(() => controller.abort(), this.timeoutMs);
    try {
      const response = await this.fetchImpl(this.endpoint, {
        method: 'POST',
        headers: { 'Content-Type': 'application/json' },
        signal: controller.signal,
        body: JSON.stringify({
          model: this.model,
          // Greedy: the same query must translate the same way every time, or
          // an A/B measurement compares two moving targets.
          temperature: 0,
          max_tokens: 128,
          messages: [
            { role: 'system', content: SYSTEM_PROMPT },
            { role: 'user', content: trimmed },
          ],
        }),
      });

      if (!response.ok) {
        logDebug(`Query translation failed: HTTP ${response.status}`);
        return null;
      }

      const parsed = (await response.json()) as ChatCompletionResponse;
      const raw = parsed.choices?.[0]?.message?.content;
      if (typeof raw !== 'string') {
        logDebug('Query translation failed: no string content in response');
        return null;
      }

      return sanitizeTranslation(raw, trimmed);
    } catch (error) {
      // Includes the AbortError from the timeout — all equally "no translation".
      logDebug(`Query translation error: ${error instanceof Error ? error.message : error}`);
      return null;
    } finally {
      clearTimeout(timer);
    }
  }
}

/**
 * Reduce a model reply to a usable query, or `null`.
 *
 * Small instruct models wrap answers in quotes, prepend "Translation:", or
 * append a rationale on later lines. Taking the first non-empty line and
 * stripping those affixes recovers the query; anything still implausible is
 * rejected rather than embedded, since a bad translated leg costs a search.
 */
export function sanitizeTranslation(raw: string, original: string): string | null {
  const firstLine = raw
    .split('\n')
    .map((line) => line.trim())
    .find((line) => line.length > 0);
  if (!firstLine) return null;

  let cleaned = firstLine
    .replace(/^(translation|english|translated)\s*:\s*/i, '')
    .replace(/^["'`]+|["'`]+$/g, '')
    .trim();

  if (cleaned.length === 0) return null;
  if (cleaned.length > MAX_TRANSLATION_CHARS) return null;
  if (cleaned.length > original.length * MAX_LENGTH_RATIO) return null;

  // An echo of the input is not a translation — searching it twice would just
  // double the cost of the identical leg.
  if (cleaned.toLowerCase() === original.toLowerCase()) return null;

  // Still in the source language? Then it is not a translation, whatever else
  // it is. Measured against the live 1.5B sidecar (2026-08-12): of 8 failing
  // benchmark queries it echoed one verbatim (caught above) and ANSWERED
  // another in Portuguese — "Os pontos com embeddings são enviados para o
  // Qdrant através de uma API..." — which is not an echo and sits well under
  // the length cap, so every earlier check passes it. Searching that as the
  // "translated" leg would spend a query re-running the language the original
  // leg already covers. Reusing the phase-1 gate keeps one definition of
  // "looks non-English" for both routing and validation.
  if (classifyQueryLanguage(cleaned).isLikelyNonEnglish) return null;

  return cleaned;
}

/**
 * Build a translator from the environment, or `null` when
 * {@link TRANSLATE_BASE_URL_ENV} is unset/blank — the default, which leaves the
 * search path byte-identical to today.
 */
export function createQueryTranslatorFromEnv(
  env: NodeJS.ProcessEnv = process.env,
  fetchImpl?: FetchLike
): QueryTranslator | null {
  const baseUrl = (env[TRANSLATE_BASE_URL_ENV] ?? '').trim();
  if (baseUrl.length === 0) return null;

  const model = (env[TRANSLATE_MODEL_ENV] ?? '').trim();
  if (model.length === 0) {
    logDebug(`${TRANSLATE_BASE_URL_ENV} is set but ${TRANSLATE_MODEL_ENV} is empty — translation off`);
    return null;
  }

  return new QueryTranslator({
    baseUrl,
    model,
    ...(fetchImpl ? { fetchImpl } : {}),
  });
}
