/**
 * Search event instrumentation via daemon gRPC.
 *
 * Logs and updates search events through the TrackingWriteService.
 * Errors are swallowed so instrumentation never blocks search execution.
 */

import type { DaemonClient } from './daemon-client.js';
import { currentSessionId, effectivenessTracker } from './effectiveness-signals.js';

/** Ops that carry a user query and participate in effectiveness tracking.
 *  All three get followup lineage via parent_event_id; the stored `op` is
 *  NEVER rewritten — op is event identity, and every op-keyed consumer
 *  (adoption sampler, Grafana series, `wqm admin search-latency` chains)
 *  depends on the census staying complete. The `token_savings` view derives
 *  "followup" from the parent link + op instead. */
const QUERY_TRACKING_OPS = new Set(['search', 'search_exact', 'grep']);

/**
 * In-flight INSERTs, keyed by event id.
 *
 * Every update below addresses its row by `event_id`, so it is only correct
 * AFTER the insert has landed. That ordering used to be implicit — guaranteed
 * only by however long the instrumented operation happened to take. A tool
 * that resolves in the SAME TICK breaks the assumption: both gRPC calls are
 * fire-and-forget, so the UPDATE reaches the daemon before the row exists,
 * matches nothing, and is silently dropped. Measured on the static `help`
 * lookup (0ms): 0 of 2 rows carried latency_ms/result_count, against ~100%
 * for every other op.
 *
 * Chaining here — at the single event-write boundary, where this file already
 * applies its other cross-cutting concerns — fixes ordering for EVERY caller
 * (the dispatcher's op-event lane and the self-instrumenting read tools)
 * instead of special-casing one tool, and costs no latency: the response path
 * still awaits none of this. Entries live only for the duration of their
 * insert, so the map cannot grow unbounded.
 */
const inFlightInserts = new Map<string, Promise<void>>();

/** Run `send` once this event's insert has settled (immediately if none is
 *  in flight — the insert already completed, or this process never made it). */
function afterInsert(eventId: string, send: () => void): void {
  const pending = inFlightInserts.get(eventId);
  if (!pending) {
    send();
    return;
  }
  // Settle either way: a failed insert must not strand the update forever.
  void pending.then(send, send);
}

export interface SearchEventInput {
  id: string;
  sessionId?: string | undefined;
  projectId?: string | undefined;
  actor: string;
  tool: string;
  op: string;
  queryText?: string | undefined;
  filters?: string | undefined;
  topK?: number | undefined;
  resultCount?: number | undefined;
  latencyMs?: number | undefined;
  topResultRefs?: string | undefined;
  outcome?: string | undefined;
  parentEventId?: string | undefined;
}

export interface SearchEventUpdate {
  resultCount: number;
  latencyMs: number;
  topResultRefs?: string | undefined;
  outcome?: string | undefined;
}

/**
 * Token-economy metrics from the post-execution shaping pass.
 * Spec: docs/specs/20-token-economy-instrumentation.md
 */
export interface SearchEventEconomyInput {
  bytesIn: number;
  bytesOut: number;
  hitsTruncated: number;
  shapeMode: 'truncate' | 'summary' | 'none' | 'packed';
  toolVersion?: string | undefined;
}

/**
 * Log a search event via daemon gRPC.
 *
 * Called at the start of a search to create the initial record.
 * Fire-and-forget: errors are swallowed so instrumentation never breaks search.
 */
export function logSearchEvent(daemonClient: DaemonClient | null, event: SearchEventInput): void {
  if (!daemonClient) return;

  // Effectiveness signals (spec 20 §1.2), applied at the single event-write
  // boundary so every tool gets them uniformly instead of per-call-site:
  //  - session_id: no caller ever had one to pass (the column was NULL on
  //    100% of live rows, so the view's per-session probes could never
  //    fire); default to the transport-scoped MCP session, falling back to
  //    a per-process id.
  //  - followup lineage: a search/search_exact/grep whose terms overlap a
  //    same-session query issued < 60s earlier gets parent_event_id = the
  //    origin. The stored `op` is preserved — the view derives "followup"
  //    from the link, so op-keyed analytics keep their full census.
  //  - agent traffic only: benchmark/eval runs (telemetryActor 'benchmark' /
  //    'user') fire dozens of related queries back-to-back in one session;
  //    classifying those would flood followup_rate with self-inflicted
  //    signal, so only actor 'claude' participates in tracking.
  // An explicit caller-provided sessionId/parentEventId always wins.
  const sessionId = event.sessionId ?? currentSessionId();
  let parentEventId = event.parentEventId;
  if (
    event.actor === 'claude' &&
    QUERY_TRACKING_OPS.has(event.op) &&
    event.queryText !== undefined &&
    event.queryText !== ''
  ) {
    const origin = effectivenessTracker.noteQuery(event.id, sessionId, event.queryText, Date.now());
    if (origin !== undefined && parentEventId === undefined) {
      parentEventId = origin;
    }
  }

  // Fire-and-forget — catch errors to avoid breaking search
  const request: {
    id: string;
    actor: string;
    tool: string;
    op: string;
    session_id?: string;
    project_id?: string;
    query_text?: string;
    filters?: string;
    top_k?: number;
    result_count?: number;
    latency_ms?: number;
    top_result_refs?: string;
    outcome?: string;
    parent_event_id?: string;
  } = {
    id: event.id,
    actor: event.actor,
    tool: event.tool,
    op: event.op,
  };
  request.session_id = sessionId;
  if (event.projectId !== undefined) request.project_id = event.projectId;
  if (event.queryText !== undefined) request.query_text = event.queryText;
  if (event.filters !== undefined) request.filters = event.filters;
  if (event.topK !== undefined) request.top_k = event.topK;
  if (event.resultCount !== undefined) request.result_count = event.resultCount;
  if (event.latencyMs !== undefined) request.latency_ms = event.latencyMs;
  if (event.topResultRefs !== undefined) request.top_result_refs = event.topResultRefs;
  if (event.outcome !== undefined) request.outcome = event.outcome;
  if (parentEventId !== undefined) request.parent_event_id = parentEventId;

  // Tracked in `inFlightInserts` so any update for this id waits for the row
  // to exist (see the map's doc comment). Still fire-and-forget to the caller.
  const insert = daemonClient.logSearchEvent(request).catch((err: unknown) => {
    // Instrumentation must never break search, but log for diagnostics
    console.warn(
      'logSearchEvent instrumentation failed:',
      err instanceof Error ? err.message : err
    );
  });
  inFlightInserts.set(event.id, insert);
  void insert.finally(() => {
    inFlightInserts.delete(event.id);
  });
}

