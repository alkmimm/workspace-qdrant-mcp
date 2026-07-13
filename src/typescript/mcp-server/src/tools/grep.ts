/**
 * Grep tool implementation for FTS5-based code search
 *
 * Provides exact substring and regex search across indexed code files with:
 * - Pattern matching (exact or regex)
 * - Path glob filtering (e.g., "**\/*.rs")
 * - Context lines before/after matches
 * - Tenant/branch scoping
 *
 * Uses daemon's TextSearchService via gRPC.
 */

import { randomUUID } from 'node:crypto';
import { matchesPathExclude } from '../utils/path-glob.js';
import { shapeGrepMatches, type GrepShapingOptions } from './grep-shaping.js';
import type { DaemonClient } from '../clients/daemon-client.js';
import type { ProjectDetector } from '../utils/project-detector.js';
import type { TextSearchMatch } from '../clients/grpc-types.js';
import type { SqliteStateManager } from '../clients/sqlite-state-manager.js';
import type { SearchDbReader } from '../clients/search-db-reader.js';
import { finishToolEvent, logSearchEvent } from '../clients/search-event-queries.js';
import { effectivenessTracker } from '../clients/effectiveness-signals.js';
import { int64ToNumber } from '../clients/daemon-client/system-methods.js';
import { SERVER_VERSION as MCP_SERVER_VERSION } from '../server-types.js';
import {
  concreteBranchFilter,
  resolveEffectiveBranch,
  resolveFallbackBranch,
  resolveProjectIdentity,
} from './branch-scope.js';
import { diagnoseEmptyResult, EMPTY_DIAGNOSIS_PROBE_LIMIT } from './empty-diagnosis.js';
import { whitespaceSensitivityHint } from './exact-hints.js';
import { lookupTestFlags } from './test-flag.js';

/**
 * Conservative proxy for the size of the files that contain a grep match.
 * Used to compute `bytes_in` for token-economy instrumentation when the
 * daemon has not yet been extended to report per-match file sizes.
 *
 * Spec `docs/specs/20-token-economy-instrumentation.md` §3.2 calls for
 * "sum of file sizes for each unique `file_path`, capped at FILE_PROBE_CAP".
 * The daemon now reports `file_size` per match (search.db v7+); when it
 * is present we use the real number. For rows ingested before v7 — or
 * for matches where stat() failed on the grep-searcher delegate path —
 * we fall back to this conservative per-unique-file proxy.
 */
export const GREP_BYTES_IN_PER_FILE_PROXY = 8192;

/**
 * Pure helper: compute `bytes_out` / `bytes_in` for a grep response.
 *
 * - `bytes_out` is the on-the-wire content cost the agent actually paid
 *   (sum of match content + context lines).
 * - `bytes_in` is what the agent would have paid to load each
 *   referenced file without the tool. When the daemon reports
 *   `file_size` for a match, that real number contributes to the sum;
 *   otherwise the per-unique-file proxy kicks in for that file. The
 *   result is always floored at `bytes_out` — we never claim savings
 *   for content we actually shipped.
 */
export function computeGrepEconomy(matches: GrepMatch[]): {
  bytesOut: number;
  bytesIn: number;
} {
  let bytesOut = 0;
  // Per-file size map: real bytes when known, proxy otherwise. We dedup
  // by `m.file` so the same file appearing in N matches contributes its
  // size exactly once.
  const perFile = new Map<string, number>();
  for (const m of matches) {
    bytesOut += m.content.length;
    for (const line of m.context_before) bytesOut += line.length;
    for (const line of m.context_after) bytesOut += line.length;
    if (!perFile.has(m.file)) {
      perFile.set(
        m.file,
        m.file_size !== undefined && m.file_size > 0 ? m.file_size : GREP_BYTES_IN_PER_FILE_PROXY
      );
    } else if (m.file_size !== undefined && m.file_size > 0) {
      // Later occurrence of the same file carries a real size — prefer
      // it over an earlier proxy fallback. (In practice every match
      // from the same file carries the same `file_size`, but the
      // promotion is cheap and makes the helper order-independent.)
      perFile.set(m.file, m.file_size);
    }
  }
  let perFileSum = 0;
  for (const size of perFile.values()) perFileSum += size;
  const bytesIn = Math.max(bytesOut, perFileSum);
  return { bytesOut, bytesIn };
}

