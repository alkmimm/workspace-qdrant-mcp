/**
 * Effectiveness signals for search instrumentation (spec 20 §1.2).
 *
 * The `token_savings` view derives two per-event quality signals:
 *   - `had_followup`:   a search re-issued soon after with overlapping terms
 *                       (the first call didn't suffice);
 *   - `had_escalation`: a retrieve of a document a recent search returned
 *                       (the trimmed payload wasn't enough).
 * Both depend on writers that never existed: nothing ever wrote
 * `op='followup'`, `parent_event_id`, or `session_id` (all NULL/0 across
 * 8.9k live events), so the rates were structurally zero. This module
 * supplies the missing write-time classification:
 *
 *   - a per-request MCP session key (AsyncLocalStorage, set by the HTTP
 *     transport dispatch) with a per-process fallback so stdio mode still
 *     groups events into one session;
 *   - an in-memory ring of recent queries per session — a search whose
 *     terms overlap one issued < FOLLOWUP_WINDOW earlier is written as
 *     `op='followup'` with `parent_event_id` = the origin event;
 *   - hit refs (document ids / file paths) attached per search so a
 *     retrieve < ESCALATION_WINDOW later links back via `parent_event_id`.
 *
 * All state is in-memory and best-effort: classification never blocks a
 * search, and a server restart simply forgets the window — signals degrade
 * to "no signal", never to wrong data. Ref matching is exact-string, so a
 * retrieve by a path spelling the search never returned (absolute vs
 * relative) is missed rather than mislinked.
 */

import { randomUUID } from 'node:crypto';
import { getRequestContext } from '../utils/request-context.js';

/** Spec 20 §1.2 FOLLOWUP_WINDOW. Mirrored by the `token_savings` view
 *  (julianday delta ≤ 0.000694). */
export const FOLLOWUP_WINDOW_MS = 60_000;
/** Spec 20 §1.2 ESCALATION_WINDOW. Mirrored by the view (≤ 0.001389). */
export const ESCALATION_WINDOW_MS = 120_000;

/** Stable fallback: stdio mode has exactly one client per process, so a
 *  per-process id is an honest session key there. */
const processSessionId = randomUUID();

/** The MCP session id of the current request (bound into the request
 *  context by the HTTP transport), or the per-process fallback. Never
 *  empty. */
export function currentSessionId(): string {
  return getRequestContext()?.mcpSessionId ?? processSessionId;
}

interface RecentQuery {
  eventId: string;
  sessionKey: string;
  tokens: Set<string>;
  refs: Set<string>;
  tsMs: number;
}

/** Ring size: bounds memory; 100 queries comfortably outlasts both windows. */
const MAX_RECENT = 100;
/** Tokens shorter than this carry no overlap signal ("a", "of", "rs"). */
const MIN_TOKEN_LEN = 3;
/** Single-token overlap needs at least this length to count — short shared
 *  words ("file", "code", "grep") are near-universal in this domain and link
 *  unrelated queries. */
const MIN_SINGLE_TOKEN_LEN = 5;

/** English function words that pass the length filter but carry no topical
 *  signal. Agent queries are natural-language sentences, so without this any
 *  two consecutive questions overlap on "where"/"the"/"does" and every pair
 *  classifies as a followup. */
const STOPWORDS = new Set([
  'the', 'and', 'for', 'are', 'was', 'were', 'not', 'you', 'all', 'any',
  'can', 'has', 'have', 'had', 'this', 'that', 'these', 'those', 'with',
  'from', 'into', 'onto', 'when', 'where', 'which', 'what', 'who', 'whom',
  'why', 'how', 'does', 'did', 'done', 'will', 'would', 'should', 'could',
  'there', 'their', 'then', 'than', 'them', 'they', 'its', 'his', 'her',
  'our', 'your', 'one', 'two', 'per', 'via', 'use', 'used', 'using',
]);

/** Lowercased identifier-ish tokens of a query, for overlap comparison.
 *  Stopwords are dropped — they pass the length filter but make unrelated
 *  natural-language queries "overlap". */
