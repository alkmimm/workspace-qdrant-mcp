/**
 * Retrieve tool — direct document access from Qdrant collections.
 *
 * - retrieve-types.ts: Types, constants, helpers
 * - retrieve.ts (this): RetrieveTool class with byId and byFilter operations
 *
 * Tenant isolation invariants (F-002 / F-011):
 *
 * - `projects` collection accesses MUST resolve to a `tenant_id` before any
 *   Qdrant call. `retrieveById` verifies the returned point's `tenant_id`
 *   matches the caller; mismatches return an empty not-found response.
 * - `libraries` collection accesses MUST resolve to a `library_name` (or
 *   `tenant_id`, since the library collection keys both fields). The
 *   project detector is never used as a fall-back for libraries.
 * - `rules` is intentionally mixed-tenancy; no scope verification is
 *   performed for `retrieveById` against rules.
 * - Project-scope retrieve without a resolvable tenant returns an empty
 *   error response and does NOT scroll Qdrant (no broad reads).
 * - Unknown argument names are refused loudly (`invalid_args`) rather than
 *   silently dropped — a mis-shaped call (e.g. `query`, a search parameter)
 *   must not degrade into a confusing unresolved-scope error.
 */

import { randomUUID } from 'node:crypto';
import type { QdrantClient } from '@qdrant/js-client-rest';
import { getQdrantClient } from '../clients/qdrant-client-factory.js';
import type { DaemonClient } from '../clients/daemon-client.js';
import type { SqliteStateManager } from '../clients/sqlite-state-manager.js';
import type { ProjectDetector } from '../utils/project-detector.js';
import { FIELD_CONTENT, FIELD_TENANT_ID, FIELD_LIBRARY_NAME } from '../common/native-bridge.js';
import { finishToolEvent, logSearchEvent } from '../clients/search-event-queries.js';
import { currentSessionId, effectivenessTracker } from '../clients/effectiveness-signals.js';
import { SERVER_VERSION as MCP_SERVER_VERSION } from '../server-types.js';
import {
  resolveProjectIdentity,
  resolveEffectiveBranch,
  resolveFallbackBranch,
  concreteBranchFilter,
} from './branch-scope.js';
import { branchFilterClause } from './search-filters.js';

/**
 * Branch scope for projects scroll paths. `branch` is the caller's effective
 * branch (their current Git branch, or an explicit value / `"*"`);
 * `fallbackBranch` is the base branch the daemon tags unchanged files under.
 * Both feed {@link branchFilterClause}, the same clause `search` uses.
 * Scratchpad reads stay branch-agnostic (notes are pinned to "main").
 */
interface BranchScope {
  branch?: string;
  fallbackBranch?: string;
}

// Re-export all types so existing imports from './retrieve.js' continue to work
export type {
  RetrieveCollectionType,
  RetrieveOptions,
  RetrievedDocument,
  RetrieveResponse,
  RetrieveToolConfig,
} from './retrieve-types.js';

import type {
  RetrieveCollectionType,
  RetrieveOptions,
  RetrievedDocument,
  RetrieveResponse,
  RetrieveToolConfig,
} from './retrieve-types.js';
import { getCollectionName, extractMetadata } from './retrieve-types.js';
import { RETRIEVE_ARG_KEYS } from '../tool-definitions/retrieve.js';

/**
 * Pure helper: compute `bytes_out` / `bytes_in` for a retrieve result.
 * Spec `docs/specs/20-token-economy-instrumentation.md` §3.3: for the
 * current implementation (full-document retrieve only — no ranged
 * retrieve) `bytes_in == bytes_out`, so `savings_ratio` is 0%. The row
 * still matters: when `parent_event_id` is populated, the v38
 * `token_savings` view uses it to detect escalation (search → retrieve
 * of the same document).
 */
export function computeRetrieveEconomy(documents: RetrievedDocument[]): {
  bytesOut: number;
  bytesIn: number;
} {
  let bytesOut = 0;
  for (const d of documents) bytesOut += d.content.length;
  // No ranged retrieve yet — full doc cost === served cost.
  return { bytesOut, bytesIn: bytesOut };
}

const RETRIEVE_ID_FILTER_HINT =
  'If you only have `metadata.document_id`, use `filter: { document_id: "<value>" }` instead.';