// Response shaping (per-line cap + byte budget) lives in grep-shaping.ts —
// re-exported here so existing consumers/tests keep their import path.
export {
  shapeGrepMatches,
  DEFAULT_GREP_MAX_BYTES_PER_LINE,
  type GrepShapingOptions,
  type ShapedGrepMatches,
} from './grep-shaping.js';

export interface GrepOptions {
  pattern: string;
  regex?: boolean;
  caseSensitive?: boolean;
  pathGlob?: string;
  pathExclude?: string;
  scope?: 'project' | 'all';
  contextLines?: number;
  maxResults?: number;
  /** Pagination offset into the deduped match list (default 0). Stable:
   *  the daemon orders matches deterministically (file, then line), so the
   *  window `[offset, offset + maxResults)` never skips or duplicates across
   *  calls with the same pattern/filters. When more matches remain the
   *  response sets `next_offset` — pass it back here for the next page. */
  offset?: number;
  branch?: string;
  projectId?: string;
  /** Per-line cap (chars) on match content and each context line. Longer
   *  lines are truncated with a `…[+N chars]` marker. Defaults to
   *  {@link DEFAULT_GREP_MAX_BYTES_PER_LINE}; 0 disables. */
  maxBytesPerLine?: number;
  /** Global cap (chars) on the summed match bodies of the response; trailing
   *  matches beyond it are dropped (>=1 kept) and reported via
   *  `budget_truncated`. Defaults to the search tool's response budget
   *  ({@link DEFAULT_MAX_RESPONSE_BYTES}); 0 disables. */
  maxResponseBytes?: number;
}

export interface GrepMatch {
  file: string;
  line: number;
  content: string;
  context_before: string[];
  context_after: string[];
  /**
   * File size in bytes, when the daemon reported it (search.db v7+).
   * `undefined` falls back to the per-file proxy. Spec
   * docs/specs/20-token-economy-instrumentation.md §3.2.
   */
  file_size?: number;
  /**
   * Present (always `true`) when the daemon classified this match's file as a
   * TEST file (`tracked_files.is_test` — same classifier behind the search
   * tool's is_test flag). Best-effort, project scope only; absent means
   * "not a test, or unknown" — never false.
   */
  is_test?: boolean;
}

export interface GrepResponse {
  success: boolean;
  matches: GrepMatch[];
  total_matches: number;
  truncated: boolean;
  latency_ms: number;
  message?: string;
  /** Attached only when the response byte budget dropped trailing matches:
   *  `dropped` is how many were cut (the kept set always has >=1). Narrow
   *  with pathGlob, lower contextLines, or raise `maxResponseBytes` — or
   *  continue from `next_offset`. */
  budget_truncated?: { dropped: number };
  /** Present when matches remain beyond this page (window end, daemon cap,
   *  or budget drop): pass it back as `offset` — with the same pattern and
   *  filters — to fetch the next page starting at the first unreturned
   *  match. Absent when the page reached the end of the match list. */
  next_offset?: number;
}

/** Build the text search request object for the daemon. */
function buildGrepRequest(
  pattern: string,
  regex: boolean,
  caseSensitive: boolean,
  contextLines: number,
  maxResults: number,
  tenantId: string | undefined,
  branch: string | undefined,
  pathGlob: string | undefined
): {
  pattern: string;
  regex: boolean;
  case_sensitive: boolean;
  context_lines: number;
  max_results: number;
  tenant_id?: string;
  branch?: string;
  path_glob?: string;
} {
  const request: {
    pattern: string;
    regex: boolean;
    case_sensitive: boolean;
    context_lines: number;
    max_results: number;
    tenant_id?: string;
    branch?: string;
    path_glob?: string;
  } = {
    pattern,
    regex,
    case_sensitive: caseSensitive,
    context_lines: contextLines,
    max_results: maxResults,
  };
  if (tenantId) request.tenant_id = tenantId;
  // `branch: "*"` is the documented "any branch" opt-out. The daemon FTS
  // query builder would otherwise filter `fm.branch = '*'` literally and
  // match nothing; drop the filter so "*" searches every branch for the
  // tenant (mirrors search-exact.ts / search-filters.ts).
  const concreteBranch = concreteBranchFilter(branch);
  if (concreteBranch) request.branch = concreteBranch;
  if (pathGlob) request.path_glob = pathGlob;
  return request;
}

