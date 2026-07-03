/**
 * FTS5 exact/substring search via daemon's TextSearchService.
 */

import type { QdrantClient } from '@qdrant/js-client-rest';
import type { DaemonClient } from '../clients/daemon-client.js';
import type { SqliteStateManager } from '../clients/sqlite-state-manager.js';
import type { ProjectDetector } from '../utils/project-detector.js';
import type { SearchOptions, SearchResult, SearchResponse, FilterParams } from './search-types.js';
import { PROJECTS_COLLECTION } from './search-types.js';
import { attachIndexingProgress } from './search-helpers.js';
import { buildFilter } from './search-filters.js';
import { FIELD_CONTENT, FIELD_TITLE } from '../common/native-bridge.js';
import {
  applyEffectiveBranch,
  concreteBranchFilter,
  resolveEffectiveBranch,
  resolveFallbackBranch,
  resolveProjectIdentity,
} from './branch-scope.js';

/**
 * Resolution outcome for exact-search tenant scoping.
 *
 * Closes F-004: project-scope exact search MUST refuse to run when no
 * tenant can be resolved — without a tenant the daemon's FTS query
 * builder drops the `fm.tenant_id = ?` clause and broadens to every
 * tenant in the FTS index.
 */
type ExactSearchTenantResolution =
  | { kind: 'tenant'; tenantId: string; projectPath?: string | undefined }
  | { kind: 'unscoped' } // explicit `scope: 'all'` — caller asked for global FTS
  | { kind: 'unresolved' }; // project-scope but no tenant could be found

async function resolveExactSearchTenant(
  options: SearchOptions,
  projectDetector: ProjectDetector,
  stateManager: SqliteStateManager
): Promise<ExactSearchTenantResolution> {
  if (options.scope === 'all') return { kind: 'unscoped' };
  const identity = await resolveProjectIdentity(projectDetector, options.projectId);
  if (identity.projectId) {
    return {
      kind: 'tenant',
      tenantId: identity.projectId,
      projectPath:
        identity.projectPath ?? stateManager.getProjectById(identity.projectId).data?.project_path,
    };
  }
  return { kind: 'unresolved' };
}

/** Map daemon text search matches to SearchResult array. */
function mapExactResults(
  matches: Array<{
    file_path: string;
    line_number: number;
    content: string;
    tenant_id?: string;
    branch?: string;
    context_before?: string[];
    context_after?: string[];
  }>,
  requestedBranch?: string
): SearchResult[] {
  return matches.map((m, idx) => ({
    id: `${m.file_path}:${m.line_number}`,
    score: 1.0 - idx * 0.001,
    collection: PROJECTS_COLLECTION,
    content: m.content,
    metadata: {
      file_path: m.file_path,
      line_number: m.line_number,
      tenant_id: m.tenant_id,
      branch: requestedBranch ?? m.branch,
      _matched_branch: m.branch,
      context_before: m.context_before,
      context_after: m.context_after,
      _search_type: 'exact',
    },
  }));
}

function dedupeExactResults(results: SearchResult[]): SearchResult[] {
  const seen = new Set<string>();
  const out: SearchResult[] = [];
  const keySep = '\0';
  for (const result of results) {
    const file = String(result.metadata?.file_path ?? result.id);
    const line = String(result.metadata?.line_number ?? '');
    const key = `${file}${keySep}${line}${keySep}${result.content}`;
    if (seen.has(key)) continue;
    seen.add(key);
    out.push(result);
  }
  return out;
}

/** Build the text search request from search options. */
function buildExactSearchRequest(
  options: SearchOptions,
  tenantId: string | undefined
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
    pattern: options.query,
    regex: false,
    case_sensitive: true,
    context_lines: options.contextLines ?? 0,
    max_results: options.limit ?? 100,
  };
  if (tenantId) request.tenant_id = tenantId;
  // `branch: "*"` is the documented "any branch" opt-out (see
  // buildSearchOptions / search-filters.ts buildBranchCondition). The
  // daemon FTS query builder has no "*" concept — it would filter
  // `fm.branch = '*'` literally and match nothing — so drop the filter
  // entirely, matching the Qdrant path's behaviour.
  if (options.branch && options.branch !== '*') request.branch = options.branch;
  if (options.pathGlob) request.path_glob = options.pathGlob;
  return request;
}

