/**
 * Search tool facade — delegates to domain-specific modules.
 *
 * - search-types.ts: Types, interfaces, constants
 * - search-filters.ts: Filter construction, collection determination
 * - search-qdrant.ts: Qdrant search, RRF fusion, parent context, fallback
 * - search-exact.ts: FTS5 exact/substring search via daemon
 * - search-expansion.ts: Tag-based BM25 query expansion
 * - search-helpers.ts: Phase helpers (project context, embeddings, fan-out, finalize)
 */

import { randomUUID } from 'node:crypto';
import type { QdrantClient } from '@qdrant/js-client-rest';
import { getQdrantClient } from '../clients/qdrant-client-factory.js';
import { effectivenessTracker } from '../clients/effectiveness-signals.js';
import {
  createQueryTranslatorFromEnv,
  type QueryTranslator,
} from '../clients/query-translator.js';
import { fuseQueryLegs } from './search-query-fusion.js';
import { resolveTranslatedQuery } from './search-translated-leg.js';
import { recordTranslatedLegHits, recordTranslatedLegSkipped } from '../telemetry/metrics.js';
import type { DaemonClient } from '../clients/daemon-client.js';
import type { SqliteStateManager } from '../clients/sqlite-state-manager.js';
import type { SearchDbReader } from '../clients/search-db-reader.js';
import type { ProjectDetector } from '../utils/project-detector.js';
import { SERVER_VERSION as MCP_SERVER_VERSION } from '../server-types.js';

// Re-export all types so existing imports from './search.js' continue to work
export type {
  SearchMode,
  SearchScope,
  SearchOptions,
  ParentContext,
  SearchResult,
  SearchResponse,
  SearchToolConfig,
  FilterParams,
  SearchCollectionParams,
  GraphContext,
  GraphContextNode,
} from './search-types.js';

import type {
  SearchOptions,
  SearchResponse,
  SearchToolConfig,
  ParentContext,
  SearchResult,
} from './search-types.js';
import {
  DEFAULT_LIMIT,
  DEFAULT_SCORE_THRESHOLD,
  DEFAULT_EXPANSION_WEIGHT,
  DEFAULT_MAX_EXPANDED_KEYWORDS,
  SCRATCHPAD_COLLECTION,
} from './search-types.js';
import {
  applyEffectiveBranch,
  concreteBranchFilter,
  resolveFallbackBranch,
} from './branch-scope.js';
import { worktreeReadNote } from './worktree-note.js';

import { determineCollections } from './search-filters.js';
import { diagnoseEmptyResult, EMPTY_DIAGNOSIS_PROBE_LIMIT } from './empty-diagnosis.js';
import { retrieveParent, collectionExists } from './search-qdrant.js';
import { searchExact } from './search-exact.js';
import { expandSparseWithTags } from './search-expansion.js';
import { shapeHitPayloads } from './search-shaping.js';
import {
  resolveProjectContext,
  logSearchEventPre,
  generateEmbeddings,
  searchAllCollections,
  searchScratchpadLane,
  finalizeResults,
  reconcileNextOffset,
  unresolvedProjectResponse,
} from './search-helpers.js';

/** Format an explicit status_reason for the F-014 base-point degradation
 * case. Used in both the embedding-fallback path and the normal
 * fan-out path so callers always see why instance isolation was
 * bypassed. */
function formatBasePointsDegradedReason(activeCount: number | undefined): string {
  const count = activeCount ?? 'too many';
  return (
    `Worktree/instance isolation degraded: project has ${count} active base points, ` +
    `exceeding the 500-filter cap; tenant filter still applies but base-point ` +
    `narrowing was bypassed. Narrow further with pathGlob, branch, or component to ` +
    `restore worktree-level isolation.`
  );
}

/**
 * Search tool for hybrid semantic + keyword search
 */