/**
 * Collapse duplicate matches that point at the same (file, line).
 *
 * search.db keys `file_metadata` by `file_id` (no unique constraint on
 * tenant/branch/file_path), so a re-embed — which clears `tracked_files` and
 * lets re-ingestion allocate NEW file_ids — orphans the previous generation's
 * `file_metadata`/`code_lines` rows instead of replacing them. The FTS query
 * then returns the same line once per surviving generation, inflating
 * `total_matches` (e.g. 4 for 2 real files). Dedupe by `file:line` so the agent
 * sees each real hit once. (Root-cause GC — the re-embed clearing search.db —
 * is tracked separately; this keeps the surfaced result honest meanwhile.)
 */
export function dedupGrepMatches(matches: GrepMatch[]): GrepMatch[] {
  const seen = new Set<string>();
  const out: GrepMatch[] = [];
  const keySep = '\0';
  for (const m of matches) {
    const key = `${m.file}${keySep}${m.line}${keySep}${m.content}`;
    if (seen.has(key)) continue;
    seen.add(key);
    out.push(m);
  }
  return out;
}

/**
 * Drop grep matches whose file path matches the `pathExclude` glob — the hard
 * per-call exclude, applied in TS on the mapped matches (the daemon FTS query
 * itself only knows the `pathGlob` include). Floats like the include filter, so
 * `old_project/**` silences that tree at the repo root and any nested depth.
 * No-op when `pathExclude` is unset.
 */
export function filterGrepMatchesByExclude(
  matches: GrepMatch[],
  pathExclude: string | undefined
): GrepMatch[] {
  if (!pathExclude) return matches;
  return matches.filter((m) => !matchesPathExclude(m.file, pathExclude));
}

/** Map daemon TextSearchMatch array to GrepMatch array. */
export function mapGrepMatches(matches: TextSearchMatch[]): GrepMatch[] {
  return matches.map((m: TextSearchMatch) => {
    const out: GrepMatch = {
      file: m.file_path,
      line: m.line_number,
      content: m.content,
      context_before: m.context_before ?? [],
      context_after: m.context_after ?? [],
    };
    // Carry file_size through when the daemon reported it (spec 20
    // §3.2 file-size probe). The proto field is int64, and the client
    // loads protos with `longs: String` — so at runtime this is a
    // STRING and MUST be coerced before any arithmetic: summing string
    // sizes concatenates digits, and the resulting "sum" overflows i64
    // on the write path where gRPC clamps it to i64::MAX, which in turn
    // blows up SUM(bytes_in) in the token_savings view. Skip 0 —
    // proto3 defaults non-optional int64 fields to 0, and an unset
    // optional decodes to undefined (coerced to the 0 fallback).
    const fileSize = int64ToNumber(m.file_size);
    if (fileSize > 0) out.file_size = fileSize;
    return out;
  });
}

function branchWideningMessage(branch: string): string {
  return (
    'No matches on branch "' +
    branch +
    '" — widened to all branches automatically; the results may be from another ' +
    'indexed branch (a file unchanged on your branch stays indexed under the branch ' +
    'it was last modified on). Pass branch:"*" to make this explicit, or an indexed ' +
    'branch name to scope.'
  );
}

function grepScopeOptInHint(pattern: string): string {
  return (
    `No matches for "${pattern}" in the current project. It may live in another indexed ` +
    'repository — pass scope:"all" to search across every repository (opt-in; this crosses ' +
    'project boundaries). For a concept rather than a literal, use the `search` tool (semantic); ' +
    'otherwise broaden the pattern or drop any pathGlob filter.'
  );
}

function grepEmptyRecoveryHint(pattern: string): string {
  return (
    `No matches for "${pattern}" in any indexed project. ` +
    'If you are looking for a concept rather than a literal string, use the `search` tool ' +
    '(semantic); otherwise broaden the pattern, drop any pathGlob filter, or check the spelling. ' +
    'Retrying the same pattern verbatim will return the same empty result.'
  );
}