/** Build the response returned when project-scope exact search has no
 * resolvable tenant. Closes F-004 (no broadening to all FTS tenants). */
function unresolvedTenantResponse(options: SearchOptions): SearchResponse {
  return {
    results: [],
    total: 0,
    query: options.query,
    mode: 'keyword',
    scope: options.scope ?? 'project',
    collections_searched: [],
    status: 'uncertain',
    status_reason:
      'Project scope requested but no project could be resolved. ' +
      'Pass `projectId` explicitly, run from a registered project directory, ' +
      'or set `scope: "all"` to search across every indexed tenant.',
  };
}

/**
 * Exact/substring search over a NON-`projects` collection (scratchpad,
 * libraries) via a tenant-scoped Qdrant scroll + substring match.
 *
 * The daemon's FTS5 index only covers project CODE lines, so the normal exact
 * path silently searched `projects` and returned nothing for a phrase that
 * lives in, e.g., the scratchpad (`collections_searched:["projects"]`). Honour
 * the requested `collection` instead: scroll it under the same tenant filter the
 * semantic path builds, then substring-match `content`/`title` case-sensitively
 * (same case semantics as the FTS path). scratchpad/libraries are NOT
 * branch-scoped, so no branch condition is applied.
 */
async function exactSearchInCollection(
  qdrantClient: QdrantClient,
  collection: string,
  options: SearchOptions,
  tenantId: string | undefined,
  eventId: string,
  stateManager: SqliteStateManager,
  startTime: number
): Promise<SearchResponse> {
  const scope = options.scope ?? 'project';
  const filterParams: FilterParams = {
    collection,
    scope,
    projectId: tenantId,
    branch: undefined, // scratchpad/libraries are not branch-scoped
    fallbackBranch: undefined,
    fileType: options.fileType,
    libraryName: options.libraryName,
    tag: options.tag,
    tags: options.tags,
    pathGlob: options.pathGlob,
    component: options.component,
    basePoints: undefined,
  };
  const filter = buildFilter(filterParams);
  const limit = options.limit ?? 100;
  const needle = options.query;
  const results: SearchResult[] = [];
  try {
    const scrolled = await qdrantClient.scroll(collection, {
      // Over-fetch: the substring filter runs locally over the scrolled page.
      limit: Math.max(limit * 4, 100),
      with_payload: true,
      ...(filter ? { filter } : {}),
    });
    for (const point of scrolled.points) {
      const content = (point.payload?.[FIELD_CONTENT] as string) ?? '';
      const title = (point.payload?.[FIELD_TITLE] as string) ?? '';
      if (!content.includes(needle) && !title.includes(needle)) continue;
      const result: SearchResult = {
        id: String(point.id),
        score: 1.0 - results.length * 0.001,
        collection,
        content,
        metadata: { ...point.payload, _search_type: 'exact' },
      };
      if (title) result.title = title;
      results.push(result);
      if (results.length >= limit) break;
    }
  } catch (error) {
    stateManager.updateSearchEvent(eventId, { resultCount: 0, latencyMs: Date.now() - startTime });
    return {
      results: [],
      total: 0,
      query: options.query,
      mode: 'keyword',
      scope,
      collections_searched: [],
      status: 'uncertain',
      status_reason: `Exact search failed: ${error instanceof Error ? error.message : 'unknown error'}`,
    };
  }
  stateManager.updateSearchEvent(eventId, {
    resultCount: results.length,
    latencyMs: Date.now() - startTime,
  });
  return {
    results,
    total: results.length,
    query: options.query,
    mode: 'keyword',
    scope,
    collections_searched: [collection],
  };
}

/**
 * Execute FTS5 exact/substring search via daemon's TextSearchService.
 * Maps TextSearchResponse to the standard SearchResponse format.
 */