const RETRIEVE_LOCATION_HINT =
  'For exact-search hits, pass `filePath` + `lineNumber` from the result metadata.';

function looksLikeContentHash(documentId: string): boolean {
  return /^[a-f0-9]{64}$/i.test(documentId);
}

function buildUnknownArgsHint(unknownArgs: string[]): string {
  if (unknownArgs.includes('query')) {
    return '`retrieve` does not search by content. Use `search` for discovery, then pass the hit `id` field to `retrieve`.';
  }

  if (
    unknownArgs.some((arg) => {
      const normalized = arg.toLowerCase();
      return normalized === 'documentid' || normalized === 'document_id' || normalized === 'metadata';
    })
  ) {
    return `Use \`documentId\` for point IDs, \`filePath\` + \`lineNumber\` for exact-search locators, and \`filter\` for metadata lookups. ${RETRIEVE_ID_FILTER_HINT}`;
  }

  return `Use only the documented retrieve parameters. For point IDs, pass \`documentId\`; for exact-search hits, use \`filePath\` + \`lineNumber\`; for metadata lookups, use \`filter\`. ${RETRIEVE_ID_FILTER_HINT}`;
}

function buildNotFoundHint(documentId: string): string {
  const base = 'If this came from `search` or `list`, pass the result `id` field to `retrieve`.';
  const lineScopedId = parseLineScopedDocumentId(documentId);
  if (lineScopedId) {
    return `${base} The requested value looks like a line-scoped exact-search result, so pass \`filePath\` + \`lineNumber\` instead. ${RETRIEVE_LOCATION_HINT}`;
  }
  if (looksLikeContentHash(documentId)) {
    return `${base} The requested value looks like a metadata \`document_id\` hash, so use \`filter: { document_id: "${documentId}" }\` instead.`;
  }
  return `${base} ${RETRIEVE_ID_FILTER_HINT}`;
}

function buildLocationNotFoundHint(filePath: string, lineNumber?: number): string {
  const base = lineNumber !== undefined
    ? `If this came from an exact-search result, pass \`filePath\` + \`lineNumber\` from the metadata.`
    : 'If this came from a search/list result, pass the result `id` field to `retrieve`.';
  const fallback =
    lineNumber !== undefined
      ? `The locator already matched both the absolute \`file_path\` and the repo-relative \`relative_path\` automatically; verify the path and line number exist.`
      : `To retrieve all chunks for this file, use \`filter: { file_path: "${filePath}" }\` (or \`relative_path\`) instead.`;
  return `${base} ${fallback}`;
}

function buildFallbackDocumentIdFilter(documentId: string): Record<string, string> {
  return { document_id: documentId };
}

function parseLineScopedDocumentId(documentId: string): { filePath: string; lineNumber: number } | null {
  const match = documentId.match(/^(.*):(\d+)$/);
  if (!match) return null;
  const lineNumber = Number(match[2]);
  if (!Number.isInteger(lineNumber) || lineNumber <= 0) return null;
  const filePath = match[1];
  if (!filePath) return null;
  return { filePath, lineNumber };
}

function formatLocation(filePath: string, lineNumber?: number): string {
  return lineNumber !== undefined ? `${filePath}:${lineNumber}` : filePath;
}

function metadataNumber(value: unknown): number | undefined {
  if (typeof value === 'number' && Number.isFinite(value)) return value;
  if (typeof value === 'string' && value.trim() !== '') {
    const parsed = Number(value);
    if (Number.isFinite(parsed)) return parsed;
  }
  return undefined;
}

function selectChunkForLine(
  documents: RetrievedDocument[],
  lineNumber: number
): RetrievedDocument | undefined {
  let bestBefore: { doc: RetrievedDocument; start: number } | undefined;

  for (const doc of documents) {
    const metadata = doc.metadata ?? {};
    const exactLine = metadataNumber(metadata['line_number']);
    if (exactLine === lineNumber) return doc;

    const chunkStart = metadataNumber(metadata['chunk_start_line']);
    const chunkEnd = metadataNumber(metadata['chunk_end_line']);
    if (
      chunkStart !== undefined &&
      chunkEnd !== undefined &&
      lineNumber >= chunkStart &&
      lineNumber <= chunkEnd
    ) {
      return doc;
    }

    if (chunkStart !== undefined && chunkStart <= lineNumber) {
      if (!bestBefore || chunkStart > bestBefore.start) {
        bestBefore = { doc, start: chunkStart };
      }
    }
  }

  return bestBefore?.doc;
}

