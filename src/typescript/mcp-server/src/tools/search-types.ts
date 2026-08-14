/**
 * Search tool types, interfaces, and constants.
 */

// Canonical collection names from native bridge (single source of truth)
import {
  COLLECTION_PROJECTS,
  COLLECTION_LIBRARIES,
  COLLECTION_SCRATCHPAD,
} from '../common/native-bridge.js';
import type { WorktreeReadNote } from './worktree-note.js';
export const PROJECTS_COLLECTION = COLLECTION_PROJECTS;
export const LIBRARIES_COLLECTION = COLLECTION_LIBRARIES;
export const SCRATCHPAD_COLLECTION = COLLECTION_SCRATCHPAD;

// Vector names for hybrid search
export const DENSE_VECTOR_NAME = 'dense';
export const SPARSE_VECTOR_NAME = 'sparse';

// RRF constant (k=60 is standard)
export const RRF_K = 60;

/** Parse a non-negative finite tuning value from an env var, else the default.
 *  Empty/unset/garbage values fall back — compose `${VAR:-}` passthroughs set
 *  empty strings, which must not zero out a tuning knob. */
export function tuningFromEnv(envVar: string, defaultValue: number): number {
  const raw = process.env[envVar];
  if (raw === undefined || raw.trim() === '') return defaultValue;
  const parsed = Number(raw);
  return Number.isFinite(parsed) && parsed >= 0 ? parsed : defaultValue;
}

// Default search parameters
export const DEFAULT_LIMIT = 10;
export const DEFAULT_SCORE_THRESHOLD = 0.3;
/** Per-hit text cap (in chars). Default 1500 keeps a 10-hit response well
 *  under typical MCP client per-tool-result token budgets (~25k chars). */
export const DEFAULT_MAX_BYTES_PER_HIT = 1500;
/** Global cap (in chars) on the SUMMED hit bodies of one response. The per-hit
 *  cap can't bound the total (N hits × cap), so once the running body total
 *  exceeds this, trailing hits are dropped (>=1 always kept) and
 *  `SearchResponse.budget_truncated` reports how many. ~24k chars ≈ 6k tokens,
 *  under the ~25k informal client budget. Set `maxResponseBytes: 0` to disable. */
export const DEFAULT_MAX_RESPONSE_BYTES = 24000;


// Tag expansion defaults
export const DEFAULT_EXPANSION_WEIGHT = 0.5;
export const DEFAULT_MAX_EXPANDED_KEYWORDS = 10;

export type SearchMode = 'hybrid' | 'semantic' | 'keyword';
export type SearchScope = 'project' | 'global' | 'all';