export function queryTokens(queryText: string): Set<string> {
  const out = new Set<string>();
  for (const tok of queryText.toLowerCase().split(/[^a-z0-9_]+/)) {
    if (tok.length >= MIN_TOKEN_LEN && !STOPWORDS.has(tok)) out.add(tok);
  }
  return out;
}

/** Overlap rule tuned for precision: two shared topical tokens, or one
 *  shared token long enough to be identifier-ish (>= MIN_SINGLE_TOKEN_LEN).
 *  A single short shared word is too weak a signal — it links unrelated
 *  same-session queries and inflates followup_rate. */
function tokensOverlap(a: Set<string>, b: Set<string>): boolean {
  let shared = 0;
  for (const t of a) {
    if (!b.has(t)) continue;
    shared += 1;
    if (shared >= 2 || t.length >= MIN_SINGLE_TOKEN_LEN) return true;
  }
  return false;
}

export class EffectivenessTracker {
  /** Time-ordered (append-only ring): entries are pushed with the caller's
   *  clock, so reverse iteration can stop at the first out-of-window entry. */
  private recent: RecentQuery[] = [];

  /**
   * Record a query and return the eventId of the most recent same-session
   * query with overlapping terms inside FOLLOWUP_WINDOW — the followup
   * origin — or undefined. The new query is recorded regardless, so a
   * chain of refinements links each hop to its predecessor.
   */
  noteQuery(
    eventId: string,
    sessionKey: string,
    queryText: string,
    tsMs: number
  ): string | undefined {
    const tokens = queryTokens(queryText);
    let origin: RecentQuery | undefined;
    if (tokens.size > 0) {
      for (let i = this.recent.length - 1; i >= 0; i--) {
        const cand = this.recent[i]!;
        if (tsMs - cand.tsMs > FOLLOWUP_WINDOW_MS) break;
        if (cand.sessionKey !== sessionKey || tsMs < cand.tsMs) continue;
        if (tokensOverlap(tokens, cand.tokens)) {
          origin = cand;
          break;
        }
      }
    }
    this.push({ eventId, sessionKey, tokens, refs: new Set(), tsMs });
    return origin?.eventId;
  }

  /** Attach hit refs (document ids, absolute AND relative file paths) to an
   *  already-noted query, once its results are known. */
  noteHits(eventId: string, refs: Iterable<string>): void {
    // Search backwards: the event being finished is almost always recent.
    for (let i = this.recent.length - 1; i >= 0; i--) {
      const entry = this.recent[i]!;
      if (entry.eventId !== eventId) continue;
      for (const r of refs) if (r) entry.refs.add(r);
      return;
    }
  }

  /**
   * Find the most recent same-session query inside ESCALATION_WINDOW whose
   * recorded hits include one of `refs` — the search a retrieve escalated
   * from — or undefined.
   */
  findOrigin(sessionKey: string, refs: Iterable<string>, tsMs: number): string | undefined {
    const wanted: string[] = [];
    for (const r of refs) if (r) wanted.push(r);
    if (wanted.length === 0) return undefined;
    for (let i = this.recent.length - 1; i >= 0; i--) {
      const cand = this.recent[i]!;
      if (tsMs - cand.tsMs > ESCALATION_WINDOW_MS) break;
      if (cand.sessionKey !== sessionKey || tsMs < cand.tsMs) continue;
      for (const w of wanted) {
        if (cand.refs.has(w)) return cand.eventId;
      }
    }
    return undefined;
  }

  private push(entry: RecentQuery): void {
    // Monotonize: reverse iteration `break`s at the first out-of-window entry,
    // which requires non-decreasing tsMs — but Date.now() is wall-clock and
    // can step backwards (NTP correction). Clamp instead of trusting it.
    const last = this.recent[this.recent.length - 1];
    if (last !== undefined && entry.tsMs < last.tsMs) entry.tsMs = last.tsMs;
    this.recent.push(entry);
    if (this.recent.length > MAX_RECENT) {
      this.recent.splice(0, this.recent.length - MAX_RECENT);
    }
  }
}

/** Process-wide tracker: sessions are separated by the sessionKey inside. */
export const effectivenessTracker = new EffectivenessTracker();