function failureResponse(message: string, hint?: string): RetrieveResponse {
  const response: RetrieveResponse = {
    success: false,
    documents: [],
    total: 0,
    hasMore: false,
    message,
  };
  if (hint !== undefined) {
    response.hint = hint;
  }
  return response;
}

/** Returned when the caller passes argument names retrieve does not accept. */
function invalidArgsResponse(unknownArgs: string[]): RetrieveResponse {
  const searchHint = unknownArgs.includes('query')
    ? ' `retrieve` does not search by content — use the `search` tool for queries.'
    : '';
  return failureResponse(
    `Unknown retrieve parameter(s): ${unknownArgs.join(', ')}.${searchHint} ` +
      `Valid parameters: ${RETRIEVE_ARG_KEYS.join(', ')}.`,
    buildUnknownArgsHint(unknownArgs)
  );
}

/** Returned when the caller passes scope but it cannot be resolved. */
function unresolvedTenantResponse(collection: RetrieveCollectionType): RetrieveResponse {
  const scopeHint =
    collection === 'libraries'
      ? 'Pass `libraryName` for libraries.'
      : 'Pass `cwd` (to auto-detect the project) or `projectId` (for projects and scratchpad).';
  return failureResponse(
    `Cannot retrieve from "${collection}" without a resolvable scope. ` +
      'Pass `cwd` (to auto-detect the project) or `projectId` (for projects), ' +
      'or `libraryName` (for libraries).',
    scopeHint
  );
}

/**
 * Verify a retrieved point matches the caller's scope.
 * Returns `true` when the point belongs to the resolved tenant/library
 * (or when the collection is rules, which is intentionally mixed).
 */
function payloadMatchesScope(
  payload: Record<string, unknown> | null | undefined,
  collection: RetrieveCollectionType,
  resolvedProjectId: string | undefined,
  libraryName: string | undefined
): boolean {
  if (!payload) return false;
  switch (collection) {
    case 'projects': {
      if (!resolvedProjectId) return false;
      return payload[FIELD_TENANT_ID] === resolvedProjectId;
    }
    case 'libraries': {
      if (!libraryName) return false;
      // The libraries collection stores `library_name` on every point;
      // some legacy paths also key by `tenant_id`. Accept either match.
      return (
        payload[FIELD_LIBRARY_NAME] === libraryName || payload[FIELD_TENANT_ID] === libraryName
      );
    }
    case 'rules':
      // Rules are explicitly mixed-tenancy. Direct-by-ID lookup is
      // therefore not gated on caller scope.
      return true;
    case 'scratchpad':
      // Scratchpad is project-scoped; verify tenant_id matches.
      if (!resolvedProjectId) return false;
      return payload[FIELD_TENANT_ID] === resolvedProjectId;
    default:
      return false;
  }
}

export class RetrieveTool {
  private readonly qdrantClient: QdrantClient;
  private readonly projectDetector: ProjectDetector;
  private readonly daemonClient: DaemonClient | null;
  private readonly stateManager: SqliteStateManager | null;

  constructor(
    config: RetrieveToolConfig,
    projectDetector: ProjectDetector,
    daemonClient?: DaemonClient,
    stateManager?: SqliteStateManager
  ) {
    this.qdrantClient = getQdrantClient({
      url: config.qdrantUrl,
      apiKey: config.qdrantApiKey,
      timeout: config.qdrantTimeout ?? 5000,
    });
    this.projectDetector = projectDetector;
    this.daemonClient = daemonClient ?? null;
    this.stateManager = stateManager ?? null;
  }