export class SearchTool {
  private readonly qdrantClient: QdrantClient;
  private readonly daemonClient: DaemonClient;
  private readonly _stateManager: SqliteStateManager;
  private readonly projectDetector: ProjectDetector;
  private readonly enableTagExpansion: boolean;
  private readonly expansionWeight: number;
  private readonly maxExpandedKeywords: number;
  private readonly searchDbReader: SearchDbReader | undefined;
  /** Null unless WQM_TRANSLATE_BASE_URL + _MODEL are both set (default: off). */
  private readonly queryTranslator: QueryTranslator | null;

  constructor(
    config: SearchToolConfig,
    daemonClient: DaemonClient,
    stateManager: SqliteStateManager,
    projectDetector: ProjectDetector,
    searchDbReader?: SearchDbReader
  ) {
    this.qdrantClient = getQdrantClient({
      url: config.qdrantUrl,
      apiKey: config.qdrantApiKey,
      timeout: config.qdrantTimeout ?? 5000,
    });
    this.daemonClient = daemonClient;
    this._stateManager = stateManager;
    this.projectDetector = projectDetector;
    this.enableTagExpansion = config.enableTagExpansion ?? true;
    this.expansionWeight = config.expansionWeight ?? DEFAULT_EXPANSION_WEIGHT;
    this.maxExpandedKeywords = config.maxExpandedKeywords ?? DEFAULT_MAX_EXPANDED_KEYWORDS;
    this.searchDbReader = searchDbReader;
    this.queryTranslator = createQueryTranslatorFromEnv();
  }

  get stateManager(): SqliteStateManager {
    return this._stateManager;
  }

  private async prepareEmbeddings(
    options: SearchOptions,
    query: string,
    mode: import('./search-types.js').SearchMode,
    collectionsToSearch: string[],
    currentProjectId: string | undefined,
    basePoints: string[] | undefined
  ): Promise<
    | { fallback: SearchResponse }
    | { denseEmbedding: number[] | undefined; sparseVector: Record<number, number> | undefined }
  > {
    const embeddings = await generateEmbeddings(
      this.daemonClient,
      this.qdrantClient,
      query,
      mode,
      options,
      collectionsToSearch,
      { currentProjectId, basePoints }
    );
    if ('fallback' in embeddings) return embeddings;
    let { denseEmbedding, sparseVector } = embeddings;
    if (sparseVector && this.enableTagExpansion && (mode === 'hybrid' || mode === 'keyword')) {
      sparseVector = await expandSparseWithTags(
        this.daemonClient,
        this._stateManager,
        query,
        sparseVector,
        collectionsToSearch,
        this.expansionWeight,
        this.maxExpandedKeywords,
        currentProjectId
      );
    }
    return { denseEmbedding, sparseVector };
  }

  private async resolveContextAndLog(
    options: SearchOptions,
    query: string,
    limit: number,
    scope: import('./search-types.js').SearchScope,
    projectId: string | undefined,
    eventId: string
  ): Promise<{
    searchStartMs: number;
    currentProjectId: string | undefined;
    basePoints: string[] | undefined;
    effectiveBranch: string | undefined;
    basePointsDegraded: boolean;
    basePointsActiveCount: number | undefined;
  }> {
    const searchStartMs = Date.now();
    const resolution = await resolveProjectContext(
      projectId,
      scope,
      this.projectDetector,
      this._stateManager
    );
    const effectiveBranch =
      options.branch === undefined && scope === 'project'
        ? resolution.currentBranch
        : options.branch;
    logSearchEventPre(this._stateManager, eventId, resolution.currentProjectId, query, limit, {
      collection: options.collection,
      scope,
      branch: effectiveBranch,
      fileType: options.fileType,
      libraryName: options.libraryName,
      tag: options.tag,
      actor: options.telemetryActor,
    });
    return {
      searchStartMs,
      currentProjectId: resolution.currentProjectId,
      basePoints: resolution.basePoints,
      effectiveBranch,
      basePointsDegraded: resolution.basePointsDegraded ?? false,
      basePointsActiveCount: resolution.basePointsActiveCount,
    };
  }

