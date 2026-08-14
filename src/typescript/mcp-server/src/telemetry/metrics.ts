/**
 * Prometheus metrics for the workspace-qdrant MCP server.
 *
 * Defines 12 metric families:
 *   wqm_mcp_tool_invocations_total     - Counter, labels [tool, status]
 *   wqm_mcp_tool_duration_seconds      - Histogram, label [tool], buckets [0.01…5]
 *   wqm_mcp_session_count              - Gauge
 *   wqm_mcp_daemon_fallback_total      - Counter, labels [tool, reason]
 *   wqm_mcp_multiclone_search_total    - Counter, label [degraded]
 *   wqm_mcp_cache_hits_total           - Counter, label [cache]
 *   wqm_mcp_cache_misses_total         - Counter, label [cache]
 *   wqm_mcp_http_requests_total        - Counter, labels [path, status_class]
 *   wqm_mcp_http_auth_failures_total   - Counter, label [reason]
 *   wqm_mcp_http_rate_limited_total    - Counter (no labels; single signal)
 *   wqm_git_hook_invocations_total     - Counter, labels [hook, result, is_worktree]
 *   wqm_git_hook_duration_seconds      - Histogram, labels [hook, result], buckets [0.05…30]
 *
 * Cache hit/miss counters are defined but unwired: no cache layer exists in
 * the MCP server at v0.1.3. They are ready for future use.
 *
 * Daemon-fallback counter is wired to gRPC error paths in DaemonClient callers.
 * For operations that have no current fallback path (e.g. store queue), the
 * counter is still defined so dashboards can reference it.
 */

import { Counter, Histogram, Gauge, Registry } from 'prom-client';
import { pushMetricsToOTLP } from './otlp.js';

/** Shared Prometheus registry for this MCP server process. */
export const register = new Registry();

// ── Metric definitions ──────────────────────────────────────────────────────

export const toolInvocations = new Counter({
  name: 'wqm_mcp_tool_invocations_total',
  help: 'Total MCP tool invocations by tool name and completion status',
  labelNames: ['tool', 'status'] as const,
  registers: [register],
});

export const toolDuration = new Histogram({
  name: 'wqm_mcp_tool_duration_seconds',
  help: 'MCP tool execution duration in seconds',
  labelNames: ['tool'] as const,
  buckets: [0.01, 0.05, 0.1, 0.5, 1, 5],
  registers: [register],
});

export const sessionCount = new Gauge({
  name: 'wqm_mcp_session_count',
  help: 'Number of active MCP sessions',
  registers: [register],
});

export const daemonFallback = new Counter({
  name: 'wqm_mcp_daemon_fallback_total',
  help: 'Number of times the daemon was unreachable and a fallback was triggered',
  labelNames: ['tool', 'reason'] as const,
  registers: [register],
});

/**
 * Cache hits by cache type.
 * Defined for future use — no in-process cache exists at v0.1.3.
 */
export const cacheHits = new Counter({
  name: 'wqm_mcp_cache_hits_total',
  help: 'Cache hits by cache name (defined for future use; no cache layer at v0.1.3)',
  labelNames: ['cache'] as const,
  registers: [register],
});

/**
 * Cache misses by cache type.
 * Defined for future use — no in-process cache exists at v0.1.3.
 */
export const cacheMisses = new Counter({
  name: 'wqm_mcp_cache_misses_total',
  help: 'Cache misses by cache name (defined for future use; no cache layer at v0.1.3)',
  labelNames: ['cache'] as const,
  registers: [register],
});

// ── Search instance-isolation (F-014) metrics ─────────────────────────────

/**
 * Genuine multi-clone project searches, labelled by whether base_point
 * instance narrowing degraded (`degraded="true"` when the active base-point
 * set exceeded the filter cap and per-file narrowing was bypassed).
 *
 * Only incremented for tenants with 2+ genuine CLONES (linked worktrees are
 * excluded — the branch filter already isolates them). This is the #3
 * observability signal that decides whether the server-side `watch_folders[]`
 * filter (the "proper half" of the F-014 fix) is worth building: if
 * `{degraded="true"}` stays flat at zero in practice, genuine multi-clone
 * degradation never actually happens and the larger change is unnecessary.
 *
 * Cardinality: 1 metric × 2 label values = 2 series.
 */