/** Build an empty failure GrepResponse. */
function grepError(message: string, latency_ms: number): GrepResponse {
  return { success: false, matches: [], total_matches: 0, truncated: false, latency_ms, message };
}

/**
 * Grep tool for FTS5-based code search
 */
export class GrepTool {
  private readonly daemonClient: DaemonClient;
  private readonly projectDetector: ProjectDetector;
  private readonly stateManager: SqliteStateManager | undefined;
  private readonly searchDbReader: SearchDbReader | undefined;

  constructor(
    daemonClient: DaemonClient,
    projectDetector: ProjectDetector,
    stateManager?: SqliteStateManager,
    searchDbReader?: SearchDbReader
  ) {
    this.daemonClient = daemonClient;
    this.projectDetector = projectDetector;
    this.stateManager = stateManager;
    this.searchDbReader = searchDbReader;
  }

  /**
   * Search code using FTS5 trigram index
   */
  async grep(options: GrepOptions): Promise<GrepResponse> {
    const {
      pattern,
      regex = false,
      caseSensitive = true,
      pathGlob,
      pathExclude,
      scope = 'project',
      contextLines = 0,
      maxResults = 1000,
      offset = 0,
      branch,
      projectId,
      maxBytesPerLine,
      maxResponseBytes,
    } = options;
    const pageOffset = Number.isFinite(offset) && offset > 0 ? Math.floor(offset) : 0;
    const shaping: GrepShapingOptions = { maxBytesPerLine, maxResponseBytes };

    if (!pattern) return grepError('Search pattern is required', 0);

    const startTime = Date.now();
    const eventId = randomUUID();

    let tenantId: string | undefined;
    let projectPath: string | undefined;
    if (scope === 'project') {
      const identity = await resolveProjectIdentity(this.projectDetector, projectId);
      tenantId = identity.projectId;
      projectPath = identity.projectPath;
      if (!tenantId) {
        // Log a quick event so the rejection still shows up in
        // followup / escalation analyses even though we never reach
        // the daemon.
        this.logGrepStart(eventId, pattern, maxResults, undefined, undefined);
        finishToolEvent(this.daemonClient, eventId, {
          resultCount: 0,
          latencyMs: Date.now() - startTime,
          bytesIn: 0,
          bytesOut: 0,
          toolVersion: MCP_SERVER_VERSION,
          outcome: 'unresolved_tenant',
        });
        return grepError(
          'Could not detect the project. Pass `cwd` (to auto-detect the project) or `projectId`, or use scope "all".',
          Date.now() - startTime
        );
      }
    }

    const effectiveBranch = resolveEffectiveBranch({
      explicitBranch: branch,
      scope,
      projectId: tenantId,
      projectPath,
    });

    const concreteEffective = concreteBranchFilter(effectiveBranch);
    const watchFolderId =
      tenantId && this.stateManager ? this.stateManager.getWatchFolderIdByTenantId(tenantId) : null;
    const baseBranch =
      watchFolderId && concreteEffective
        ? this.stateManager?.getBaseBranch(watchFolderId, concreteEffective)
        : null;
    const fallbackBranch = resolveFallbackBranch({ effectiveBranch, baseBranch });

    this.logGrepStart(eventId, pattern, maxResults, tenantId, effectiveBranch);

    return this.executeSearch(
      pattern,
      regex,
      caseSensitive,
      contextLines,
      maxResults,
      pageOffset,
      tenantId,
      effectiveBranch,
      pathGlob,
      pathExclude,
      startTime,
      eventId,
      shaping,
      fallbackBranch
    );
  }

  /** Log the pre-execution event for a grep call. */
  private logGrepStart(
    eventId: string,
    pattern: string,
    maxResults: number,
    tenantId: string | undefined,
    branch: string | undefined
  ): void {
    // Cap the queryText so a pathological regex doesn't bloat the
    // search_events row.
    const truncatedPattern = pattern.length > 500 ? pattern.slice(0, 500) : pattern;
    const concreteBranch = concreteBranchFilter(branch);
    logSearchEvent(this.daemonClient, {
      id: eventId,
      actor: 'claude',
      tool: 'mcp_qdrant',
      op: 'grep',
      queryText: truncatedPattern,
      topK: maxResults,
      projectId: tenantId,
      filters:
        concreteBranch !== undefined ? JSON.stringify({ branch: concreteBranch }) : undefined,
    });
  }