export interface SearchOptions {
  query: string;
  /** Telemetry-only override for the `search_events.actor` column (default
   *  "claude"). The benchmark harness sets "benchmark" so eval traffic can be
   *  excluded when mining REAL agent queries from the search history. */
  telemetryActor?: string;
  /**
   * Marks harness traffic so metrics can exclude it.
   *
   * The eval harness must report `telemetryActor: 'user'` — the search_events
   * CHECK permits only claude/user/daemon — so the actor alone cannot tell a
   * benchmark run from a person typing. Anything that decides on "real usage"
   * has to key off this instead, or one eval run (71 queries, 33 of them
   * Portuguese) lands in the human bucket and invents the very signal it is
   * being read for. Metrics-only; never persisted to search_events.
   */
  telemetryIsBenchmark?: boolean;
  collection?: string;
  mode?: SearchMode;
  limit?: number;
  scoreThreshold?: number;
  scope?: SearchScope;
  branch?: string;
  fileType?: string;
  /** Exclude test-classified files from results — a server-side must_not on
   *  the daemon's ingest tags (projects collection, semantic/hybrid path), so
   *  the result limit is spent on implementation hits instead of tests the
   *  agent would skip via `is_test`. Default: false. */
  excludeTests?: boolean;
  projectId?: string;
  libraryName?: string;
  includeLibraries?: boolean;
  /** Append a small, tenant-filtered scratchpad recall lane to project-scoped
   *  searches so project notes/snippets surface automatically alongside code
   *  (labeled `collection: "scratchpad"`, capped, never displacing code hits).
   *  Default: true for scope="project". Set false to skip. No effect for
   *  global/all scopes or when an explicit `collection` is targeted. */
  includeScratchpad?: boolean;
  tag?: string;
  /** Filter results by multiple concept tags (OR logic) */
  tags?: string[];
  /** When true, fetch parent unit context for each chunk result */
  expandContext?: boolean;
  /** File path glob filter (e.g., "**\/*.rs") — applies in both exact and semantic modes */
  pathGlob?: string;
  /** File path glob to EXCLUDE (hard filter) — drops any hit whose path matches,
   *  in both exact and semantic modes. Floats: `old_project/**` excludes the dir
   *  at the repo root AND at any nested depth. Complements `pathGlob` (include). */
  pathExclude?: string;
  /** Filter by project component (e.g., "daemon", "daemon.core"). Supports prefix matching. */
  component?: string;
  /** Internal: base branch to include for files unchanged on a feature branch. */
  fallbackBranch?: string;
  /** When true, use FTS5 exact/substring search instead of semantic search */
  exact?: boolean;
  /** Lines of context before/after matches (only for exact mode, default: 0) */
  contextLines?: number;
  /** When true, fetch 1-hop graph context for code symbol results */
  includeGraphContext?: boolean;
  /** Cross-encoder rerank of the top candidates (default: deployment setting,
   *  WQM_SEARCH_RERANK; code default is ON — set WQM_SEARCH_RERANK=0 to disable).
   *  Set false to skip the reranker (lower latency), or true to enable it for a call. */
  rerank?: boolean;
  /** Blend weight (0–1) for the cross-encoder score when reranking. The final
   *  pool order is `(1-w)·norm(rrf_boosted) + w·norm(rerank)` over min-max
   *  normalized scores. 1 = pure cross-encoder order (legacy replace
   *  behavior); 0 = reranking disabled. Default: WQM_SEARCH_RERANK_WEIGHT
   *  env, else 0.10 (measured default for the CodeRankEmbed index, 2026-06-22 A/B). */
  rerankWeight?: number;
  /** Per-hit text cap (in chars). Content longer than this is truncated
   *  with a marker pointing to retrieve() for the full chunk. Defaults
   *  to {@link DEFAULT_MAX_BYTES_PER_HIT}. Set to 0 to disable truncation. */
  maxBytesPerHit?: number;
  /** When true, drop chunk text bodies entirely and return only
   *  metadata (id, score, collection, title, path/symbol). Intended for
   *  pure discovery before a follow-up retrieve(). Default: false. */
  summary?: boolean;
  /** Response verbosity. `concise` (default) truncates each hit body to the
   *  per-hit cap; `detailed` returns full bodies (disables the per-hit cap);
   *  `packed` assembles ONE ranked, deduplicated context bundle under the
   *  response byte budget (see {@link SearchResponse.packed_bundle}) with
   *  metadata-only entries in `results`. An explicit `maxBytesPerHit`
   *  overrides the per-hit cap in concise/packed; the `summary` flag is
   *  stronger than all three. At the tool boundary `responseFormat:"summary"`
   *  is accepted and aliased to that flag (this field stays the three
   *  body-verbosity modes). */
  responseFormat?: 'concise' | 'detailed' | 'packed';
  /** Global cap (chars) on the summed hit bodies of the whole response; trailing
   *  hits beyond it are dropped (>=1 kept) and reported via
   *  {@link SearchResponse.budget_truncated}. Defaults to
   *  {@link DEFAULT_MAX_RESPONSE_BYTES}; set 0 to disable. */
  maxResponseBytes?: number;
  /** Pagination offset into the ranked results (default 0). The page
   *  [offset, offset+limit) is sliced AFTER fusion/dedup/rerank, so consecutive
   *  pages don't overlap. The scratchpad recall lane is a page-1-only supplement
   *  (omitted when offset>0). See {@link SearchResponse.next_offset}. */
  offset?: number;
}

/** Resolve the deployment rerank default from WQM_SEARCH_RERANK: ON unless
 *  explicitly disabled. Unset => ON (soft default — a weak w=0.10 blend that
 *  improves top1/MRR without recall loss); '0' => OFF (e.g. deployments with no
 *  rerank backend, to skip the failed-call round-trip); any other value => ON.
 *  A per-call `rerank` overrides this. Exported so search-helpers and search-eval
 *  resolve the deployment default identically. */
export function rerankEnabledByDefault(envValue: string | undefined): boolean {
  return envValue === undefined ? true : envValue !== '0';
}

/** Default score multiplier for a de-ranked (legacy) path. 0.2 sinks a matched
 *  hit well below live code (cosine hits sit ~0.4–0.7 → ~0.08–0.14 after) while
 *  keeping it in the result set, so a legacy dir never *hides* a needed file. */
export const DEFAULT_DERANK_PENALTY = 0.2;

/** Resolved soft de-rank configuration (see {@link resolveDerankConfig}). */
export interface DerankConfig {
  /** Path substrings; a hit whose relative OR absolute path CONTAINS any one
   *  is de-ranked (both are tested — a worktree-origin point only carries the
   *  distinguishing segment in its absolute path). Same match semantics as the
   *  daemon's `WQM_GRAPH_CENTRALITY_EXCLUDE` so the two knobs can share a value. */
  substrings: string[];
  /** Ranking-score multiplier in [0,1) applied to a matched hit. >=1 or empty
   *  `substrings` ⇒ the de-rank is a no-op. */
  penalty: number;
}