export const multicloneSearches = new Counter({
  name: 'wqm_mcp_multiclone_search_total',
  help: 'Genuine multi-clone (worktrees excluded) project searches, by base_point degradation',
  labelNames: ['degraded'] as const,
  registers: [register],
});
// Pre-initialise both series to 0 so the metric is present (at zero) from
// startup — a dashboard reads "0", not "no data", until the first genuine
// multi-clone search actually degrades.
multicloneSearches.labels({ degraded: 'true' }).inc(0);
multicloneSearches.labels({ degraded: 'false' }).inc(0);

// ── Query-language / translation metrics ───────────────────────────────────

/**
 * Language verdict for every ranked search, recorded WHETHER OR NOT translation
 * is enabled.
 *
 * This is the decision metric: it answers "what share of real traffic would the
 * translated leg touch?" with the feature switched off and costing nothing, so
 * the choice to enable it can be made on observed usage instead of a guess.
 * The gate is local and pure, so recording it adds no I/O.
 *
 * Cardinality: 1 metric × 2 label values = 2 series.
 */
export const queryLanguageVerdicts = new Counter({
  name: 'wqm_mcp_query_language_total',
  help: 'Ranked searches by detected query language class and actor',
  labelNames: ['verdict', 'actor'] as const,
  registers: [register],
});
// `actor` separates a human typing in the IDE from an agent's own searches.
// It matters for the decision: development traffic on this machine is heavy and
// not representative of real usage, so a share computed over everything can be
// dominated by the agent's own work. Bounded on purpose — anything unrecognised
// collapses to `other` rather than opening cardinality.
//
// `benchmark` exists because the eval harness deliberately reports itself as
// `telemetryActor: 'user'` (the search_events CHECK only permits
// claude/user/daemon, so a dedicated actor there would need a migration —
// see benchmarks/semantic-search.ts). Without a separate bucket HERE, one
// search_eval run injects 33 Portuguese and 38 English queries straight into
// the series this dashboard headlines as real human usage, manufacturing a
// ~46% non-English share out of the harness alone. Metrics carry no CHECK
// constraint, so the split costs nothing.
export const KNOWN_ACTORS = ['user', 'claude', 'benchmark', 'other'] as const;
for (const verdict of ['non_english', 'english'] as const) {
  for (const actor of KNOWN_ACTORS) {
    queryLanguageVerdicts.labels({ verdict, actor }).inc(0);
  }
}

/**
 * Collapse a free-form actor to a bounded label value.
 *
 * `isBenchmark` wins over `actor` because the eval harness reports itself as
 * `user`; trusting the actor alone is what would poison the decision series.
 */
export function actorLabel(actor: string | undefined, isBenchmark = false): string {
  if (isBenchmark) return 'benchmark';
  return actor === 'user' || actor === 'claude' ? actor : 'other';
}

/**
 * What actually happened to the second leg, by outcome:
 *
 *  - `disabled`          feature off (the default)
 *  - `no_translator`     enabled but endpoint/model unset
 *  - `already_english`   gate declined — the common path, costs nothing
 *  - `translated`        a usable translation came back
 *  - `translation_failed` the sidecar errored, timed out, or returned an
 *                        echo / still-Portuguese / runaway reply
 *
 * `translated` vs `translation_failed` is the health of the SLM itself: the 1.5B
 * sidecar failed 2 of 8 smoke queries this way, and that ratio is exactly what
 * decides whether a model swap is due.
 *
 * Cardinality: 1 metric × 5 label values = 5 series.
 */
export const queryTranslationOutcomes = new Counter({
  name: 'wqm_mcp_query_translation_total',
  help: 'Second-leg query translation attempts by outcome',
  labelNames: ['outcome'] as const,
  registers: [register],
});
for (const outcome of [
  'disabled',
  'no_translator',
  'already_english',
  'translated',
  'translation_failed',
] as const) {
  queryTranslationOutcomes.labels({ outcome }).inc(0);
}