  async search(options: SearchOptions): Promise<SearchResponse> {
    // Single shaping pass at the outer boundary keeps the budget-cap
    // logic in one place and applies to every exit (exact, fallback,
    // and normal pipeline) without sprinkling it through helpers.
    // The eventId is generated here so the post-shape token-economy
    // update can attribute metrics to the same search_events row that
    // the inner pipeline already wrote/updated. Spec:
    // docs/specs/20-token-economy-instrumentation.md
    const eventId = randomUUID();
    const response = await this.executeSearch(options, eventId);
    const { response: shaped, metrics } = shapeHitPayloads(response, options);
    // Reconcile pagination with the byte budget (P1.5 B × C): shaping runs the
    // byte budget AFTER executeSearch set next_offset = offset+limit. If it
    // dropped trailing CODE hits, repoint next_offset at the first dropped hit
    // so the next page resumes there (else those ranks are unreachable). Count
    // only code hits — the scratchpad lane trails and is page-1-only.
    const pageOffset = Math.max(0, options.offset ?? 0);
    const preCode = response.results.filter((r) => r.collection !== SCRATCHPAD_COLLECTION).length;
    const keptCode = shaped.results.filter((r) => r.collection !== SCRATCHPAD_COLLECTION).length;
    const reconciled = reconcileNextOffset(
      pageOffset,
      preCode,
      keptCode,
      shaped.next_offset !== undefined
    );
    if (reconciled !== undefined) shaped.next_offset = reconciled;
    this._stateManager.updateSearchEventEconomy(eventId, {
      bytesIn: metrics.bytesInShaped,
      bytesOut: metrics.bytesOutShaped,
      hitsTruncated: metrics.hitsTruncated,
      shapeMode: metrics.mode,
      toolVersion: MCP_SERVER_VERSION,
    });
    // Effectiveness signals (spec 20 §1.2): remember what this search
    // returned so a retrieve() of one of these hits within the escalation
    // window links back via parent_event_id. Covers the exact path too —
    // it exits through this same shaping boundary. Record every spelling a
    // later retrieve might use: point id, document_id, absolute and
    // repo-relative paths (ref matching is exact-string).
    const hitRefs: string[] = [];
    for (const r of shaped.results) {
      hitRefs.push(String(r.id));
      for (const key of ['document_id', 'file_path', 'relative_path']) {
        const v = r.metadata?.[key];
        if (typeof v === 'string' && v !== '') hitRefs.push(v);
      }
    }
    effectivenessTracker.noteHits(eventId, hitRefs);
    // Worktree callers get MAIN-anchored paths; tell them how to Read their own
    // copy once (only when there are results to translate).
    if (shaped.results.length > 0) {
      const wtNote = worktreeReadNote();
      if (wtNote) shaped.worktree = wtNote;
    }
    return shaped;
  }