  async retrieve(options: RetrieveOptions): Promise<RetrieveResponse> {
    const {
      documentId,
      filePath,
      lineNumber,
      collection = 'projects',
      filter,
      limit = 10,
      offset = 0,
      projectId,
      libraryName,
      unknownArgs,
    } = options;

    const startTime = Date.now();
    const eventId = randomUUID();

    const collectionName = getCollectionName(collection);
    const queryText =
      documentId ??
      (filePath
        ? formatLocation(filePath, lineNumber)
        : filter
          ? JSON.stringify(filter).slice(0, 500)
          : `:${collection}`);

    // Log start. queryText carries documentId for by-id lookups, a
    // filePath/lineNumber locator for exact-search hits, or a compact
    // filter summary for by-filter scans, so retrieve events remain
    // self-describing under `wqm admin token-savings`. The escalation
    // parent link is deliberately attached AFTER argument validation
    // (a refused retrieve never delivered a document, so it must not
    // mark the origin search as escalated).
    const logStart = (parentEventId?: string): void => {
      logSearchEvent(this.daemonClient, {
        id: eventId,
        actor: 'claude',
        tool: 'mcp_qdrant',
        op: 'retrieve',
        queryText,
        topK: limit,
        projectId: projectId,
        parentEventId,
      });
    };

    // Refuse unknown argument names before any scope resolution. Silently
    // dropping them would turn a mis-shaped call into an unrelated error
    // (or worse, an unintended broad read) further down.
    if (unknownArgs && unknownArgs.length > 0) {
      logStart();
      const result = invalidArgsResponse(unknownArgs);
      this.finishRetrieve(eventId, result, startTime, 'invalid_args');
      return result;
    }

    if (lineNumber !== undefined && !filePath) {
      logStart();
      const result = failureResponse(
        'lineNumber requires filePath.',
        'Pass `filePath` together with `lineNumber` for exact-search hits.'
      );
      this.finishRetrieve(eventId, result, startTime, 'invalid_args');
      return result;
    }

    // Effectiveness signals (spec 20 §1.2): when this retrieve targets a
    // document/path a recent same-session search returned, link the event
    // to that search via parent_event_id — the `token_savings` view counts
    // it into the origin's had_escalation ("the trimmed payload wasn't
    // enough; the agent paid for the full document"). The view additionally
    // requires result_count > 0, so a linked-but-empty retrieve still does
    // not count as an escalation.
    const escalationRefs = [
      documentId,
      filePath,
      typeof filter?.['document_id'] === 'string' ? (filter['document_id'] as string) : undefined,
    ].filter((r): r is string => typeof r === 'string' && r !== '');
    logStart(
      effectivenessTracker.findOrigin(currentSessionId(), escalationRefs, Date.now())
    );

    // F-002 / F-011: resolve the tenant context up front so that BOTH
    // by-id verification AND by-filter scoping share the same answer. For
    // projects reads also resolve the branch scope now, so the scroll paths
    // (document_id fallback, path locator, metadata filter) are scoped to the
    // caller's branch instead of leaking stale chunks tagged on other
    // branches — the same default-to-current-branch rule search / grep /
    // search_exact already apply.
    let resolvedProjectId: string | undefined;
    let branchScope: BranchScope = {};
    if (collection === 'projects' || collection === 'scratchpad') {
      const identity = await resolveProjectIdentity(
        this.projectDetector,
        projectId,
        true,
        this.stateManager ?? undefined
      );
      resolvedProjectId = identity.projectId;
      // Branch scoping applies to project chunks only. Scratchpad notes are
      // branch-agnostic (pinned to "main" at write time), so scoping them to
      // the session's branch — or its base-branch widening — empties reads on
      // any repo whose base branch is not literally "main".
      if (collection === 'projects') {
        branchScope = this.resolveBranchScope(options.branch, identity);
      }
    }

    if (filePath) {
      const locationParams = {
        collectionName,
        collection,
        filePath,
        limit,
        offset,
        projectId: resolvedProjectId,
        libraryName,
        branchScope,
        ...(filter ? { filter } : {}),
        ...(lineNumber !== undefined ? { lineNumber } : {}),
      };
      const result = await this.retrieveByLocation(locationParams);
      this.finishRetrieve(eventId, result, startTime);
      return result;
    }

    if (documentId) {
      const result = await this.retrieveById(
        collectionName,
        collection,
        documentId,
        resolvedProjectId,
        libraryName,
        limit,
        offset,
        branchScope
      );
      this.finishRetrieve(eventId, result, startTime);
      return result;
    }

    // F-011: project-scope retrieve without a resolved tenant MUST refuse
    // to scroll. Same rule for libraries when no library_name is given.
    if (collection === 'projects' && !resolvedProjectId) {
      const result = unresolvedTenantResponse('projects');
      this.finishRetrieve(eventId, result, startTime, 'unresolved_tenant');
      return result;
    }
    if (collection === 'scratchpad' && !resolvedProjectId) {
      const result = unresolvedTenantResponse('scratchpad');
      this.finishRetrieve(eventId, result, startTime, 'unresolved_tenant');
      return result;
    }
    if (collection === 'libraries' && !libraryName) {
      const result = unresolvedTenantResponse('libraries');
      this.finishRetrieve(eventId, result, startTime, 'unresolved_tenant');
      return result;
    }

    const filterParams: {
      collectionName: string;
      collection: RetrieveCollectionType;
      filter?: Record<string, string>;
      limit: number;
      offset: number;
      projectId: string | undefined;
      libraryName: string | undefined;
      branchScope: BranchScope;
    } = {
      collectionName,
      collection,
      limit,
      offset,
      projectId: resolvedProjectId,
      libraryName,
      branchScope,
    };
    if (filter) filterParams.filter = filter;

    const result = await this.retrieveByFilter(filterParams);
    this.finishRetrieve(eventId, result, startTime);
    return result;
  }