/**
 * Hits in the returned page contributed by the translated leg, split by whether
 * both legs agreed on them.
 *
 * This is the EFFECTIVENESS metric, and the one that distinguishes "translation
 * ran" from "translation helped": a deployment where the leg fires constantly
 * but never places a hit is paying latency for nothing.
 *
 * Cardinality: 1 metric × 2 label values = 2 series.
 */
export const translatedLegHits = new Counter({
  name: 'wqm_mcp_translated_leg_hits_total',
  help: 'Returned hits contributed by the translated query leg',
  labelNames: ['agreement'] as const,
  registers: [register],
});
translatedLegHits.labels({ agreement: 'both_legs' }).inc(0);
translatedLegHits.labels({ agreement: 'translated_only' }).inc(0);

/**
 * Translations that succeeded but whose leg never ran.
 *
 * The second leg needs its own embeddings, and that call can degrade to the
 * fallback path. When it does, the translation genuinely succeeded — so
 * `queryTranslationOutcomes{outcome="translated"}` is correct and stays
 * one-per-search — but no second ranking was produced. Without this counter the
 * dashboard shows healthy translations against zero contributed hits and the
 * reading is "the leg is useless" when the truth is "the leg never ran".
 *
 * A separate counter rather than another `outcome` value on purpose: outcomes
 * must remain a partition of searches, and a search that translated AND skipped
 * would be counted twice there.
 */
export const translatedLegSkipped = new Counter({
  name: 'wqm_mcp_translated_leg_skipped_total',
  help: 'Successful translations whose second leg could not run (degraded embeddings)',
  registers: [register],
});
translatedLegSkipped.inc(0);

/** Record that a usable translation could not be turned into a second leg. */
export function recordTranslatedLegSkipped(): void {
  translatedLegSkipped.inc();
}

/**
 * Wall-clock of the translation round-trip. Buckets are tuned to the measured
 * range (1.5B median 123ms, 7B median 182ms) so a regression past ~0.5s — the
 * point where it visibly doubles a p50 ~90ms search — lands in its own bucket
 * instead of a catch-all.
 */
export const translationDuration = new Histogram({
  name: 'wqm_mcp_query_translation_duration_seconds',
  help: 'Latency of the query-translation round-trip',
  buckets: [0.05, 0.1, 0.15, 0.2, 0.3, 0.5, 1, 3],
  registers: [register],
});

/** Record the language gate's verdict for a ranked search. */
export function recordQueryLanguage(
  isLikelyNonEnglish: boolean,
  actor?: string,
  isBenchmark = false
): void {
  queryLanguageVerdicts
    .labels({
      verdict: isLikelyNonEnglish ? 'non_english' : 'english',
      actor: actorLabel(actor, isBenchmark),
    })
    .inc();
}

/** Record what became of the second leg, and how long any round-trip took. */
export function recordTranslationOutcome(outcome: string, durationSeconds?: number): void {
  queryTranslationOutcomes.labels({ outcome }).inc();
  if (durationSeconds !== undefined) translationDuration.observe(durationSeconds);
}

/** Record how many returned hits the translated leg put there. */
export function recordTranslatedLegHits(bothLegs: number, translatedOnly: number): void {
  if (bothLegs > 0) translatedLegHits.labels({ agreement: 'both_legs' }).inc(bothLegs);
  if (translatedOnly > 0) translatedLegHits.labels({ agreement: 'translated_only' }).inc(translatedOnly);
}

// ── HTTP transport metrics (MCP_SERVER_MODE=http) ──────────────────────────

/**
 * Total HTTP requests handled by the MCP listener, bucketed by logical path
 * (`/mcp`, `/healthz`, `other`) and status class (`2xx`, `4xx`, `5xx`). These
 * are only incremented in HTTP mode; stdio deployments leave them at zero.
 */
export const httpRequests = new Counter({
  name: 'wqm_mcp_http_requests_total',
  help: 'MCP HTTP transport requests by path and status class',
  labelNames: ['path', 'status_class'] as const,
  registers: [register],
});