  private async executeSearch(options: SearchOptions, eventId: string): Promise<SearchResponse> {
    if (options.exact) {
      return searchExact(
        this.qdrantClient,
        this.daemonClient,
        this._stateManager,
        this.projectDetector,
        options,
        eventId,
        this.searchDbReader
      );
    }
    const mode = options.mode ?? 'semantic';
    const limit = options.limit ?? DEFAULT_LIMIT;
    const scope = options.scope ?? 'project';
    const collectionsToSearch = determineCollections(
      options.collection,
      scope,
      options.includeLibraries ?? false
    );
    const {
      searchStartMs,
      currentProjectId,
      basePoints,
      effectiveBranch,
      basePointsDegraded,
      basePointsActiveCount,
    } = await this.resolveContextAndLog(
      options,
      options.query,
      limit,
      scope,
      options.projectId,
      eventId
    );
    // F-004 (semantic parity): a project-scoped search whose tenant could not be
    // resolved MUST refuse rather than run unfiltered. buildProjectCondition drops
    // the tenant filter on an undefined projectId, so proceeding would silently
    // return cross-tenant results (another repo's code). grep / search_exact /
    // retrieve / list / graph all already refuse here — the semantic path was the
    // lone gap. Explicit scope:"all"/"global" is unaffected (it intends no tenant).
    if (scope === 'project' && !currentProjectId) {
      return unresolvedProjectResponse(options.query, mode, scope);
    }
    const concreteEffective = concreteBranchFilter(effectiveBranch);
    let fallbackBranch: string | undefined;
    if (currentProjectId && concreteEffective) {
      const watchFolderId = this._stateManager.getWatchFolderIdByTenantId(currentProjectId);
      const baseBranch = watchFolderId
        ? this._stateManager.getBaseBranch(watchFolderId, concreteEffective)
        : null;
      fallbackBranch = resolveFallbackBranch({ effectiveBranch, baseBranch });
    }
    const baseEffectiveOptions = applyEffectiveBranch(options, effectiveBranch);
    const effectiveOptions = fallbackBranch
      ? { ...baseEffectiveOptions, fallbackBranch }
      : baseEffectiveOptions;
    const embeddings = await this.prepareEmbeddings(
      effectiveOptions,
      effectiveOptions.query,
      mode,
      collectionsToSearch,
      currentProjectId,
      basePoints
    );
    if ('fallback' in embeddings) {
      // F-014: if base-point isolation degraded, surface it on the
      // fallback response too — tenant filter still applies via
      // fallbackSearch, but instance/worktree narrowing was bypassed.
      if (basePointsDegraded) {
        embeddings.fallback.status = 'uncertain';
        const reason = formatBasePointsDegradedReason(basePointsActiveCount);
        embeddings.fallback.status_reason = embeddings.fallback.status_reason
          ? `${embeddings.fallback.status_reason}; ${reason}`
          : reason;
      }
      return embeddings.fallback;
    }
    const runFinalize = (opts: SearchOptions, fb: string | undefined): Promise<SearchResponse> =>
      this.runSearchAndFinalize(
        opts,
        mode,
        limit,
        scope,
        collectionsToSearch,
        eventId,
        searchStartMs,
        currentProjectId,
        basePoints,
        fb,
        basePointsDegraded,
        basePointsActiveCount,
        embeddings.denseEmbedding,
        embeddings.sparseVector
      );

    // Second leg (query translation, off by default): kick the SLM off BEFORE
    // awaiting the primary search so the round-trip overlaps it instead of
    // adding to it. Every failure path resolves to `query: null`, which leaves
    // `primary` exactly as it is today. See search-translated-leg.ts.
    const translationPromise = resolveTranslatedQuery(
      options.query,
      this.queryTranslator,
      process.env,
      options.telemetryActor,
      options.telemetryIsBenchmark ?? false
    );
    const primaryPromise = runFinalize(effectiveOptions, fallbackBranch);
    const translation = await translationPromise;

    let primary: SearchResponse;
    if (translation.query === null) {
      primary = await primaryPromise;
    } else {
      // Same options and filters — only the text being embedded differs, so
      // the two legs are directly comparable rankings of the same corpus slice.
      const translatedOptions: SearchOptions = { ...effectiveOptions, query: translation.query };
      const translatedEmbeddings = await this.prepareEmbeddings(
        translatedOptions,
        translation.query,
        mode,
        collectionsToSearch,
        currentProjectId,
        basePoints
      );
      primary = await primaryPromise;
      // A degraded/fallback embedding on the SECOND leg is not worth surfacing
      // or acting on — drop the leg and keep today's single-leg answer. Counted
      // so the dashboard can tell "the leg contributed nothing" from "the leg
      // never ran"; without it, healthy translations against zero contributed
      // hits reads as a useless feature.
      if ('fallback' in translatedEmbeddings) {
        recordTranslatedLegSkipped();
      } else {
        const translatedLeg = await this.runSearchAndFinalize(
          translatedOptions,
          mode,
          limit,
          scope,
          collectionsToSearch,
          eventId,
          searchStartMs,
          currentProjectId,
          basePoints,
          fallbackBranch,
          basePointsDegraded,
          basePointsActiveCount,
          translatedEmbeddings.denseEmbedding,
          translatedEmbeddings.sparseVector
        );
        const fusedResults = fuseQueryLegs(primary.results, translatedLeg.results, { limit });
        // Effectiveness, not just activity: how many of the hits we are about to
        // return did the translated leg actually put there. A deployment where
        // the leg fires constantly but places nothing is paying latency for
        // nothing, and only this distinguishes the two.
        const originalKeys = new Set(primary.results.map((r) => `${r.collection}:${r.id}`));
        let bothLegs = 0;
        let translatedOnly = 0;
        for (const hit of fusedResults) {
          if (hit.metadata['_query_legs'] === 'both') bothLegs += 1;
          else if (!originalKeys.has(`${hit.collection}:${hit.id}`)) translatedOnly += 1;
        }
        recordTranslatedLegHits(bothLegs, translatedOnly);
        primary = {
          ...primary,
          results: fusedResults,
          // `total` is the size of the returned set (search-helpers.ts:719),
          // not a corpus count — it has to follow the fused list or the
          // response contradicts itself.
          total: fusedResults.length,
          translated_query: translation.query,
        };
      }
    }

    // Auto-widen on empty (parity with grep / search_exact): a branch-scoped
    // semantic search that finds nothing may be missing content the daemon
    // tagged under another branch (a file unchanged on the current branch stays
    // indexed under the branch it was last modified on). Re-run across ALL
    // branches, REUSING the embeddings (no re-embed — same dense/sparse vectors).
    // Fires ONLY for a concrete project-scoped branch and ONLY when zero CODE
    // results were found, so good branch-scoped results are never diluted. Gate
    // on CODE hits (not `results.length`): the project-memory scratchpad lane
    // appends notes into `results`, and a matching note must NOT mask missing
    // cross-branch code (that would defeat the parity). Never crosses the tenant
    // boundary — currentProjectId still filters the tenant, only branch widens.
    const codeHits = (r: SearchResponse): number =>
      r.results.filter((x) => x.collection !== SCRATCHPAD_COLLECTION).length;
    if (codeHits(primary) === 0 && scope === 'project' && concreteEffective && currentProjectId) {
      const widened = await runFinalize(applyEffectiveBranch(options, '*'), undefined);
      if (codeHits(widened) > 0) {
        const note = `No semantic matches on branch "${concreteEffective}" — widened to all branches; results may be from another indexed branch.`;
        widened.hint = widened.hint ? `${widened.hint} ${note}` : note;
        // The widen re-runs the ORIGINAL query only — runFinalize closes over
        // the original embeddings — so these results come from one leg even
        // when translation ran. Carry `translated_query` through anyway: it
        // reports that a translation was made, and dropping it here would have
        // the response deny work the caller was already told about on the
        // non-widened path. Re-running both legs widened is deliberately not
        // done: this fires only when the fused result was already empty, and it
        // would double the cost of an already-degraded path.
        if (translation.query !== null) widened.translated_query = translation.query;
        return widened;
      }
    }

    // Still empty (any branch-widen also came back empty): diagnose WHY, with the
    // same shared probes grep/exact use (CLAUDE.md shared-behavior rule) — in
    // 'semantic' mode so a well-formed pathGlob over relevant-less files is NOT
    // falsely blamed as malformed (the 1b fork), and a not-yet-indexed branch is
    // named. Only for a project-scoped miss with a tenant resolved; the probe
    // re-runs the SAME scope sans path filter, REUSING the embeddings (no
    // re-embed) via runFinalize.
    if (codeHits(primary) === 0 && scope === 'project' && currentProjectId) {
      const diagnosis = await diagnoseEmptyResult({
        tenantId: currentProjectId,
        branch: concreteEffective,
        pathGlob: options.pathGlob,
        pathExclude: options.pathExclude,
        searchDbReader: this.searchDbReader,
        mode: 'semantic',
        countWithoutPathFilter: async () => {
          const probeOptions: SearchOptions = { ...effectiveOptions };
          delete probeOptions.pathGlob;
          delete probeOptions.pathExclude;
          const probe = await runFinalize(probeOptions, fallbackBranch);
          return Math.min(codeHits(probe), EMPTY_DIAGNOSIS_PROBE_LIMIT);
        },
      });
      if (diagnosis) primary.hint = primary.hint ? `${primary.hint} ${diagnosis}` : diagnosis;
    }
    return primary;
  }