  /**
   * Retrieve by file locator. This is the canonical path for exact-search
   * hits, which carry file_path + line_number rather than a Qdrant point id.
   */
  private async retrieveByLocation(params: {
    collectionName: string;
    collection: RetrieveCollectionType;
    filePath: string;
    lineNumber?: number;
    filter?: Record<string, string>;
    limit: number;
    offset: number;
    projectId: string | undefined;
    libraryName: string | undefined;
    branchScope?: BranchScope;
  }): Promise<RetrieveResponse> {
    const {
      collectionName,
      collection,
      filePath,
      lineNumber,
      filter,
      limit,
      offset,
      projectId,
      libraryName,
      branchScope,
    } = params;

    // The exact-search `id` field carries the ABSOLUTE file_path, but agents
    // often copy the repo-relative `relative_path` from a result instead. Match
    // either field so the documented locator resolves regardless of which path
    // form was passed, rather than silently scrolling the whole tenant.
    const fallback = await this.retrieveByFilter({
      collectionName,
      collection,
      ...(filter ? { filter } : {}),
      pathLocator: filePath,
      limit: lineNumber !== undefined ? Math.max(limit, 1000) : limit,
      offset,
      projectId,
      libraryName,
      branchScope: branchScope ?? {},
    });

    if (!fallback.success) {
      return fallback;
    }

    if (lineNumber === undefined) {
      return fallback;
    }

    const selected = selectChunkForLine(fallback.documents, lineNumber);
    if (selected) {
      return {
        ...fallback,
        documents: [selected],
        total: 1,
        hasMore: false,
      };
    }

    return failureResponse(
      `Document not found: ${formatLocation(filePath, lineNumber)}`,
      buildLocationNotFoundHint(filePath, lineNumber)
    );
  }

  /** Record post-execution metrics for a retrieve call. */
  private finishRetrieve(
    eventId: string,
    response: RetrieveResponse,
    startTime: number,
    outcome?: string
  ): void {
    const economy = computeRetrieveEconomy(response.documents);
    const finish: import('../clients/search-event-queries.js').ToolEventFinish = {
      resultCount: response.documents.length,
      latencyMs: Date.now() - startTime,
      bytesIn: economy.bytesIn,
      bytesOut: economy.bytesOut,
      toolVersion: MCP_SERVER_VERSION,
    };
    if (outcome !== undefined) finish.outcome = outcome;
    finishToolEvent(this.daemonClient, eventId, finish);
  }