/** Resolve the deployment de-rank default from the environment. `WQM_SEARCH_DERANK`
 *  is a comma-separated list of path SUBSTRINGS (e.g. `old_project/,/generated/`) —
 *  intentionally the SAME format as the graph `WQM_GRAPH_CENTRALITY_EXCLUDE` knob so
 *  one value covers both. `WQM_SEARCH_DERANK_PENALTY` overrides the score multiplier
 *  ([0,1); default {@link DEFAULT_DERANK_PENALTY}). Unlike a hard `pathExclude`, this
 *  only reorders — matched hits sink but stay findable. Semantic/hybrid search only
 *  (exact/grep are literal, not ranked). Read per-call; change ⇒ recreate the mcp
 *  container (docker compose up -d --force-recreate mcp), no reembed. */
export function resolveDerankConfig(env: NodeJS.ProcessEnv = process.env): DerankConfig {
  const raw = env['WQM_SEARCH_DERANK'];
  const substrings = raw
    ? raw
        .split(',')
        .map((s) => s.trim())
        .filter((s) => s.length > 0)
    : [];
  let penalty = DEFAULT_DERANK_PENALTY;
  const penaltyRaw = env['WQM_SEARCH_DERANK_PENALTY'];
  if (penaltyRaw !== undefined && penaltyRaw.trim() !== '') {
    const parsed = Number(penaltyRaw);
    // Only accept a genuine down-weight in [0,1). >=1 (or garbage) keeps the
    // default rather than silently disabling or inverting the de-rank.
    if (Number.isFinite(parsed) && parsed >= 0 && parsed < 1) penalty = parsed;
  }
  return { substrings, penalty };
}

export interface ParentContext {
  parent_unit_id: string;
  unit_type: string;
  unit_text: string;
  locator?: Record<string, unknown>;
}

export interface GraphContextNode {
  symbol: string;
  file_path: string;
  line?: number;
}

export interface GraphContext {
  symbol: string;
  file_path: string;
  callers: GraphContextNode[];
  callees: GraphContextNode[];
}

export interface SearchResult {
  id: string;
  /** Pre-rerank similarity: raw cosine for the dense/semantic lane (same scale
   *  as `scoreThreshold`), or the RRF-fused score for hybrid. Stays comparable
   *  across queries. NOT overwritten by the reranker. */
  score: number;
  /** Present only when the cross-encoder reranker ran: the per-query, min-max
   *  blended rank in [0,1] that determined this result's order (1 = top of the
   *  pool, the pool minimum is 0). Use this to understand ordering; use `score`
   *  for absolute similarity. Absent when rerank is off. */
  rerankScore?: number;
  collection: string;
  /** Convenience `relative_path:line` (or bare path when no line) locator,
   *  lifted out of `metadata` so an agent reads the hit's location like a grep
   *  line without digging into the metadata bag. Absent when the hit has no
   *  path. Item 4 of the agent-ergonomics work. */
  location?: string;
  /** Present (always `true`) when the daemon's ingest classifier marked this
   *  projects-collection chunk as coming from a TEST file (payload `tags`
   *  contains "test"). Lifted to the top level so an implementation-seeking
   *  agent can skip test hits without digging into metadata. Absent means
   *  "not a test, or unknown" — never false. */
  is_test?: boolean;
  content: string;
  title?: string;
  metadata: Record<string, unknown>;
  parent_context?: ParentContext;
  graph_context?: GraphContext;
}

/**
 * Per-project indexing-progress block attached to project-scoped search
 * responses while the daemon's queue is still draining.
 *
 * `pending` + `in_progress` + `failed` come from `unified_queue`; `done`
 * is the durable count from `tracked_files`. `percent` is `done / total *
 * 100`, capped at 100.0. We only attach this when `(pending + in_progress)
 * > 0` so a fully indexed project doesn't pay the noise cost.
 */
export interface IndexingProgress {
  pending: number;
  in_progress: number;
  failed: number;
  done: number;
  total: number;
  percent: number;
  /** Estimated seconds until the queue drains for this tenant.
   *  Absent when the daemon doesn't have enough recent activity to
   *  estimate honestly (cold-start) or when the rate is zero with
   *  pending > 0. UIs should render "warming up" in those cases. */
  eta_seconds?: number;
}

