/**
 * Per-hit payload shaping for the `search` tool.
 *
 * Search hits can carry large chunk bodies. Without a cap, a 10-hit
 * response easily exceeds an MCP client's per-tool-result token budget
 * and triggers disk offload at the client side, which breaks the agent's
 * reasoning flow. This module trims hit bodies before serialization so
 * callers don't have to think about budgets at all.
 *
 * Modes:
 *  - default: truncate each hit's `content` (and `parent_context.unit_text`)
 *    at {@link DEFAULT_MAX_BYTES_PER_HIT} chars; append a marker that
 *    points the agent at retrieve() for the full chunk.
 *  - `summary: true`: drop text bodies entirely; keep only id/score/
 *    collection/title and structural metadata. Intended for "which doc
 *    do I want?" discovery before a follow-up retrieve() call.
 *  - `responseFormat: "packed"`: assemble ONE ranked, deduplicated context
 *    bundle under the response byte budget (`packed_bundle.text`) — per-hit
 *    header (location · symbol) + capped body — with metadata-only entries
 *    in `results` for the full page. One coherent blob for the agent to
 *    read instead of N independently-truncated hits.
 *  - `responseFormat: "detailed"`: disable the per-hit cap (full bodies).
 *
 * The function also emits a {@link ShapingMetrics} sidecar so callers can
 * record token-economy stats per spec
 * `docs/specs/20-token-economy-instrumentation.md`.
 */

import { FIELD_CONTENT } from '../common/native-bridge.js';
import { RANKING_AID_KEYS } from '../common/payload-noise.js';
import { applyByteBudget } from './response-budget.js';
import type {
  ParentContext,
  SearchOptions,
  SearchResponse,
  SearchResult,
  ShapingMetrics,
} from './search-types.js';
import { DEFAULT_MAX_BYTES_PER_HIT, DEFAULT_MAX_RESPONSE_BYTES } from './search-types.js';

/** Minimum trimmed-body length for cross-hit identical-content collapse.
 *  Below this, identical bodies are more likely legitimately-common short
 *  snippets (closing braces, one-line accessors) than vendored copies, and
 *  collapsing them would hide distinct results for negligible savings. */
export const DUPLICATE_BODY_MIN_CHARS = 80;

/**
 * Collapse hits whose trimmed body is byte-identical to a higher-ranked hit.
 *
 * The per-file dedup upstream (`dedupeByFile`) already guarantees one hit per
 * file — what remains is the SAME content appearing in DIFFERENT files
 * (vendored/copied/generated code), which ships the identical snippet N times.
 * Input must be rank-ordered; the first occurrence (best-ranked) is kept.
 */
export function dedupeIdenticalBodies(results: readonly SearchResult[]): {
  kept: SearchResult[];
  dropped: number;
} {
  const seen = new Set<string>();
  const kept: SearchResult[] = [];
  let dropped = 0;
  for (const r of results) {
    const body = (r.content ?? '').trim();
    if (body.length >= DUPLICATE_BODY_MIN_CHARS) {
      if (seen.has(body)) {
        dropped += 1;
        continue;
      }
      seen.add(body);
    }
    kept.push(r);
  }
  return { kept, dropped };
}

/** Metadata payload fields known to carry chunk text. Stripped in
 *  summary mode AND deduplicated against `result.content` in truncate
 *  mode (the daemon's payload already duplicates content into both
 *  `result.content` and `result.metadata[FIELD_CONTENT]`). */
const TEXT_BODY_KEYS: readonly string[] = [
  'content',
  'text',
  'chunk_text',
  'unit_text',
  'snippet',
  'body',
];

function metadataNumber(value: unknown): number | undefined {
  if (typeof value === 'number' && Number.isFinite(value)) return value;
  if (typeof value === 'string' && value.trim() !== '') {
    const parsed = Number(value);
    if (Number.isFinite(parsed)) return parsed;
  }
  return undefined;
}

function metadataString(metadata: Record<string, unknown>, key: string): string | undefined {
  const v = metadata[key];
  return typeof v === 'string' && v.trim() !== '' ? v : undefined;
}

/** Build a grep-like `path:line` (or bare `path`) locator from a hit's
 *  metadata. Prefers the repo-relative path (clickable, what an agent wants)
 *  and the most specific line available (exact-search `line_number`, else the
 *  tree-sitter chunk's `chunk_start_line`). Returns undefined when the hit
 *  carries no path at all (e.g. some library/scratchpad entries). Item 4. */
