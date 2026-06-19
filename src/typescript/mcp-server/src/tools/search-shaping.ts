/**
 * Per-hit payload shaping for the `search` tool.
 *
 * Search hits can carry large chunk bodies. Without a cap, a 10-hit
 * response easily exceeds an MCP client's per-tool-result token budget
 * and triggers disk offload at the client side, which breaks the agent's
 * reasoning flow. This module trims hit bodies before serialization so
 * callers don't have to think about budgets at all.
 *
 * Two modes:
 *  - default: truncate each hit's `content` (and `parent_context.unit_text`)
 *    at {@link DEFAULT_MAX_BYTES_PER_HIT} chars; append a marker that
 *    points the agent at retrieve() for the full chunk.
 *  - `summary: true`: drop text bodies entirely; keep only id/score/
 *    collection/title and structural metadata. Intended for "which doc
 *    do I want?" discovery before a follow-up retrieve() call.
 *
 * The function also emits a {@link ShapingMetrics} sidecar so callers can
 * record token-economy stats per spec
 * `docs/specs/20-token-economy-instrumentation.md`.
 */

import { FIELD_CONTENT } from '../common/native-bridge.js';
import { RANKING_AID_KEYS } from '../common/payload-noise.js';
import type {
  ParentContext,
  SearchOptions,
  SearchResponse,
  SearchResult,
  ShapingMetrics,
} from './search-types.js';
import { DEFAULT_MAX_BYTES_PER_HIT } from './search-types.js';

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

function emptyMetrics(mode: ShapingMetrics['mode']): ShapingMetrics {
  return { bytesInShaped: 0, bytesOutShaped: 0, hitsTruncated: 0, mode };
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
  const finalize = (base: SearchResponse, results: SearchResult[]): SearchResponse => {
    const out: SearchResponse = { ...base, results };
    if (hint !== undefined && out.hint === undefined) out.hint = hint;
    return out;
  };

  if (options.summary === true) {
    const metrics = emptyMetrics('summary');
    const results = response.results.map((r) => {
      metrics.bytesInShaped += hitShapedBytes(r);
      return shapeAsSummary(r);
    });
    // bytesOutShaped stays 0 — summary mode drops bodies entirely.
    return { response: finalize(response, results), metrics };
  }
  const cap = options.maxBytesPerHit ?? DEFAULT_MAX_BYTES_PER_HIT;
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
    return { response: finalize(response, results), metrics };
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
  return { response: finalize(response, results), metrics };
}