  private async executeSearch(
    pattern: string,
    regex: boolean,
    caseSensitive: boolean,
    contextLines: number,
    maxResults: number,
    offset: number,
    tenantId: string | undefined,
    branch: string | undefined,
    pathGlob: string | undefined,
    pathExclude: string | undefined,
    startTime: number,
    eventId: string,
    shaping: GrepShapingOptions,
    fallbackBranch?: string
  ): Promise<GrepResponse> {
    try {
      // Paging is a client-side slice over the daemon's deterministic order
      // (file, then line — the FTS request has no offset field), so fetch
      // deep enough to cover the requested window.
      const fetchDepth = maxResults + offset;
      const request = buildGrepRequest(
        pattern,
        regex,
        caseSensitive,
        contextLines,
        fetchDepth,
        tenantId,
        branch,
        pathGlob
      );
      const responses = [await this.daemonClient.textSearch(request)];
      if (fallbackBranch) {
        responses.push(
          await this.daemonClient.textSearch(
            buildGrepRequest(
              pattern,
              regex,
              caseSensitive,
              contextLines,
              fetchDepth,
              tenantId,
              fallbackBranch,
              pathGlob
            )
          )
        );
      }
      const rawMatches = filterGrepMatchesByExclude(
        responses.flatMap((response) => mapGrepMatches(response.matches)),
        pathExclude
      );
      const dedupedMatches = dedupGrepMatches(rawMatches);
      let matches = dedupedMatches.slice(offset, offset + maxResults);
      let duplicatesDropped = rawMatches.length - dedupedMatches.length;
      let truncated =
        responses.some((response) => response.truncated) ||
        dedupedMatches.length > offset + matches.length;
      let totalMatches = responses.reduce((sum, response) => sum + response.total_matches, 0);
      let message: string | undefined;
      let widenedFired = false;
      // Auto-widen on empty: a branch-scoped grep that finds nothing may simply
      // be missing content the daemon tagged under another branch — the daemon
      // only tags CHANGED files under a feature branch, so a file UNCHANGED on
      // the current branch stays indexed under the branch it was last modified
      // on and is invisible to a strict branch filter (and the recorded
      // base-branch fallback can be absent for ad-hoc branches). Re-run across
      // ALL branches. Fires ONLY when the scoped result is empty, so it never
      // dilutes good branch-scoped results (no cross-branch leak in that case).
      // Gate on offset === 0: an empty PAGE (offset at/past the end of the
      // match list) is a pagination boundary, not a scoping problem — widening
      // there would restart the search cross-branch and return page-1 hits
      // under a stale offset.
      if (matches.length === 0 && offset === 0) {
        const widened = await this.widenGrepToAllBranches(
          pattern,
          regex,
          caseSensitive,
          contextLines,
          maxResults,
          tenantId,
          branch,
          pathGlob,
          pathExclude
        );
        if (widened) {
          widenedFired = true;
          matches = widened.matches;
          duplicatesDropped = widened.duplicatesDropped;
          truncated = widened.truncated;
          totalMatches = widened.totalMatches;
          message = widened.message;
        }
      }
      // NO automatic cross-project widen. A project-scoped grep (tenantId set)
      // that finds nothing must NOT silently return hits from OTHER repos — that
      // crosses the tenant data-isolation boundary without the caller opting in
      // (a confidential repo indexed in the same instance would leak into an
      // unrelated project's session). Offer scope:"all" as an explicit opt-in
      // instead of fetching cross-project data. When the search was already
      // cross-project (scope:"all" → no tenantId), it's a genuine total miss.
      if (matches.length === 0 && offset > 0 && message === undefined) {
        // Empty PAGE, not an empty result: say so instead of running the
        // empty-result diagnosis (which would wrongly report "pattern absent").
        message =
          `Offset ${offset} is at or beyond the end of the match list ` +
          `(${dedupedMatches.length} deduped match(es) total). Restart from offset 0 ` +
          `or lower the offset.`;
      }
      if (matches.length === 0 && message === undefined) {
        // Before the generic hints, try to explain the emptiness SPECIFICALLY:
        // a path filter that dropped everything, or a branch with no index.
        // A bare "No matches" otherwise conflates "pattern absent" with "the
        // filter/branch hid it" — the silent-zero trap. Only for project scope
        // (tenantId set); best-effort, never throws.
        const diagnosis = tenantId
          ? await this.diagnoseEmptyGrep(
              pattern,
              regex,
              caseSensitive,
              contextLines,
              tenantId,
              branch,
              pathGlob,
              pathExclude
            )
          : undefined;
        if (diagnosis) {
          message = diagnosis;
        } else {
          message = tenantId ? grepScopeOptInHint(pattern) : grepEmptyRecoveryHint(pattern);
          // A literal multi-token miss is often just a spacing / type-annotation
          // mismatch, not a true absence — nudge toward a whitespace-tolerant regex.
          if (!regex) {
            const ws = whitespaceSensitivityHint(pattern, true);
            if (ws) message += ` ${ws}`;
          }
        }
      }
      // Shaping (spec 20 §3.2): per-line cap + global response byte budget,
      // applied BEFORE the economy is computed so bytes_out reflects what
      // actually ships. Grep matches are line-scoped, but minified/generated
      // one-liners and high maxResults × contextLines sweeps still explode
      // without a bound — the same budget semantics as the search tool.
      const shaped = shapeGrepMatches(matches, shaping);
      // is_test parity with the semantic path (which reads the Qdrant ingest
      // tags): FTS rows carry no tags, so read the daemon's verdict back from
      // tracked_files by absolute path. Best-effort and project-scope only.
      if (tenantId && this.stateManager) {
        const testFlags = lookupTestFlags(
          this.stateManager,
          tenantId,
          shaped.matches.map((m) => m.file)
        );
        for (const m of shaped.matches) {
          if (testFlags.get(m.file) === true) m.is_test = true;
        }
      }
      const economy = computeGrepEconomy(shaped.matches);
      // Effectiveness signals (spec 20 §1.2): a retrieve() of one of these
      // files within the escalation window links back via parent_event_id.
      // Uses the SHAPED matches — refs the agent actually received (a
      // budget-dropped match never reached it, so it cannot escalate).
      effectivenessTracker.noteHits(
        eventId,
        shaped.matches.map((m) => m.file)
      );
      const latencyMs = Date.now() - startTime;
      finishToolEvent(this.daemonClient, eventId, {
        resultCount: shaped.matches.length,
        latencyMs,
        bytesIn: economy.bytesIn,
        bytesOut: economy.bytesOut,
        toolVersion: MCP_SERVER_VERSION,
        shapeMode: shaped.shapeMode,
        hitsTruncated: shaped.hitsTruncated,
      });
      // Budget drops get their own signal — do NOT fold them into `truncated`,
      // whose long-standing meaning is "the daemon capped the match set at
      // maxResults" with "raise maxResults / narrow the pattern" as the remedy.
      // A budget drop needs a DIFFERENT lever, so overloading the flag sends
      // pre-existing consumers into a raise-maxResults retry loop that returns
      // the identical page. The message teaches the right knob in-band.
      if (shaped.dropped > 0) {
        const budgetNote =
          `Response byte budget dropped ${shaped.dropped} trailing match(es) — ` +
          `continue from next_offset, narrow with pathGlob, lower contextLines, ` +
          `or raise maxResponseBytes.`;
        message = message ? `${message} ${budgetNote}` : budgetNote;
      }
      // More matches remain past this page when the pre-widen `truncated`
      // already said so (daemon cap or window end) or the byte budget cut the
      // tail. Suppressed after an auto-widen: paging re-runs the SCOPED query,
      // so a widened next_offset would dangle — page the widened set by
      // passing branch:"*" explicitly instead.
      const moreRemain = !widenedFired && (truncated || shaped.dropped > 0);
      return {
        success: true,
        matches: shaped.matches,
        // Report the deduped count. When the daemon truncated, its
        // total_matches is an upper bound over the (duplicated) full set —
        // discount the duplicates seen on this page as a best effort. An
        // untruncated paged read knows the exact full count (the deduped set);
        // a widened result only knows its own page.
        total_matches: truncated
          ? Math.max(offset + matches.length, totalMatches - duplicatesDropped)
          : widenedFired
            ? matches.length
            : dedupedMatches.length,
        truncated,
        latency_ms: latencyMs,
        ...(message ? { message } : {}),
        ...(shaped.dropped > 0 ? { budget_truncated: { dropped: shaped.dropped } } : {}),
        ...(shaped.matches.length > 0 && moreRemain
          ? { next_offset: offset + shaped.matches.length }
          : {}),
      };
    } catch (error) {
      const latencyMs = Date.now() - startTime;
      finishToolEvent(this.daemonClient, eventId, {
        resultCount: 0,
        latencyMs,
        bytesIn: 0,
        bytesOut: 0,
        toolVersion: MCP_SERVER_VERSION,
        outcome: 'error',
      });
      return grepError(
        `Grep failed: ${error instanceof Error ? error.message : 'unknown error'}`,
        latencyMs
      );
    }
  }