function deriveLocation(metadata: Record<string, unknown>): string | undefined {
  const path = metadataString(metadata, 'relative_path') ?? metadataString(metadata, 'file_path');
  if (path === undefined) return undefined;
  const line =
    metadataNumber(metadata['line_number']) ?? metadataNumber(metadata['chunk_start_line']);
  return line !== undefined ? `${path}:${line}` : path;
}

/** One-line hint that teaches the `graph` tool in-band. Emitted only when at
 *  least one hit is a named code symbol, so non-code searches pay no token
 *  cost. The subagent channel: these agents never get the server's MCP
 *  `instructions`, so the result body is the only place that can teach them
 *  the next tool. Item 3. */
const GRAPH_HINT =
  'Tip: for callers, usages, or change-impact of a symbol in these results, ' +
  'call the `graph` tool (e.g. graph(action="impact", symbol="<name>")) instead of re-searching.';

function hasSymbolHit(results: readonly SearchResult[]): boolean {
  return results.some((r) => metadataString(r.metadata, 'chunk_symbol_name') !== undefined);
}

function buildRetrieveReference(r: SearchResult): string {
  const filePath = r.metadata['file_path'] as string | undefined;
  const lineNumber = metadataNumber(r.metadata['line_number']);
  if (filePath !== undefined && filePath !== '' && lineNumber !== undefined) {
    return `retrieve(filePath=${JSON.stringify(filePath)}, lineNumber=${lineNumber}, collection=${JSON.stringify(r.collection)})`;
  }
  return `retrieve(documentId=${JSON.stringify(r.id)}, collection=${JSON.stringify(r.collection)})`;
}

function truncateText(text: string, cap: number, reference: string): string {
  if (text.length <= cap) return text;
  const marker = ` ... [truncated at ${cap} chars; full chunk via ${reference}]`;
  const keep = Math.max(0, cap - marker.length);
  return text.slice(0, keep) + marker;
}

/** Strip the metadata an agent never reads from a default (truncate-mode) hit:
 *  duplicated text bodies (shipping the chunk twice) AND the daemon's
 *  ranking-aid fields ({@link RANKING_AID_KEYS} — keywords/baskets/tags, ~1.5–2k
 *  tokens/hit). Summary mode drops the same noise via its allowlist; this keeps
 *  the default mode from leaking it back in. */
function stripBulkMetadata(metadata: Record<string, unknown>): Record<string, unknown> {
  const out: Record<string, unknown> = { ...metadata };
  for (const key of TEXT_BODY_KEYS) {
    if (key in out) delete out[key];
  }
  for (const key of RANKING_AID_KEYS) {
    if (key in out) delete out[key];
  }
  if (FIELD_CONTENT && FIELD_CONTENT in out) delete out[FIELD_CONTENT];
  return out;
}

/** Metadata fields worth keeping in `summary` mode — just enough to decide
 *  "which result do I want?" before a follow-up retrieve(). Everything else
 *  (ranking aids like `keyword_baskets`/`keywords`, sparse-vector debris, and
 *  other large payload fields) is dropped: summary exists to economize tokens,
 *  and the verbose metadata was ~1–2k tokens of noise per hit.
 *
 *  Key names follow the daemon's Qdrant payload schema
 *  (src/rust/common/src/schema/qdrant/projects.rs): tree-sitter chunk
 *  metadata is prefixed `chunk_` — `chunk_symbol_name`, `chunk_start_line`,
 *  `chunk_end_line`, `chunk_chunk_type`. Unprefixed spellings never existed
 *  in the payload and silently matched nothing. */
const SUMMARY_METADATA_KEYS: readonly string[] = [
  'file_path',
  'relative_path',
  'language',
  'branch',
  'document_id',
  'chunk_symbol_name',
  'chunk_start_line',
  'chunk_end_line',
  'chunk_chunk_type',
];

function pickSummaryMetadata(metadata: Record<string, unknown>): Record<string, unknown> {
  const out: Record<string, unknown> = {};
  for (const key of SUMMARY_METADATA_KEYS) {
    const v = metadata[key];
    if (v !== undefined && v !== null && v !== '') out[key] = v;
  }
  return out;
}

