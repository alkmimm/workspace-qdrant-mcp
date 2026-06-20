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
import type { DaemonClient } from '../clients/daemon-client.js';
import type { ProjectDetector } from '../utils/project-detector.js';
import type { TextSearchMatch } from '../clients/grpc-types.js';
import type { SqliteStateManager } from '../clients/sqlite-state-manager.js';
import { finishToolEvent, logSearchEvent } from '../clients/search-event-queries.js';
import { SERVER_VERSION as MCP_SERVER_VERSION } from '../server-types.js';
import {
  concreteBranchFilter,
  resolveEffectiveBranch,
  resolveFallbackBranch,
  resolveProjectIdentity,
} from './branch-scope.js';

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

export interface GrepOptions {
  pattern: string;
  regex?: boolean;
  caseSensitive?: boolean;
  pathGlob?: string;
  scope?: 'project' | 'all';
  contextLines?: number;
  maxResults?: number;
  branch?: string;
  projectId?: string;
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
}

export interface GrepResponse {
  success: boolean;
  matches: GrepMatch[];
  total_matches: number;
  truncated: boolean;
  latency_ms: number;
  message?: string;
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

/** Map daemon TextSearchMatch array to GrepMatch array. */
function mapGrepMatches(matches: TextSearchMatch[]): GrepMatch[] {
  return matches.map((m: TextSearchMatch) => {
    const out: GrepMatch = {
      file: m.file_path,
      line: m.line_number,
      content: m.content,
      context_before: m.context_before ?? [],
      context_after: m.context_after ?? [],
    };
    // Carry file_size through when the daemon reported it (spec 20
    // §3.2 file-size probe). Skip 0 — proto3 defaults non-optional
    // int64 fields to 0, and an unset optional decodes to undefined
    // (which we want); keep the conditional defensive.
    if (m.file_size !== undefined && m.file_size > 0) out.file_size = m.file_size;
    return out;
  });
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

  constructor(
    daemonClient: DaemonClient,
    projectDetector: ProjectDetector,
    stateManager?: SqliteStateManager
  ) {
    this.daemonClient = daemonClient;
    this.projectDetector = projectDetector;
    this.stateManager = stateManager;
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
      scope = 'project',
      contextLines = 0,
      maxResults = 1000,
      branch,
      projectId,
    } = options;

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
      tenantId,
      effectiveBranch,
      pathGlob,
      startTime,
      eventId,
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
    tenantId: string | undefined,
    branch: string | undefined,
    pathGlob: string | undefined,
    startTime: number,
    eventId: string,
    fallbackBranch?: string
  ): Promise<GrepResponse> {
    try {
      const request = buildGrepRequest(
        pattern,
        regex,
        caseSensitive,
        contextLines,
        maxResults,
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
              maxResults,
              tenantId,
              fallbackBranch,
              pathGlob
            )
          )
        );
      }
      const rawMatches = responses.flatMap((response) => mapGrepMatches(response.matches));
      const dedupedMatches = dedupGrepMatches(rawMatches);
      const matches = dedupedMatches.slice(0, maxResults);
      const duplicatesDropped = rawMatches.length - dedupedMatches.length;
      const truncated =
        responses.some((response) => response.truncated) || dedupedMatches.length > matches.length;
      const totalMatches = responses.reduce((sum, response) => sum + response.total_matches, 0);
      const economy = computeGrepEconomy(matches);
      const latencyMs = Date.now() - startTime;
      finishToolEvent(this.daemonClient, eventId, {
        resultCount: matches.length,
        latencyMs,
        bytesIn: economy.bytesIn,
        bytesOut: economy.bytesOut,
        toolVersion: MCP_SERVER_VERSION,
      });
      return {
        success: true,
        matches,
        // Report the deduped count. When the daemon truncated, its
        // total_matches is an upper bound over the (duplicated) full set —
        // discount the duplicates seen on this page as a best effort.
        total_matches: truncated
          ? Math.max(matches.length, totalMatches - duplicatesDropped)
          : matches.length,
        truncated,
        latency_ms: latencyMs,
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
   * Resolve project ID from current working directory
   */
}