/**
 * HTTP bearer-auth failures. Labelled by failure reason (`missing_header`,
 * `invalid_token`, `not_configured`) so dashboards can distinguish
 * mis-configured clients from probable attacks.
 */
export const httpAuthFailures = new Counter({
  name: 'wqm_mcp_http_auth_failures_total',
  help: 'MCP HTTP auth failures by failure reason',
  labelNames: ['reason'] as const,
  registers: [register],
});

/**
 * HTTP rate-limit hits. Single counter — alerting on sustained non-zero rate
 * catches runaway clients or brute-force attempts.
 */
export const httpRateLimited = new Counter({
  name: 'wqm_mcp_http_rate_limited_total',
  help: 'MCP HTTP requests rejected by the per-IP sliding-window rate limiter',
  registers: [register],
});

// ── Git hook integration metrics ────────────────────────────────────────────

/**
 * Total invocations of the git-hook → `workspace_index.sync_current_branch`
 * call path, broken down by:
 *  - `hook`: the originating hook name (`post-commit`, `post-checkout`,
 *    `post-merge`, `post-rewrite`, `post-worktree-add`, or `manual`)
 *  - `result`: `success` if the daemon `registerProject` call returned,
 *    `error` if it threw, `bad_request` if required args were missing.
 *  - `is_worktree`: `true`/`false`, indicating whether the sync targeted a
 *    linked worktree.
 *
 * Cardinality is bounded (~6 hooks × 3 results × 2 worktree = 36 series).
 */
export const gitHookInvocations = new Counter({
  name: 'wqm_git_hook_invocations_total',
  help: 'Git hooks dispatched to sync_current_branch by hook name, result and worktree flag',
  labelNames: ['hook', 'result', 'is_worktree'] as const,
  registers: [register],
});

/**
 * Server-side duration of a git-hook sync_current_branch call (mostly the
 * RegisterProject gRPC round-trip; excludes the initial MCP session init the
 * client does before this handler is reached).
 *
 * Buckets reach 30s because RegisterProject can stall behind LSP startup
 * (pyright initialize alone can take ~10s; other servers queue behind it).
 */
export const gitHookDuration = new Histogram({
  name: 'wqm_git_hook_duration_seconds',
  help: 'Server-side sync_current_branch handler duration in seconds',
  labelNames: ['hook', 'result'] as const,
  buckets: [0.05, 0.1, 0.25, 0.5, 1, 2.5, 5, 10, 30],
  registers: [register],
});

/**
 * Start a hook-duration timer. Returns a function that finalises the timer
 * and records the matching invocation counter. Always call the returned
 * function exactly once, with the resolved `result` label.
 */
export function startGitHookTimer(
  hook: string,
  isWorktree: boolean
): (result: 'success' | 'error' | 'bad_request') => void {
  const startNs = process.hrtime.bigint();
  return (result) => {
    const seconds = Number(process.hrtime.bigint() - startNs) / 1e9;
    gitHookDuration.labels({ hook, result }).observe(seconds);
    gitHookInvocations
      .labels({ hook, result, is_worktree: isWorktree ? 'true' : 'false' })
      .inc();
  };
}

// ── Tool wrapper ─────────────────────────────────────────────────────────────

/**
 * Wraps a tool handler function with Prometheus instrumentation.
 *
 * Records:
 *  - wqm_mcp_tool_invocations_total{tool, status="success"|"error"}
 *  - wqm_mcp_tool_duration_seconds{tool}
 *
 * The original return value is preserved on success; the original error is
 * re-thrown unchanged on failure so callers see unmodified error behaviour.
 */
export async function withToolMetrics<T>(toolName: string, fn: () => Promise<T>): Promise<T> {
  const end = toolDuration.startTimer({ tool: toolName });
  try {
    const result = await fn();
    end();
    toolInvocations.labels({ tool: toolName, status: 'success' }).inc();
    return result;
  } catch (err: unknown) {
    end();
    toolInvocations.labels({ tool: toolName, status: 'error' }).inc();
    throw err;
  }
}

// ── Session helpers ───────────────────────────────────────────────────────────

/** Call once when a new MCP session starts. */
export function recordSessionStart(): void {
  sessionCount.inc();
}