  /**
   * Direct point lookup by ID. Verifies the returned point matches the
   * caller's resolved scope (F-002). A mismatch is returned as a clean
   * "not found" rather than the foreign document, so the leak path
   * collapses to the same response as a genuinely missing ID.
   */
  private async retrieveById(
    collectionName: string,
    collection: RetrieveCollectionType,
    documentId: string,
    resolvedProjectId: string | undefined,
    libraryName: string | undefined,
    limit: number,
    offset: number,
    branchScope: BranchScope = {}
  ): Promise<RetrieveResponse> {
    // F-002: project-scope and library-scope lookups MUST resolve their
    // scope before reading. Without it, the verification step below
    // cannot distinguish "owned" from "foreign", so refuse the read.
    if (collection === 'projects' && !resolvedProjectId) {
      return unresolvedTenantResponse('projects');
    }
    if (collection === 'scratchpad' && !resolvedProjectId) {
      return unresolvedTenantResponse('scratchpad');
    }
    if (collection === 'libraries' && !libraryName) {
      return unresolvedTenantResponse('libraries');
    }

    const lineScopedId = parseLineScopedDocumentId(documentId);
    if (lineScopedId) {
      return this.retrieveByLocation({
        collectionName,
        collection,
        filePath: lineScopedId.filePath,
        lineNumber: lineScopedId.lineNumber,
        limit,
        offset,
        projectId: resolvedProjectId,
        libraryName,
        branchScope,
      });
    }

    try {
      const result = await this.qdrantClient.retrieve(collectionName, {
        ids: [documentId],
        with_payload: true,
        with_vector: false,
      });

      const point = result[0];
      if (!point) {
        const fallback = await this.retrieveByFilter({
          collectionName,
          collection,
          filter: buildFallbackDocumentIdFilter(documentId),
          limit: 10,
          offset: 0,
          projectId: resolvedProjectId,
          libraryName,
          branchScope,
        });

        if (fallback.success && fallback.documents.length > 0) {
          return fallback;
        }
        if (!fallback.success) {
          return fallback;
        }

        return failureResponse(
          `Document not found: ${documentId}`,
          buildNotFoundHint(documentId)
        );
      }

      // F-002: ownership check. A mismatch is reported as not-found —
      // we MUST NOT leak that the ID exists in a foreign tenant.
      if (!payloadMatchesScope(point.payload, collection, resolvedProjectId, libraryName)) {
        return failureResponse(
          `Document not found: ${documentId}`,
          buildNotFoundHint(documentId)
        );
      }

      const document: RetrievedDocument = {
        id: String(point.id),
        content: (point.payload?.[FIELD_CONTENT] as string) ?? '',
        metadata: extractMetadata(point.payload),
      };

      return { success: true, documents: [document], total: 1, hasMore: false };
    } catch (error) {
      return failureResponse(
        `Failed to retrieve document: ${error instanceof Error ? error.message : 'unknown error'}`,
        'Check Qdrant connectivity and confirm the collection name. If this started from `search` or `list`, make sure you passed the result `id` field.'
      );
    }
  }

  private async retrieveByFilter(params: {
    collectionName: string;
    collection: RetrieveCollectionType;
    filter?: Record<string, string>;
    pathLocator?: string;
    limit: number;
    offset: number;
    projectId: string | undefined;
    libraryName: string | undefined;
    branchScope?: BranchScope;
  }): Promise<RetrieveResponse> {
    const {
      collectionName,
      collection,
      filter,
      pathLocator,
      limit,
      offset,
      projectId,
      libraryName,
      branchScope,
    } = params;
    const branch = branchScope?.branch;
    const fallbackBranch = branchScope?.fallbackBranch;

    const scroll = (useBranch: boolean) => {
      const qdrantFilter = this.buildFilter(
        collection,
        filter,
        projectId,
        libraryName,
        pathLocator,
        useBranch ? branch : undefined,
        useBranch ? fallbackBranch : undefined
      );
      const scrollRequest: {
        limit: number;
        offset?: number;
        with_payload: boolean;
        with_vector: boolean;
        filter?: Record<string, unknown>;
      } = { limit: limit + 1, with_payload: true, with_vector: false };
      if (offset > 0) scrollRequest.offset = offset;
      if (qdrantFilter) scrollRequest.filter = qdrantFilter;
      return this.qdrantClient.scroll(collectionName, scrollRequest);
    };

    try {
      let result = await scroll(true);

      // Auto-widen (mirrors grep / search_exact #151): a branch-scoped scroll
      // that finds nothing may just be missing a version the daemon tagged
      // under another branch — unchanged files live under the base branch, and
      // a file can exist only on a different feature branch. `retrieve` is a
      // known-point lookup, so widen to all branches rather than returning
      // empty. The tenant filter still applies, so this never crosses project
      // boundaries.
      const branchApplied = branchFilterClause(branch, fallbackBranch) !== null;
      if (branchApplied && result.points.length === 0) {
        result = await scroll(false);
      }

      const hasMore = result.points.length > limit;
      const points = hasMore ? result.points.slice(0, limit) : result.points;

      const documents: RetrievedDocument[] = points.map((point) => ({
        id: String(point.id),
        content: (point.payload?.[FIELD_CONTENT] as string) ?? '',
        metadata: extractMetadata(point.payload),
      }));

      return { success: true, documents, total: documents.length, hasMore };
    } catch (error) {
      const errorMessage = error instanceof Error ? error.message : 'unknown error';
      if (errorMessage.includes('not found') || errorMessage.includes("doesn't exist")) {
        return {
          success: true,
          documents: [],
          total: 0,
          hasMore: false,
          message: 'Collection not found or empty',
        };
      }
      return failureResponse(
        `Failed to retrieve documents: ${errorMessage}`,
        'Check Qdrant connectivity and confirm the collection name.'
      );
    }
  }