export async function searchExact(
  qdrantClient: QdrantClient,
  daemonClient: DaemonClient,
  stateManager: SqliteStateManager,
  projectDetector: ProjectDetector,
  options: SearchOptions,
  eventId: string
): Promise<SearchResponse> {
  const startTime = Date.now();
  const resolution = await resolveExactSearchTenant(options, projectDetector, stateManager);

  if (resolution.kind === 'unresolved') {
    // F-004: refuse to broaden to every tenant in the FTS index. The
    // pre-fix code path omitted `tenant_id` from the daemon request and
    // the Rust query builder then dropped its `fm.tenant_id = ?`
    // clause, returning cross-tenant matches.
    stateManager.logSearchEvent({
      id: eventId,
      actor: options.telemetryActor ?? 'claude',
      tool: 'mcp_qdrant',
      op: 'search_exact',
      queryText: options.query,
    });
    stateManager.updateSearchEvent(eventId, {
      resultCount: 0,
      latencyMs: Date.now() - startTime,
    });
    return unresolvedTenantResponse(options);
  }

  const tenantId = resolution.kind === 'tenant' ? resolution.tenantId : undefined;
  const effectiveBranch = resolveEffectiveBranch({
    explicitBranch: options.branch,
    scope: options.scope ?? 'project',
    projectId: tenantId,
    projectPath: resolution.kind === 'tenant' ? resolution.projectPath : undefined,
  });
  const effectiveOptions = applyEffectiveBranch(options, effectiveBranch);
  stateManager.logSearchEvent({
    id: eventId,
    projectId: tenantId,
    actor: options.telemetryActor ?? 'claude',
    tool: 'mcp_qdrant',
    op: 'search_exact',
    queryText: effectiveOptions.query,
    filters:
      effectiveBranch && effectiveBranch !== '*'
        ? JSON.stringify({ branch: effectiveBranch })
        : undefined,
  });

  // The daemon FTS5 index only covers `projects` code lines. When the caller
  // explicitly targets another collection (scratchpad/libraries), honour it via
  // a scoped Qdrant scroll instead of silently searching `projects`.
  if (options.collection && options.collection !== PROJECTS_COLLECTION) {
    return exactSearchInCollection(
      qdrantClient,
      options.collection,
      effectiveOptions,
      tenantId,
      eventId,
      stateManager,
      startTime
    );
  }

  const concreteEffective = concreteBranchFilter(effectiveBranch);
  let baseBranch: string | null = null;
  if (tenantId && concreteEffective) {
    const watchFolderId = stateManager.getWatchFolderIdByTenantId(tenantId);
    if (watchFolderId) baseBranch = stateManager.getBaseBranch(watchFolderId, concreteEffective);
  }
  const fallbackBranch = resolveFallbackBranch({ effectiveBranch, baseBranch });

  return executeAndLogSearch(
    daemonClient,
    stateManager,
    effectiveOptions,
    tenantId,
    eventId,
    startTime,
    fallbackBranch
  );
}