export interface SearchResponse {
  results: SearchResult[];
  total: number;
  query: string;
  mode: SearchMode;
  scope: SearchScope;
  collections_searched: string[];
  status?: 'ok' | 'uncertain';
  status_reason?: string;
  /** One-line in-band teaching hint, attached only when the results contain
   *  code symbols — points the agent at the `graph` tool for callers/impact.
   *  Subagents never receive the server's MCP `instructions`, so a hint carried
   *  in the result body is the only channel that reliably teaches the next
   *  tool. Absent otherwise. Item 3 of the agent-ergonomics work. */
  hint?: string;
  /** Attached only when `scope === 'project'` and the daemon queue
   *  still has work for the current tenant. Absent otherwise. */
  indexing?: IndexingProgress;
  /** Attached only when the caller's cwd is inside a linked worktree and there
   *  are results: result `file_path`s are MAIN-anchored, so this tells the caller
   *  how to Read the worktree's own copy. Absent otherwise. */
  worktree?: WorktreeReadNote;
  /** Attached only when the global response byte budget dropped trailing hits:
   *  `dropped` is how many were cut (the kept set always has >=1). The agent can
   *  narrow the query, raise `maxResponseBytes`, or use `summary` for the rest. */
  budget_truncated?: { dropped: number };
  /** The English query that was searched as a SECOND leg alongside the original,
   *  present only when query translation fired (non-English query + the feature
   *  enabled + a usable translation). Absent on every other path, including when
   *  translation was attempted and rejected. Surfaced so a caller can see that
   *  its results are a fusion of two queries — and which one. */
  translated_query?: string;
  /** Set when more code candidates exist beyond the returned page — pass it back
   *  as `offset` to fetch the next page. Absent on the last page. */
  next_offset?: number;
  /** Present only for `responseFormat: "packed"`: ONE ranked, deduplicated
   *  context bundle assembled under the response byte budget. `text` holds
   *  the bundle (per-hit header + capped body per section); `included` is how
   *  many hits made it in; `dropped` is how many page hits were left out
   *  (budget, duplicate body, or empty body) — their metadata still appears
   *  in `results`, so the agent can retrieve() them individually. */
  packed_bundle?: { text: string; included: number; dropped: number };
  /** Count of hits collapsed because their body was byte-identical to a
   *  higher-ranked hit from a DIFFERENT file (vendored/copied code). Same-file
   *  chunks are already collapsed upstream by the per-file dedup. Counted over
   *  the whole candidate pool (not just this page) and therefore reported on
   *  the FIRST page only. Absent when 0. */
  duplicates_collapsed?: number;
}

/**
 * Token-economy metrics emitted by `shapeHitPayloads`.
 *
 * Spec: docs/specs/20-token-economy-instrumentation.md §3.1
 *
 * `bytes_in_shaped` and `bytes_out_shaped` cover only the fields that the
 * shaping pass can see and rewrite — `result.content` and
 * `parent_context.unit_text`. The eventual full `bytes_in` recorded in
 * `search_events` is built on top of these by adding a per-hit file-size
 * probe (out of scope for this initial wiring).
 */
export interface ShapingMetrics {
  /** Sum of bytes in `result.content` + `parent_context.unit_text` BEFORE shaping. */
  bytesInShaped: number;
  /** Sum of bytes in `result.content` + `parent_context.unit_text` AFTER shaping. */
  bytesOutShaped: number;
  /** Number of hits whose body was truncated (0 in summary mode). */
  hitsTruncated: number;
  /** Which shaping mode produced the response. */
  mode: 'truncate' | 'summary' | 'none' | 'packed';
}

export interface SearchToolConfig {
  qdrantUrl: string;
  qdrantApiKey?: string;
  qdrantTimeout?: number;
  /** Enable tag-based query expansion for BM25 sparse search (default: true) */
  enableTagExpansion?: boolean;
  /** Weight multiplier for expanded keywords (default: 0.5) */
  expansionWeight?: number;
  /** Maximum number of expanded keywords to add (default: 10) */
  maxExpandedKeywords?: number;
}

export interface FilterParams {
  collection: string;
  scope: SearchScope;
  projectId: string | undefined;
  branch: string | undefined;
  fileType: string | undefined;
  libraryName: string | undefined;
  tag: string | undefined;
  tags: string[] | undefined;
  pathGlob: string | undefined;
  /** Filter by component_id in Qdrant payload (prefix matching) */
  component: string | undefined;
  /** Task 15: base_point values for instance-aware filtering */
  basePoints: string[] | undefined;
  /** Base branch to include for files unchanged on a feature branch. */
  fallbackBranch: string | undefined;
  /** Exclude test-classified chunks at the vector store (projects collection,
   *  semantic/hybrid path). See buildMustNotConditions. */
  excludeTests?: boolean | undefined;
}

export interface SearchCollectionParams {
  collection: string;
  mode: SearchMode;
  denseEmbedding: number[] | undefined;
  sparseVector: Record<number, number> | undefined;
  filter: Record<string, unknown> | null;
  limit: number;
  scoreThreshold: number;
}