  /**
   * On an empty branch-scoped result, re-run the grep across ALL branches and
   * return the widened matches (+ a message noting the widening).
   *
   * Returns `undefined` when no widening applies (no tenant, or the branch was
   * already "*"/unset) or when the all-branches query is also empty — in which
   * case the caller keeps its (empty) branch-scoped result unchanged.
   */
  private async widenGrepToAllBranches(
    pattern: string,
    regex: boolean,
    caseSensitive: boolean,
    contextLines: number,
    maxResults: number,
    tenantId: string | undefined,
    branch: string | undefined,
    pathGlob: string | undefined,
    pathExclude: string | undefined
  ): Promise<
    | {
        matches: GrepMatch[];
        truncated: boolean;
        totalMatches: number;
        duplicatesDropped: number;
        message: string;
      }
    | undefined
  > {
    const concreteBranch = concreteBranchFilter(branch);
    if (!tenantId || !concreteBranch) return undefined;
    try {
      const resp = await this.daemonClient.textSearch(
        buildGrepRequest(
          pattern,
          regex,
          caseSensitive,
          contextLines,
          maxResults,
          tenantId,
          '*',
          pathGlob
        )
      );
      const rawMatches = filterGrepMatchesByExclude(mapGrepMatches(resp.matches), pathExclude);
      const dedupedMatches = dedupGrepMatches(rawMatches);
      const matches = dedupedMatches.slice(0, maxResults);
      if (matches.length === 0) return undefined;
      return {
        matches,
        duplicatesDropped: rawMatches.length - dedupedMatches.length,
        truncated: resp.truncated || dedupedMatches.length > matches.length,
        totalMatches: resp.total_matches,
        message: branchWideningMessage(concreteBranch),
      };
    } catch {
      return undefined;
    }
  }