async function executeAndLogSearch(
  daemonClient: DaemonClient,
  stateManager: SqliteStateManager,
  options: SearchOptions,
  tenantId: string | undefined,
  eventId: string,
  startTime: number,
  fallbackBranch?: string
): Promise<SearchResponse> {
  try {
    const request = buildExactSearchRequest(options, tenantId);
    const requestedBranch = concreteBranchFilter(options.branch);
    const primaryResponse = await daemonClient.textSearch(request);
    const responses = [primaryResponse];
    const resultGroups: SearchResult[][] = [mapExactResults(primaryResponse.matches, requestedBranch)];
    if (fallbackBranch) {
      const fallbackResponse = await daemonClient.textSearch(
        buildExactSearchRequest({ ...options, branch: fallbackBranch }, tenantId)
      );
      responses.push(fallbackResponse);
      resultGroups.push(mapExactResults(fallbackResponse.matches, requestedBranch));
    }
    const rawResults = resultGroups.flat();
    let dedupedResults = dedupeExactResults(rawResults);
    // Track the RAW set that produced `dedupedResults` — recomputed below if the
    // auto-widen replaces it. (A stale value goes negative: rawResults is empty
    // whenever dedupedResults is empty, so `0 - widened.length` would inflate
    // `total` for a truncated widen.)
    let duplicatesDropped = rawResults.length - dedupedResults.length;
    // Auto-widen on empty (parity with grep): a branch-scoped exact search that
    // finds nothing may be missing content the daemon tagged under another branch
    // (a file unchanged on the current branch stays indexed under the branch it
    // was last modified on). Re-run across ALL branches. Fires ONLY when the
    // scoped result is empty, so good branch-scoped results are never diluted.
    let branchWidened = false;
    if (dedupedResults.length === 0 && requestedBranch) {
      const widenedResponse = await daemonClient.textSearch(
        buildExactSearchRequest({ ...options, branch: '*' }, tenantId)
      );
      const widenedRaw = mapExactResults(widenedResponse.matches, undefined);
      const widened = dedupeExactResults(widenedRaw);
      if (widened.length > 0) {
        responses.push(widenedResponse);
        dedupedResults = widened;
        duplicatesDropped = widenedRaw.length - widened.length;
        branchWidened = true;
      }
    }
    // Scope-widen on empty (parity with grep): still empty AND this was a
    // project-scoped exact search (tenantId set) → the literal often lives in
    // another repository (measured: ~90% of empty exact hits were cross-project).
    // Re-run across ALL projects. Fires only on a genuinely empty project result.
    let scopeWidened = false;
    if (dedupedResults.length === 0 && tenantId) {
      const widenedResponse = await daemonClient.textSearch(
        buildExactSearchRequest({ ...options, branch: '*' }, undefined) // no tenant → all projects
      );
      const widenedRaw = mapExactResults(widenedResponse.matches, undefined);
      const widened = dedupeExactResults(widenedRaw);
      if (widened.length > 0) {
        responses.push(widenedResponse);
        dedupedResults = widened;
        duplicatesDropped = widenedRaw.length - widened.length;
        scopeWidened = true;
      }
    }
    const limit = options.limit ?? 100;
    // Pagination parity with the vector path (P1.5 C): honor `offset` by slicing
    // the deduped result list; set next_offset below when more remain.
    const offset = Math.max(0, options.offset ?? 0);
    const results = dedupedResults.slice(offset, offset + limit);
    const totalMatches = responses.reduce((sum, response) => sum + response.total_matches, 0);
    const total = responses.some((response) => response.truncated)
      ? Math.max(results.length, totalMatches - duplicatesDropped)
      : dedupedResults.length;

    stateManager.updateSearchEvent(eventId, {
      resultCount: results.length,
      latencyMs: Date.now() - startTime,
    });
    const successResponse: SearchResponse = {
      results,
      total,
      query: options.query,
      mode: 'keyword',
      scope: options.scope ?? 'project',
      collections_searched: [PROJECTS_COLLECTION],
    };
    if (dedupedResults.length > offset + limit) successResponse.next_offset = offset + results.length;
    if (scopeWidened) {
      successResponse.hint = `No exact matches in the current project — widened to ALL projects (${dedupedResults.length} found). Pass scope:"all" to search across repositories directly next time.`;
    } else if (branchWidened && requestedBranch) {
      successResponse.hint = `No exact matches on branch "${requestedBranch}" — widened to all branches; results may be from another indexed branch.`;
    } else if (dedupedResults.length === 0) {
      successResponse.hint = `No exact matches for "${options.query}" in any indexed project or branch. If you are looking for a concept rather than a literal string, use the search tool (semantic); otherwise broaden it or drop filters. Retrying the same query verbatim returns the same empty result.`;
    }
    await attachIndexingProgress(successResponse, daemonClient, successResponse.scope, tenantId);
    return successResponse;
  } catch (error) {
    stateManager.updateSearchEvent(eventId, { resultCount: 0, latencyMs: Date.now() - startTime });
    // Don't attach indexing on the error path: the daemon just failed
    // a different RPC, so the cached probe is unlikely to be fresh.
    return {
      results: [],
      total: 0,
      query: options.query,
      mode: 'keyword',
      scope: options.scope ?? 'project',
      collections_searched: [],
      status: 'uncertain',
      status_reason: `Exact search failed: ${error instanceof Error ? error.message : 'unknown error'}`,
    };
  }
}