/**
 * Update a search event with post-search results via daemon gRPC.
 *
 * Updates result_count, latency_ms, top_result_refs, and outcome
 * for a previously created search event.
 * Fire-and-forget: errors are swallowed.
 */
export function updateSearchEvent(
  daemonClient: DaemonClient | null,
  eventId: string,
  update: SearchEventUpdate
): void {
  if (!daemonClient) return;

  const request: {
    event_id: string;
    result_count: number;
    latency_ms: number;
    top_result_refs?: string;
    outcome?: string;
  } = {
    event_id: eventId,
    result_count: update.resultCount,
    latency_ms: update.latencyMs,
  };
  if (update.topResultRefs !== undefined) request.top_result_refs = update.topResultRefs;
  if (update.outcome !== undefined) request.outcome = update.outcome;

  afterInsert(eventId, () => {
    daemonClient.updateSearchEvent(request).catch((err: unknown) => {
      // Instrumentation must never break search, but log for diagnostics
      console.warn(
        'updateSearchEvent instrumentation failed:',
        err instanceof Error ? err.message : err
      );
    });
  });
}

/**
 * Record token-economy metrics for a previously logged search event.
 * Fire-and-forget: errors are swallowed so instrumentation never blocks
 * the search response.
 */
export function updateSearchEventEconomy(
  daemonClient: DaemonClient | null,
  eventId: string,
  update: SearchEventEconomyInput
): void {
  if (!daemonClient) return;

  const request: {
    event_id: string;
    bytes_in: number;
    bytes_out: number;
    hits_truncated: number;
    shape_mode: 'truncate' | 'summary' | 'none' | 'packed';
    tool_version?: string;
  } = {
    event_id: eventId,
    bytes_in: update.bytesIn,
    bytes_out: update.bytesOut,
    hits_truncated: update.hitsTruncated,
    shape_mode: update.shapeMode,
  };
  if (update.toolVersion !== undefined) request.tool_version = update.toolVersion;

  afterInsert(eventId, () => {
    daemonClient.updateSearchEventEconomy(request).catch((err: unknown) => {
      console.warn(
        'updateSearchEventEconomy instrumentation failed:',
        err instanceof Error ? err.message : err
      );
    });
  });
}

/**
 * Combined finish-instrumentation for tools that don't have a shaping
 * pass of their own (grep / retrieve / list). Records both the
 * post-execution `result_count` / `latency_ms` and the token-economy
 * sidecar in a single call site so the tool's own code stays minimal.
 *
 * Spec: `docs/specs/20-token-economy-instrumentation.md` §3.2–§3.4.
 *
 * Fire-and-forget for both sides — never raises to the caller.
 */
export interface ToolEventFinish {
  resultCount: number;
  latencyMs: number;
  bytesIn: number;
  bytesOut: number;
  toolVersion?: string | undefined;
  outcome?: string | undefined;
  /**
   * Optional shaping mode. Defaults to `'none'` for tools without a
   * shaping pass; grep passes `'truncate'` now that it caps lines and
   * enforces the response byte budget.
   */
  shapeMode?: 'truncate' | 'summary' | 'none' | 'packed' | undefined;
  /** Optional truncated-hit count. Defaults to 0 when shapeMode is 'none'. */
  hitsTruncated?: number | undefined;
}

export function finishToolEvent(
  daemonClient: DaemonClient | null,
  eventId: string,
  finish: ToolEventFinish
): void {
  if (!daemonClient) return;
  const updateArgs: SearchEventUpdate = {
    resultCount: finish.resultCount,
    latencyMs: finish.latencyMs,
  };
  if (finish.outcome !== undefined) updateArgs.outcome = finish.outcome;
  updateSearchEvent(daemonClient, eventId, updateArgs);

  const economyArgs: SearchEventEconomyInput = {
    bytesIn: finish.bytesIn,
    bytesOut: finish.bytesOut,
    hitsTruncated: finish.hitsTruncated ?? 0,
    shapeMode: finish.shapeMode ?? 'none',
  };
  if (finish.toolVersion !== undefined) economyArgs.toolVersion = finish.toolVersion;
  updateSearchEventEconomy(daemonClient, eventId, economyArgs);
}