  /**
   * Explain a still-empty project-scoped result before falling back to the
   * generic hints, via the shared {@link diagnoseEmptyResult} probes (path
   * filter excluded everything → branch has no indexed content). The
   * `countWithoutPathFilter` closure re-runs the same branch scope with NO path
   * filter (no `pathGlob` sent, no `pathExclude` post-filter).
   */
  private async diagnoseEmptyGrep(
    pattern: string,
    regex: boolean,
    caseSensitive: boolean,
    contextLines: number,
    tenantId: string,
    branch: string | undefined,
    pathGlob: string | undefined,
    pathExclude: string | undefined
  ): Promise<string | undefined> {
    return diagnoseEmptyResult({
      tenantId,
      branch,
      pathGlob,
      pathExclude,
      searchDbReader: this.searchDbReader,
      countWithoutPathFilter: async () => {
        const resp = await this.daemonClient.textSearch(
          buildGrepRequest(
            pattern,
            regex,
            caseSensitive,
            contextLines,
            EMPTY_DIAGNOSIS_PROBE_LIMIT,
            tenantId,
            branch,
            undefined // drop pathGlob; no pathExclude post-filter either
          )
        );
        return dedupGrepMatches(mapGrepMatches(resp.matches)).length;
      },
    });
  }

  /**
   * Resolve project ID from current working directory
   */
}