function shapeAsSummary(r: SearchResult): SearchResult {
  const out: SearchResult = {
    id: r.id,
    score: r.score,
    collection: r.collection,
    content: '',
    // Allowlist, not just text-stripping: summary keeps only discovery-relevant
    // structural fields and drops the rest (keyword_baskets/keywords/etc.).
    metadata: pickSummaryMetadata(r.metadata),
  };
  if (r.title !== undefined && r.title !== '') out.title = r.title;
  // Carry the rerank ordering signal through the summary allowlist (the
  // truncate path keeps it via spread; summary rebuilds, so add it explicitly).
  if (r.rerankScore !== undefined) out.rerankScore = r.rerankScore;
  const location = deriveLocation(r.metadata);
  if (location !== undefined) out.location = location;
  return out;
}

function shapeParentContext(parent: ParentContext, cap: number, reference: string): ParentContext {
  return {
    ...parent,
    unit_text: truncateText(parent.unit_text ?? '', cap, reference),
  };
}

function shapeAsTruncated(r: SearchResult, cap: number): SearchResult {
  const retrieveReference = buildRetrieveReference(r);
  const out: SearchResult = {
    ...r,
    content: truncateText(r.content ?? '', cap, retrieveReference),
    // Drop content duplication AND the daemon's ranking-aid fields from
    // metadata: without this we'd ship the body twice for any sub-cap hit,
    // and carry ~1.5–2k tokens of keywords/baskets noise per hit that the
    // agent never consumes.
    metadata: stripBulkMetadata(r.metadata),
  };
  if (r.parent_context) {
    out.parent_context = shapeParentContext(r.parent_context, cap, retrieveReference);
  }
  const location = deriveLocation(r.metadata);
  if (location !== undefined) out.location = location;
  return out;
}

function hitShapedBytes(r: SearchResult): number {
  return (r.content?.length ?? 0) + (r.parent_context?.unit_text?.length ?? 0);
}

/** Drop trailing hits once the cumulative shaped body bytes exceed `budget`
 *  (always keeping at least one hit). Bounds the TOTAL response size — the
 *  per-hit cap alone can't, since N hits at the cap still sum to N×cap. A
 *  non-positive budget disables the trim. The caller surfaces `dropped` via
 *  `budget_truncated` so the agent can narrow the query or ask for summary.
 *  Delegates to the shared {@link applyByteBudget} so search and grep enforce
 *  budgets with identical semantics. */
function applyResponseBudget(
  results: SearchResult[],
  budget: number
): { kept: SearchResult[]; dropped: number } {
  return applyByteBudget(results, hitShapedBytes, budget);
}

function emptyMetrics(mode: ShapingMetrics['mode']): ShapingMetrics {
  return { bytesInShaped: 0, bytesOutShaped: 0, hitsTruncated: 0, mode };
}

/** Header line for one packed-bundle section: grep-like locator, chunk end
 *  line when known, and the symbol name — the minimal framing an agent needs
 *  to attribute a body without digging through a metadata bag. */
function packedSectionHeader(r: SearchResult): string {
  const loc = deriveLocation(r.metadata) ?? `${r.collection}:${r.id}`;
  const endLine = metadataNumber(r.metadata['chunk_end_line']);
  // deriveLocation ends with the start line when one is known; extend it to
  // a range so the agent sees the section's true extent.
  const range = endLine !== undefined && /:\d+$/.test(loc) ? `-${endLine}` : '';
  const symbol = metadataString(r.metadata, 'chunk_symbol_name');
  return `── ${loc}${range}${symbol !== undefined ? ` · ${symbol}` : ''} ──`;
}

/**
 * `responseFormat: "packed"` — assemble ONE ranked, deduplicated context
 * bundle under the response byte budget instead of N independently-truncated
 * hits.
 *
 * Fill is rank-strict and greedy: sections are appended in rank order until
 * the next section would exceed the budget (at least one is always kept, and
 * skipped-not-counted sections are empty bodies and byte-identical duplicate
 * bodies — the cross-file copy case). `results` carries metadata-only entries
 * for the FULL page (same allowlist as summary mode), so everything the
 * bundle had no room for remains discoverable and retrievable.
 */