/** Call once when the current MCP session ends. */
export function recordSessionEnd(): void {
  sessionCount.dec();
}

// ── Daemon-fallback helper ────────────────────────────────────────────────────

/**
 * Increment the daemon-fallback counter.
 *
 * @param tool   Name of the tool that triggered the fallback (or 'session' for
 *               non-tool paths such as heartbeat).
 * @param reason Short reason string, e.g. 'connection_failed', 'timeout'.
 */
export function recordDaemonFallback(tool: string, reason: string): void {
  daemonFallback.labels({ tool, reason }).inc();
}

// ── Multi-clone search helper ─────────────────────────────────────────────────

/**
 * Record one genuine multi-clone project search (2+ clones sharing a tenant,
 * linked worktrees excluded). `degraded` is true when the active base-point
 * set exceeded the filter cap so instance narrowing was bypassed. See
 * {@link multicloneSearches}.
 */
export function recordMulticloneSearch(degraded: boolean): void {
  multicloneSearches.labels({ degraded: degraded ? 'true' : 'false' }).inc();
}

// ── HTTP helpers ────────────────────────────────────────────────────────────

/**
 * The configured MCP HTTP path, cached at module load from `MCP_HTTP_PATH`
 * (same env var used by `resolveHttpOptions()` in `src/index.ts`).
 * Defaults to `/mcp` when the variable is unset (matching DEFAULT_HTTP_PATH
 * in server-types.ts).
 *
 * Cached so every call to `httpPathLabel()` does not re-read process.env.
 * Tests that need a different value must call `vi.resetModules()` and
 * re-import this module after setting the env var.
 */
const _mcpHttpPath: string = process.env['MCP_HTTP_PATH'] ?? '/mcp';

/** Logical path label for HTTP counters — collapses ad-hoc URLs into buckets.
 *
 * Uses `MCP_HTTP_PATH` (cached at module load) to recognise the MCP route.
 * Any path that matches the configured MCP path (or starts with it followed
 * by `/`) is labelled `mcp`. `/healthz` is labelled `/healthz`. Everything
 * else falls back to `other`.
 */
export function httpPathLabel(rawPath: string | undefined): string {
  if (rawPath === undefined) return 'other';
  const noQuery = rawPath.split('?', 1)[0] ?? '';
  if (noQuery === '/healthz') return '/healthz';
  if (noQuery === _mcpHttpPath || noQuery.startsWith(_mcpHttpPath + '/')) return 'mcp';
  return 'other';
}

/** Status-class label for HTTP counters (`2xx`, `4xx`, `5xx`, or `other`). */
export function httpStatusClass(statusCode: number): string {
  if (statusCode >= 200 && statusCode < 300) return '2xx';
  if (statusCode >= 400 && statusCode < 500) return '4xx';
  if (statusCode >= 500 && statusCode < 600) return '5xx';
  return 'other';
}

/** Record a completed HTTP request. Safe to call from middleware or handler. */
export function recordHttpRequest(rawPath: string | undefined, statusCode: number): void {
  httpRequests
    .labels({ path: httpPathLabel(rawPath), status_class: httpStatusClass(statusCode) })
    .inc();
}

/** Record a bearer-auth failure. `reason` is free-form but low-cardinality. */
export function recordHttpAuthFailure(reason: string): void {
  httpAuthFailures.labels({ reason }).inc();
}

/** Record a rate-limit rejection. */
export function recordHttpRateLimited(): void {
  httpRateLimited.inc();
}

// ── Stdio-mode exit hook ──────────────────────────────────────────────────────

/**
 * Push accumulated metrics to the OTLP collector on process exit (stdio mode).
 *
 * In HTTP mode metrics are already scraped by Prometheus, so OTLP push is
 * redundant and intentionally skipped — callers are expected to guard on
 * MCP_SERVER_MODE before invoking this function (see src/index.ts).
 *
 * Delegates to pushMetricsToOTLP() which is fire-and-forget with a 1s timeout
 * and never throws, ensuring process exit is not blocked.
 */
export async function pushMetricsOnExit(): Promise<void> {
  await pushMetricsToOTLP();
}