  /**
   * Build the Qdrant filter for scroll-based retrieve.
   *
   * Callers MUST have already validated the scope (`projects` →
   * `projectId`; `libraries` → `libraryName`) — this helper trusts the
   * inputs and is no longer responsible for refusing on an unresolvable
   * tenant. That refusal happens in `retrieve()`.
   */
  private buildFilter(
    collection: RetrieveCollectionType,
    filter?: Record<string, string>,
    projectId?: string,
    libraryName?: string,
    pathLocator?: string,
    branch?: string,
    fallbackBranch?: string
  ): Record<string, unknown> | null {
    const mustConditions: Record<string, unknown>[] = [];

    if ((collection === 'projects' || collection === 'scratchpad') && projectId) {
      mustConditions.push({ key: FIELD_TENANT_ID, match: { value: projectId } });
    } else if (collection === 'libraries' && libraryName) {
      mustConditions.push({ key: FIELD_TENANT_ID, match: { value: libraryName } });
    }

    // Branch scope applies only to the branch-partitioned collections. Reuses
    // the exact clause `search` builds (`branchFilterClause`) so retrieve can
    // never drift from the canonical branch semantics.
    if (collection === 'projects' || collection === 'scratchpad') {
      const branchCond = branchFilterClause(branch, fallbackBranch);
      if (branchCond) mustConditions.push(branchCond);
    }

    if (filter) {
      for (const [key, value] of Object.entries(filter)) {
        mustConditions.push({ key, match: { value } });
      }
    }

    // A file locator matches against either the absolute `file_path` or the
    // repo-relative `relative_path` payload field, so an agent can pass whichever
    // path form a search/list result surfaced.
    if (pathLocator) {
      mustConditions.push({
        should: [
          { key: 'file_path', match: { value: pathLocator } },
          { key: 'relative_path', match: { value: pathLocator } },
        ],
      });
    }

    return mustConditions.length > 0 ? { must: mustConditions } : null;
  }

  /**
   * Resolve the branch scope for a projects-collection read. Mirrors search /
   * grep / search_exact: default to the caller's current Git branch, widened to
   * the base branch the daemon tags unchanged files under. An explicit `branch`
   * (including `"*"` for cross-branch) is honored verbatim. Without a
   * `stateManager` (e.g. in unit tests) only the effective branch is applied.
   */
  private resolveBranchScope(
    explicitBranch: string | undefined,
    identity: { projectId: string | undefined; projectPath: string | undefined }
  ): BranchScope {
    const effectiveBranch = resolveEffectiveBranch({
      explicitBranch,
      scope: 'project',
      projectId: identity.projectId,
      projectPath: identity.projectPath,
    });
    const scope: BranchScope = {};
    if (effectiveBranch !== undefined) scope.branch = effectiveBranch;

    const concreteEffective = concreteBranchFilter(effectiveBranch);
    if (identity.projectId && concreteEffective && this.stateManager) {
      const watchFolderId = this.stateManager.getWatchFolderIdByTenantId(identity.projectId);
      const baseBranch = watchFolderId
        ? this.stateManager.getBaseBranch(watchFolderId, concreteEffective)
        : null;
      const fallbackBranch = resolveFallbackBranch({ effectiveBranch, baseBranch });
      if (fallbackBranch !== undefined) scope.fallbackBranch = fallbackBranch;
    }
    return scope;
  }
}