function shapeAsPackedBundle(
  response: SearchResponse,
  options: SearchOptions,
  budget: number,
  hint: string | undefined
): { response: SearchResponse; metrics: ShapingMetrics } {
  const metrics = emptyMetrics('packed');
  for (const r of response.results) metrics.bytesInShaped += hitShapedBytes(r);

  const cap = options.maxBytesPerHit ?? DEFAULT_MAX_BYTES_PER_HIT;
  const sections: string[] = [];
  const seenBodies = new Set<string>();
  let used = 0;
  for (const r of response.results) {
    const body = (r.content ?? '').trim();
    if (body === '') continue;
    if (body.length >= DUPLICATE_BODY_MIN_CHARS && seenBodies.has(body)) continue;
    const packedBody = cap > 0 ? truncateText(body, cap, buildRetrieveReference(r)) : body;
    const section = `${packedSectionHeader(r)}\n${packedBody}`;
    if (sections.length > 0 && budget > 0 && used + section.length > budget) break;
    if (packedBody.length < body.length) metrics.hitsTruncated += 1;
    sections.push(section);
    seenBodies.add(body);
    used += section.length;
  }

  const bundleText = sections.join('\n\n');
  metrics.bytesOutShaped = bundleText.length;
  const out: SearchResponse = {
    ...response,
    results: response.results.map((r) => shapeAsSummary(r)),
    packed_bundle: {
      text: bundleText,
      included: sections.length,
      dropped: response.results.length - sections.length,
    },
  };
  if (hint !== undefined && out.hint === undefined) out.hint = hint;
  return { response: out, metrics };
}

/**
 * Apply per-hit payload shaping to a search response based on the
 * caller's options. Returns a new SearchResponse (input is not mutated)
 * along with shaping metrics for instrumentation.
 */
export function shapeHitPayloads(
  response: SearchResponse,
  options: SearchOptions
): { response: SearchResponse; metrics: ShapingMetrics } {
  // Computed from the ORIGINAL hits so it is independent of which shaping
  // branch runs (every branch preserves chunk_symbol_name). Folded into the
  // response by `finalize` below.
  const hint = hasSymbolHit(response.results) ? GRAPH_HINT : undefined;
  const budget = options.maxResponseBytes ?? DEFAULT_MAX_RESPONSE_BYTES;
  const finalize = (
    base: SearchResponse,
    results: SearchResult[],
    metrics: ShapingMetrics
  ): SearchResponse => {
    // Apply the global byte budget in finalize so ALL three shaping branches
    // (summary / none / truncate) honor it uniformly.
    const { kept, dropped } = applyResponseBudget(results, budget);
    const out: SearchResponse = { ...base, results: kept };
    if (hint !== undefined && out.hint === undefined) out.hint = hint;
    if (dropped > 0) {
      out.budget_truncated = { dropped };
      // Keep `total` consistent with the returned `results` after the trim
      // (it was set to the pre-budget page length upstream).
      out.total = kept.length;
      // The per-hit map already counted dropped hits into bytesOutShaped;
      // recompute from the kept hits so token-economy telemetry stays honest.
      metrics.bytesOutShaped = kept.reduce((sum, r) => sum + hitShapedBytes(r), 0);
    }
    return out;
  };

  if (options.summary === true) {
    const metrics = emptyMetrics('summary');
    const results = response.results.map((r) => {
      metrics.bytesInShaped += hitShapedBytes(r);
      return shapeAsSummary(r);
    });
    // bytesOutShaped stays 0 — summary mode drops bodies entirely.
    return { response: finalize(response, results, metrics), metrics };
  }
  if (options.responseFormat === 'packed') {
    return shapeAsPackedBundle(response, options, budget, hint);
  }
  // responseFormat is the coarse knob: `detailed` disables the per-hit cap
  // (full bodies); `concise`/unset uses the default. An explicit maxBytesPerHit
  // still wins over both.
  const cap =
    options.maxBytesPerHit ?? (options.responseFormat === 'detailed' ? 0 : DEFAULT_MAX_BYTES_PER_HIT);
  if (cap <= 0) {
    const metrics = emptyMetrics('none');
    // Cap disabled: bodies pass through untouched, but still lift `location`
    // out of metadata so the grep-like locator is uniform across all modes.
    const results = response.results.map((r) => {
      const bytes = hitShapedBytes(r);
      metrics.bytesInShaped += bytes;
      metrics.bytesOutShaped += bytes;
      const location = deriveLocation(r.metadata);
      return location !== undefined ? { ...r, location } : r;
    });
    return { response: finalize(response, results, metrics), metrics };
  }
  const metrics = emptyMetrics('truncate');
  const results = response.results.map((r) => {
    const beforeContent = r.content?.length ?? 0;
    const beforeParent = r.parent_context?.unit_text?.length ?? 0;
    metrics.bytesInShaped += beforeContent + beforeParent;
    if (beforeContent > cap || beforeParent > cap) {
      metrics.hitsTruncated += 1;
    }
    const shaped = shapeAsTruncated(r, cap);
    metrics.bytesOutShaped += hitShapedBytes(shaped);
    return shaped;
  });
  return { response: finalize(response, results, metrics), metrics };
}