  private async runSearchCollections(
    options: SearchOptions,
    mode: import('./search-types.js').SearchMode,
    limit: number,
    scope: import('./search-types.js').SearchScope,
    collectionsToSearch: string[],
    currentProjectId: string | undefined,
    basePoints: string[] | undefined,
    fallbackBranch: string | undefined,
    denseEmbedding: number[] | undefined,
    sparseVector: Record<number, number> | undefined
  ): ReturnType<typeof searchAllCollections> {
    const scoreThreshold = options.scoreThreshold ?? DEFAULT_SCORE_THRESHOLD;
    return searchAllCollections(this.qdrantClient, {
      stateManager: this.stateManager,
      collectionsToSearch,
      scope,
      currentProjectId,
      basePoints,
      branch: options.branch,
      fallbackBranch,
      fileType: options.fileType,
      libraryName: options.libraryName,
      tag: options.tag,
      tags: options.tags,
      options,
      mode,
      denseEmbedding,
      sparseVector,
      limit,
      scoreThreshold,
    });
  }

  private async runSearchAndFinalize(
    options: SearchOptions,
    mode: import('./search-types.js').SearchMode,
    limit: number,
    scope: import('./search-types.js').SearchScope,
    collectionsToSearch: string[],
    eventId: string,
    searchStartMs: number,
    currentProjectId: string | undefined,
    basePoints: string[] | undefined,
    fallbackBranch: string | undefined,
    basePointsDegraded: boolean,
    basePointsActiveCount: number | undefined,
    denseEmbedding: number[] | undefined,
    sparseVector: Record<number, number> | undefined
  ): Promise<SearchResponse> {
    // Project-memory recall lane: enabled only for the default project scope,
    // when not targeting an explicit collection, with the lane on and a tenant
    // resolved. Resolve its tenant up front so the lane runs CONCURRENTLY with
    // the main collection fan-out — it reuses the same embeddings, so there is
    // no reason to serialize it behind the code search. `undefined` skips the
    // lane (resolves to []); failures inside the lane also degrade to [].
    const offset = Math.max(0, options.offset ?? 0);
    // Over-fetch the [offset, offset+limit] window (+1 probe so finalize can set
    // next_offset), then slice the page post-fusion in finalizeResults.
    const fetchLimit = limit + offset + 1;
    const laneProjectId =
      scope === 'project' &&
      !options.collection &&
      options.includeScratchpad !== false &&
      offset === 0
        ? currentProjectId
        : undefined;
    const [collectionsResult, scratchpadHits] = await Promise.all([
      this.runSearchCollections(
        options,
        mode,
        fetchLimit,
        scope,
        collectionsToSearch,
        currentProjectId,
        basePoints,
        fallbackBranch,
        denseEmbedding,
        sparseVector
      ),
      laneProjectId
        ? searchScratchpadLane(this.qdrantClient, {
            projectId: laneProjectId,
            mode,
            denseEmbedding,
            sparseVector,
            scoreThreshold: options.scoreThreshold ?? DEFAULT_SCORE_THRESHOLD,
          })
        : Promise.resolve<SearchResult[]>([]),
    ]);
    let { status, statusReason } = collectionsResult;
    if (basePointsDegraded) {
      // F-014: merge the explicit degradation message into the final
      // response so callers know instance isolation was bypassed.
      status = 'uncertain';
      const reason = formatBasePointsDegradedReason(basePointsActiveCount);
      statusReason = statusReason ? `${statusReason}; ${reason}` : reason;
    }
    return finalizeResults(this.qdrantClient, this.daemonClient, this._stateManager, {
      allResults: collectionsResult.allResults,
      mode,
      limit,
      options,
      eventId,
      searchStartMs,
      query: options.query,
      scope,
      collectionsToSearch,
      status,
      statusReason,
      currentProjectId,
      scratchpadHits,
    });
  }

  async retrieveParent(parentUnitId: string, collection: string): Promise<ParentContext | null> {
    return retrieveParent(this.qdrantClient, parentUnitId, collection);
  }

  async collectionExists(collectionName: string): Promise<boolean> {
    return collectionExists(this.qdrantClient, collectionName);
  }
}
